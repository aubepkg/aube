//! Encode ordered APFS resource-fork blocks with bounded parallel workers.

use super::{Error, Path};

const BLOCK: usize = 0x1_0000; // 64 KiB
const COMPRESSION_LZVN: i32 = 0x900;
const COMPRESSION_LZFSE: i32 = 0x801;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Codec {
    Lzvn,
    Lzfse,
}

impl Codec {
    pub(super) const fn compression_type(self) -> u32 {
        match self {
            Self::Lzvn => 8,
            Self::Lzfse => 12,
        }
    }

    const fn algorithm(self) -> i32 {
        match self {
            Self::Lzvn => COMPRESSION_LZVN,
            Self::Lzfse => COMPRESSION_LZFSE,
        }
    }
}

#[link(name = "compression")]
unsafe extern "C" {
    fn compression_decode_buffer(
        dst_buffer: *mut u8,
        dst_size: usize,
        src_buffer: *const u8,
        src_size: usize,
        scratch_buffer: *mut u8,
        algorithm: i32,
    ) -> usize;
    fn compression_decode_scratch_buffer_size(algorithm: i32) -> usize;
    fn compression_encode_buffer(
        dst_buffer: *mut u8,
        dst_size: usize,
        src_buffer: *const u8,
        src_size: usize,
        scratch_buffer: *mut u8,
        algorithm: i32,
    ) -> usize;
    fn compression_encode_scratch_buffer_size(algorithm: i32) -> usize;
}

fn resource_fork_too_large() -> Error {
    Error::Io {
        context: "decmpfs resource fork exceeds u32 offsets",
        source: std::io::Error::from_raw_os_error(libc::EFBIG),
    }
}

/// Encode `src` into one kernel-decodable block. libcompression emits a valid
/// frame even for incompressible input (slightly larger than `src`), so every
/// block decodes the same way. `None` means the codec declined outright.
fn compress_block_with_codec(src: &[u8], scratch: &mut [u8], codec: Codec) -> Option<Vec<u8>> {
    // Headroom for the worst case (incompressible data expands a little).
    let mut dst = vec![0u8; src.len() + src.len() / 16 + 1024];
    let n = unsafe {
        compression_encode_buffer(
            dst.as_mut_ptr(),
            dst.len(),
            src.as_ptr(),
            src.len(),
            scratch.as_mut_ptr(),
            codec.algorithm(),
        )
    };
    if n == 0 {
        return None;
    }
    dst.truncate(n);
    Some(dst)
}

/// Compute the offset-table size without overflowing addressable memory.
fn resource_fork_table_len(num_blocks: usize) -> Result<usize, Error> {
    num_blocks
        .checked_add(1)
        .and_then(|entries| entries.checked_mul(std::mem::size_of::<u32>()))
        .ok_or_else(resource_fork_too_large)
}

/// Build the com.apple.ResourceFork blob for `raw` in the LZVN/LZFSE decmpfs
/// layout (what `ditto` writes): `(numBlocks+1)` u32 LE offsets, then the blocks.
/// `offset[0]` = table size; `offset[i+1]` = end of block i; last = total size.
/// `Ok(None)` means this codec did not shrink the input.
pub(super) fn build_resource_fork_with_codec(
    raw: &[u8],
    codec: Codec,
) -> Result<Option<Vec<u8>>, Error> {
    let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
        1
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(raw.len().div_ceil(BLOCK).max(1))
    };
    build_resource_fork_with_workers(raw, codec, workers, compress_block_with_codec)
}

