//! Opt-in local codec measurements; excluded from normal test runs.

use super::super::*;
use super::*;
use std::time::Instant;

#[test]
#[ignore = "local APFS install comparison; run explicitly with AUBE_CODEC_BENCH_INPUTS"]
fn measure_actual_apfs_import() {
    use std::os::unix::fs::MetadataExt;

    let path = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input path");
    let path = std::env::split_paths(&path)
        .next()
        .expect("first input path");
    let raw = std::fs::read(path).expect("read input");
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let target = temp_dir.path().join("addon.node");
    let mut before = unsafe { std::mem::zeroed::<libc::rusage>() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut before) },
        0
    );
    let start = Instant::now();
    let outcome =
        crate::compression::compress_bytes(&target, &raw, &crate::compression::Gate::any())
            .expect("compress bytes");
    let elapsed = start.elapsed().as_secs_f64();
    assert_eq!(std::fs::read(&target).expect("read compressed file"), raw);
    let metadata = std::fs::metadata(&target).expect("compressed file metadata");
    let mut after = unsafe { std::mem::zeroed::<libc::rusage>() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut after) }, 0);
    println!(
        "raw_mb={:.1} allocated_mb={:.1} logical_mb={:.1} elapsed_s={elapsed:.4} peak_rss_mb={:.1} outcome={outcome:?}",
        raw.len() as f64 / 1_000_000.0,
        (metadata.blocks() * 512) as f64 / 1_000_000.0,
        metadata.len() as f64 / 1_000_000.0,
        after.ru_maxrss as f64 / 1_000_000.0
    );
}

#[test]
#[ignore = "local peak-memory probe; run explicitly with AUBE_CODEC_BENCH_INPUTS"]
fn measure_resource_fork_peak_memory() {
    let path = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input path");
    let path = std::env::split_paths(&path)
        .next()
        .expect("first input path");
    let raw = std::fs::read(path).expect("read input");
    let mode = std::env::var("AUBE_CODEC_BENCH_MODE").unwrap_or_else(|_| "memory".into());
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let target = temp_dir.path().join("probe.node");
    std::fs::File::create(&target).expect("create target");
    let mut before = unsafe { std::mem::zeroed::<libc::rusage>() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut before) },
        0
    );
    let start = Instant::now();
    let fork_len = if mode == "stream" {
        assert!(
            build_streaming_resource_fork(&target, &raw)
                .expect("stream compress")
                .is_some()
        );
        std::fs::metadata(target.join("..namedfork").join("rsrc"))
            .expect("resource fork")
            .len() as usize
    } else {
        build_resource_fork_with_codec(&raw, Codec::Lzfse)
            .expect("compress")
            .expect("fixture should compress")
            .len()
    };
    let elapsed = start.elapsed().as_secs_f64();
    let mut after = unsafe { std::mem::zeroed::<libc::rusage>() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut after) }, 0);
    println!(
        "mode={mode} raw={} fork={} elapsed_s={elapsed:.4} before_peak_mb={:.1} after_peak_mb={:.1}",
        raw.len(),
        fork_len,
        before.ru_maxrss as f64 / 1_000_000.0,
        after.ru_maxrss as f64 / 1_000_000.0
    );
}

