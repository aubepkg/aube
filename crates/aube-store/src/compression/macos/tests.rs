//! Resource-fork round trips and worker cancellation.

use super::*;

#[test]
fn parallel_encoding_failure_cancels_and_joins_workers() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let raw = vec![b'A'; BLOCK * 32];
        let result = build_resource_fork_with_workers(&raw, Codec::Lzfse, 4, |_, _, _| None);
        sender.send(result.unwrap().is_none()).unwrap();
    });
    assert!(
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("all scoped workers must stop after encoding fails")
    );
    thread.join().unwrap();
}

#[test]
fn in_memory_resource_fork_round_trips_for_both_codecs() {
    let raw: Vec<u8> = (0..BLOCK * 17 + 17)
        .map(|i| b'A' + (i / BLOCK) as u8)
        .collect();
    let num_blocks = raw.len().div_ceil(BLOCK);
    let table_len = resource_fork_table_len(num_blocks).unwrap();

    for codec in [Codec::Lzvn, Codec::Lzfse] {
        let fork = build_resource_fork_with_workers(&raw, codec, 4, compress_block_with_codec)
            .unwrap()
            .expect("repeated input should compress");
        assert_eq!(
            u32::from_le_bytes(fork[..4].try_into().unwrap()) as usize,
            table_len
        );
        assert_eq!(
            u32::from_le_bytes(fork[table_len - 4..table_len].try_into().unwrap()) as usize,
            fork.len()
        );

        let mut scratch =
            vec![0; unsafe { compression_decode_scratch_buffer_size(codec.algorithm()) }];
        let mut raw_offset = 0;
        for block_index in 0..num_blocks {
            let table_start = block_index * 4;
            let encoded_start =
                u32::from_le_bytes(fork[table_start..table_start + 4].try_into().unwrap()) as usize;
            let encoded_end =
                u32::from_le_bytes(fork[table_start + 4..table_start + 8].try_into().unwrap())
                    as usize;
            let raw_end = (raw_offset + BLOCK).min(raw.len());
            let mut decoded = vec![0; raw_end - raw_offset];
            let size = unsafe {
                compression_decode_buffer(
                    decoded.as_mut_ptr(),
                    decoded.len(),
                    fork[encoded_start..encoded_end].as_ptr(),
                    encoded_end - encoded_start,
                    scratch.as_mut_ptr(),
                    codec.algorithm(),
                )
            };
            assert_eq!(size, decoded.len());
            assert_eq!(decoded, raw[raw_offset..raw_end]);
            raw_offset = raw_end;
        }
    }
}