/// Encode blocks in order with bounded worker queues; cancel and join workers on failure.
fn build_resource_fork_with_workers(
    raw: &[u8],
    codec: Codec,
    workers: usize,
    encode: impl Fn(&[u8], &mut [u8], Codec) -> Option<Vec<u8>> + Sync,
) -> Result<Option<Vec<u8>>, Error> {
    if raw.is_empty() {
        return Ok(None);
    }
    let num_blocks = raw.len().div_ceil(BLOCK).max(1);
    let table_len = resource_fork_table_len(num_blocks)?;
    if table_len >= raw.len() || table_len > u32::MAX as usize {
        return Ok(None);
    }

    // Append blocks to their final buffer as workers finish them in order. The
    // old path retained every encoded block, then allocated and copied a second
    // full resource-fork buffer. Single-slot channels bound queued output to one
    // block per worker while preserving parallel compression.
    let initial_capacity = table_len
        .checked_add(raw.len() / 2)
        .unwrap_or(raw.len())
        .min(raw.len());
    let mut out = Vec::with_capacity(initial_capacity);
    out.resize(table_len, 0);
    let mut offsets = Vec::with_capacity(num_blocks + 1);
    offsets.push(u32::try_from(table_len).map_err(|_| resource_fork_too_large())?);

    let scratch_len = unsafe { compression_encode_scratch_buffer_size(codec.algorithm()) };
    let append_block = |out: &mut Vec<u8>, offsets: &mut Vec<u32>, block: Vec<u8>| {
        let Some(next_offset) = out.len().checked_add(block.len()) else {
            return false;
        };
        if next_offset >= raw.len() || next_offset > u32::MAX as usize {
            return false;
        }
        out.extend_from_slice(&block);
        offsets.push(next_offset as u32);
        true
    };

    let won = if workers <= 1 || num_blocks < 8 {
        let mut scratch = vec![0u8; scratch_len];
        let mut won = true;
        for chunk in raw.chunks(BLOCK) {
            let Some(block) = encode(chunk, &mut scratch, codec) else {
                won = false;
                break;
            };
            if !append_block(&mut out, &mut offsets, block) {
                won = false;
                break;
            }
        }
        won
    } else {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| -> Result<bool, Error> {
            let mut receivers = Vec::with_capacity(workers);
            for worker in 0..workers {
                let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                receivers.push(receiver);
                let cancelled = &cancelled;
                let encode = &encode;
                scope.spawn(move || {
                    let mut scratch = vec![0u8; scratch_len];
                    let mut block_index = worker;
                    while block_index < num_blocks && !cancelled.load(Ordering::Relaxed) {
                        let start = block_index * BLOCK;
                        let end = start.saturating_add(BLOCK).min(raw.len());
                        let encoded = encode(&raw[start..end], &mut scratch, codec);
                        if sender.send(encoded).is_err() {
                            break;
                        }
                        block_index += workers;
                    }
                });
            }

            let result = (|| -> Result<bool, Error> {
                for block_index in 0..num_blocks {
                    let Some(block) = receivers[block_index % workers].recv().ok().flatten() else {
                        return Ok(false);
                    };
                    if !append_block(&mut out, &mut offsets, block) {
                        return Ok(false);
                    }
                }
                Ok(true)
            })();
            cancelled.store(true, Ordering::Relaxed);
            drop(receivers);
            result
        })?
    };

    if !won || offsets.len() != num_blocks + 1 {
        return Ok(None);
    }
    for (index, offset) in offsets.into_iter().enumerate() {
        let at = index * std::mem::size_of::<u32>();
        out[at..at + 4].copy_from_slice(&offset.to_le_bytes());
    }
    debug_assert!(out.len() < raw.len());
    debug_assert!(out.len() <= u32::MAX as usize);
    Ok(Some(out))
}

pub(super) struct InMemoryResourceFork {
    pub(super) codec: Codec,
    pub(super) bytes: Vec<u8>,
}

/// Prefer LZFSE for ordinary addons: paired local measurements show a smaller
/// resource fork with equal decode time and lower encode time. LZVN remains a
/// fallback for inputs that LZFSE cannot shrink.
pub(super) fn build_in_memory_resource_fork(
    raw: &[u8],
) -> Result<Option<InMemoryResourceFork>, Error> {
    for codec in [Codec::Lzfse, Codec::Lzvn] {
        if let Some(bytes) = build_resource_fork_with_codec(raw, codec)? {
            return Ok(Some(InMemoryResourceFork { codec, bytes }));
        }
    }
    Ok(None)
}