#[test]
#[ignore = "local codec comparison; run explicitly with AUBE_CODEC_BENCH_INPUTS"]
fn compare_local_addon_codecs() {
    let inputs = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input paths");
    for path in std::env::split_paths(&inputs) {
        let raw = std::fs::read(&path).expect("read input");
        println!("{} raw={}", path.display(), raw.len());
        let codecs = [Codec::Lzvn, Codec::Lzfse];
        let mut encoded: [Option<Vec<u8>>; 2] = [None, None];
        for codec in codecs {
            for _ in 0..2 {
                let _ = build_resource_fork_with_codec(&raw, codec)
                    .expect("warm encode")
                    .expect("warm fixture should compress");
            }
        }
        let mut encode_times: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        for round in 0..9 {
            let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
            for index in order {
                let start = Instant::now();
                let fork = build_resource_fork_with_codec(&raw, codecs[index])
                    .expect("encode")
                    .expect("fixture should compress");
                encode_times[index].push(start.elapsed().as_secs_f64());
                if encoded[index].is_none() {
                    encoded[index] = Some(fork);
                }
            }
        }
        let mut decode_times: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        for round in 0..9 {
            let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
            for index in order {
                let codec = codecs[index];
                let fork = encoded[index].as_ref().unwrap();
                let num_blocks = raw.len().div_ceil(BLOCK);
                let scratch_len =
                    unsafe { compression_decode_scratch_buffer_size(codec.algorithm()) };
                let mut scratch = vec![0; scratch_len];
                let start = Instant::now();
                let mut offset = 0;
                for block_index in 0..num_blocks {
                    let table_start = block_index * 4;
                    let encoded_start =
                        u32::from_le_bytes(fork[table_start..table_start + 4].try_into().unwrap())
                            as usize;
                    let encoded_end = u32::from_le_bytes(
                        fork[table_start + 4..table_start + 8].try_into().unwrap(),
                    ) as usize;
                    let end = (offset + BLOCK).min(raw.len());
                    let mut decoded = vec![0; end - offset];
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
                    assert_eq!(decoded, raw[offset..end]);
                    offset = end;
                }
                decode_times[index].push(start.elapsed().as_secs_f64());
            }
        }
        for index in 0..2 {
            let mut encode = encode_times[index].clone();
            encode.sort_by(f64::total_cmp);
            let mut decode = decode_times[index].clone();
            decode.sort_by(f64::total_cmp);
            let bytes = encoded[index].as_ref().unwrap().len();
            println!(
                "codec={:?} encoded={bytes} encode_median_s={:.4} decode_median_s={:.4}",
                codecs[index], encode[4], decode[4]
            );
        }
    }
}

#[test]
#[ignore = "local native-load comparison; run explicitly with AUBE_CODEC_BENCH_INPUTS"]
fn compare_local_native_load_times() {
    use std::os::fd::AsRawFd;
    use std::process::Command;

    let input = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input path");
    let input = std::env::split_paths(&input)
        .next()
        .expect("first input path");
    let raw = std::fs::read(input).expect("read input");
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let codecs = [Codec::Lzvn, Codec::Lzfse];
    let mut paths = Vec::new();
    for codec in codecs {
        let path = temp_dir.path().join(format!("{codec:?}.node"));
        std::fs::File::create(&path).expect("create native addon file");
        let fork = build_resource_fork_with_codec(&raw, codec)
            .expect("compress fixture")
            .expect("fixture should compress");
        let cpath = cstring(&path).expect("native addon path");
        setxattr(&cpath, c"com.apple.ResourceFork", &fork).expect("write resource fork");
        setxattr(
            &cpath,
            c"com.apple.decmpfs",
            &decmpfs_header(codec, raw.len()),
        )
        .expect("write decmpfs header");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open addon for compressed flag");
        assert_eq!(
            unsafe { libc::fchflags(file.as_raw_fd(), UF_COMPRESSED) },
            0
        );
        paths.push(path);
    }
    let load = |index: usize| {
        let output = Command::new("node")
            .arg("-e")
            .arg("require(process.argv[1])")
            .arg(&paths[index])
            .output()
            .expect("launch node");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    for _ in 0..2 {
        load(0);
        load(1);
    }
    let mut timings: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    for round in 0..9 {
        let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
        for index in order {
            let start = Instant::now();
            load(index);
            timings[index].push(start.elapsed().as_secs_f64());
        }
    }
    for index in 0..2 {
        timings[index].sort_by(f64::total_cmp);
        println!(
            "codec={:?} node_process_with_addon_median_s={:.4}",
            codecs[index], timings[index][4]
        );
    }
}