/// Stream one codec into the temp file's named resource fork. One single-slot
/// channel per worker keeps at most one encoded block per core waiting for the
/// ordered writer, so output memory is bounded while libcompression stays
/// parallel. `Ok(false)` means no gain, an unrepresentable u32 fork, or a codec
/// decline; the caller may retry the original bytes with another codec.
pub(super) fn write_streaming_resource_fork(
    path: &Path,
    raw: &[u8],
    codec: Codec,
) -> Result<bool, Error> {
    use std::io::{Seek, Write};
    use std::sync::atomic::{AtomicBool, Ordering};

    let num_blocks = raw.len().div_ceil(BLOCK).max(1);
    let table_len = resource_fork_table_len(num_blocks)?;
    if table_len >= raw.len() || table_len > u32::MAX as usize {
        return Ok(false);
    }

    let fork_path = path.join("..namedfork").join("rsrc");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(fork_path)
        .map_err(|source| Error::Io {
            context: "open resource fork",
            source,
        })?;
    file.set_len(table_len as u64).map_err(|source| Error::Io {
        context: "reserve resource-fork table",
        source,
    })?;
    file.seek(std::io::SeekFrom::Start(table_len as u64))
        .map_err(|source| Error::Io {
            context: "seek resource-fork payload",
            source,
        })?;
    // Coalesce codec blocks into larger writes. Besides syscall overhead, one
    // write per 64 KiB block makes APFS allocate several extra MiB of extents on
    // a multi-gigabyte fork even when its logical compressed bytes are identical.
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);

    let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
        1
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(num_blocks)
    };
    let scratch_len = unsafe { compression_encode_scratch_buffer_size(codec.algorithm()) };
    let cancelled = AtomicBool::new(false);
    let mut offsets = Vec::with_capacity(num_blocks + 1);
    offsets.push(u32::try_from(table_len).map_err(|_| resource_fork_too_large())?);
    let mut offset = table_len;

    let won = std::thread::scope(|scope| -> Result<bool, Error> {
        let mut receivers = Vec::with_capacity(workers);
        for worker in 0..workers {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            receivers.push(receiver);
            let cancelled = &cancelled;
            scope.spawn(move || {
                let mut scratch = vec![0u8; scratch_len];
                let mut block_index = worker;
                while block_index < num_blocks && !cancelled.load(Ordering::Relaxed) {
                    let start = block_index * BLOCK;
                    let end = start.saturating_add(BLOCK).min(raw.len());
                    let encoded = compress_block_with_codec(&raw[start..end], &mut scratch, codec);
                    if sender.send(encoded).is_err() {
                        break;
                    }
                    block_index += workers;
                }
            });
        }

        let result = (|| -> Result<bool, Error> {
            for block_index in 0..num_blocks {
                let Some(block) = receivers[block_index % workers].recv().ok().flatten() else {
                    return Ok(false);
                };
                let Some(next_offset) = offset.checked_add(block.len()) else {
                    return Ok(false);
                };
                // The fork can only grow from here. Stop early rather than writing a
                // multi-gigabyte losing candidate before trying the fallback codec.
                if next_offset >= raw.len() || next_offset > u32::MAX as usize {
                    return Ok(false);
                }
                writer.write_all(&block).map_err(|source| Error::Io {
                    context: "write resource-fork block",
                    source,
                })?;
                offset = next_offset;
                offsets.push(u32::try_from(offset).map_err(|_| resource_fork_too_large())?);
            }
            Ok(true)
        })();
        cancelled.store(true, Ordering::Relaxed);
        drop(receivers);
        result
    })?;

    if !won {
        return Ok(false);
    }
    debug_assert_eq!(offsets.len(), num_blocks + 1);
    let mut table = Vec::with_capacity(table_len);
    for offset in offsets {
        table.extend_from_slice(&offset.to_le_bytes());
    }
    debug_assert_eq!(table.len(), table_len);
    writer
        .seek(std::io::SeekFrom::Start(0))
        .and_then(|_| writer.write_all(&table))
        .and_then(|_| writer.flush())
        .map_err(|source| Error::Io {
            context: "finish resource fork",
            source,
        })?;
    writer.get_ref().sync_all().map_err(|source| Error::Io {
        context: "sync resource fork",
        source,
    })?;
    Ok(true)
}

/// Large assets favor ratio-first LZFSE so we do not write and discard a huge
/// LZVN candidate like Gemini's. LZVN remains a codec-specific fallback.
pub(super) fn build_streaming_resource_fork(
    path: &Path,
    raw: &[u8],
) -> Result<Option<Codec>, Error> {
    for codec in [Codec::Lzfse, Codec::Lzvn] {
        if write_streaming_resource_fork(path, raw, codec)? {
            return Ok(Some(codec));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "benchmarks.rs"]
mod local_codec_perf_probe;
