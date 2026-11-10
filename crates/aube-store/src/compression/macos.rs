//! macOS backend — APFS/HFS+ decmpfs transparent compression.
//!
//! decmpfs is an undocumented kernel ABI; afsctool and `ditto --hfsCompression` are
//! the references. We write the LZVN (type 8) and LZFSE (type 12) resource-fork
//! variants: the kernel decompresses on read(), so the file keeps its logical size
//! and stays a loadable native binary. Both codecs come from the system
//! libcompression library, so the macOS stub gains no Rust codec dependency.
//!
//! Common `.node` files take the speed-first LZVN path, with LZFSE as a no-gain
//! fallback. Large assets stream ratio-first LZFSE blocks directly into the named
//! resource fork, avoiding both a multi-gigabyte output allocation and `setxattr`'s
//! `E2BIG` ceiling. LZVN is the last fallback there for unusual codec-specific data.
//!
//! Layout written (verified by the kernel-roundtrip test):
//!   xattr com.apple.decmpfs      = [magic u32 LE][type=8/12 u32 LE][rawSize u64 LE]
//!   xattr com.apple.ResourceFork = [(numBlocks+1) u32 LE offsets][codec blocks]
//! A winning fork is built on an empty sibling temp with those xattrs and
//! UF_COMPRESSED; an expanding fork becomes an ordinary sibling data fork. Either
//! form is atomically renamed over the original — never an in-place truncate, so
//! a crash can't leave a 0-byte file.

use std::os::fd::AsRawFd;
use std::path::Path;

use super::{Error, Support, UnsupportedReason, cstring, io};

const UF_COMPRESSED: u32 = 0x0000_0020;
const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' (XNU sys/decmpfs.h); LE on disk = "fpmc"
const BLOCK: usize = 0x1_0000; // 64 KiB
const XATTR_NOFOLLOW: libc::c_int = 0x0001;
const COMPRESSION_LZVN: i32 = 0x900;
const COMPRESSION_LZFSE: i32 = 0x801;

// 2026-07-16 — The data supports keeping a 64 MiB in-memory fast path for now.
// The sampled Darwin ARM64 Vite ecosystem topped out at SWC 36.563 MiB, followed
// by Rolldown 15.6–17.9 MiB, Oxlint 14.4 MiB, Lightning CSS 8.1 MiB, and Oxc
// bindings at 1.9–5.7 MiB. Keeping <=64 MiB on parallel LZFSE + one `setxattr`
// leaves 27 MiB of headroom above that observed upper tail; larger assets stream
// to `..namedfork/rsrc` so peak output memory stays bounded. Re-benchmark this
// boundary as native-addon sizes or the streaming implementation changes.
pub(crate) const STREAMING_THRESHOLD: usize = 64 * 1024 * 1024;

/// Use streamed output only when the raw input exceeds the in-memory threshold.
fn should_stream_resource_fork(raw_len: usize, threshold: usize) -> bool {
    raw_len > threshold
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Lzvn,
    Lzfse,
}

impl Codec {
    const fn compression_type(self) -> u32 {
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

/// Report an unrepresentable resource-fork offset as a size-limit error.
fn resource_fork_too_large() -> Error {
    Error::Io {
        context: "decmpfs resource fork exceeds u32 offsets",
        source: std::io::Error::from_raw_os_error(libc::EFBIG),
    }
}

/// Read filesystem capabilities, propagating path and system-call errors.
fn statfs(path: &Path) -> Result<libc::statfs, Error> {
    let cpath = cstring(path)?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return Err(io("statfs"));
    }
    Ok(buf)
}

/// Local APFS or HFS+ only — the two filesystems with the decmpfs path. A network
/// or non-local mount reports Unsupported (the signal isn't ours to trust).
pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
    let buf = statfs(path)?;
    // f_fstypename is a NUL-padded C string ("apfs", "hfs").
    let name: Vec<u8> = buf
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Ok(classify_fs(
        buf.f_flags & (libc::MNT_LOCAL as u32) != 0,
        &name,
    ))
}

/// The pure detect policy — split from the statfs syscall so the network/non-APFS
/// branches are unit-testable without a network mount or an exotic filesystem.
fn classify_fs(is_local: bool, fstype: &[u8]) -> Support {
    if !is_local {
        return Support::Unsupported(UnsupportedReason::NetworkOrOverlay);
    }
    if fstype == b"apfs" || fstype == b"hfs" {
        Support::Supported
    } else {
        Support::Unsupported(UnsupportedReason::Filesystem)
    }
}

/// Read file flags without following a symlink.
fn st_flags(path: &Path) -> Result<u32, Error> {
    let cpath = cstring(path)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(cpath.as_ptr(), &mut st) } != 0 {
        return Err(io("lstat"));
    }
    Ok(st.st_flags)
}

/// Check the kernel flag that enables transparent resource-fork decoding.
pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
    Ok(st_flags(path)? & UF_COMPRESSED != 0)
}

/// On macOS, UF_COMPRESSED is the authoritative win signal (st_blocks also drops,
/// but the flag is unambiguous and what we set).
pub(crate) fn compressed_on_disk(path: &Path) -> Result<Option<bool>, Error> {
    Ok(Some(is_already_compressed(path)?))
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
fn build_resource_fork_with_codec(raw: &[u8], codec: Codec) -> Result<Option<Vec<u8>>, Error> {
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

struct InMemoryResourceFork {
    codec: Codec,
    bytes: Vec<u8>,
}

/// Prefer LZFSE for ordinary addons: paired local measurements show a smaller
/// resource fork with equal decode time and lower encode time. LZVN remains a
/// fallback for inputs that LZFSE cannot shrink.
fn build_in_memory_resource_fork(raw: &[u8]) -> Result<Option<InMemoryResourceFork>, Error> {
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
fn write_streaming_resource_fork(path: &Path, raw: &[u8], codec: Codec) -> Result<bool, Error> {
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
fn build_streaming_resource_fork(path: &Path, raw: &[u8]) -> Result<Option<Codec>, Error> {
    for codec in [Codec::Lzfse, Codec::Lzvn] {
        if write_streaming_resource_fork(path, raw, codec)? {
            return Ok(Some(codec));
        }
    }
    Ok(None)
}

/// Serialize the OS compression header with the codec and original logical size.
fn decmpfs_header(codec: Codec, raw_len: usize) -> [u8; 16] {
    let mut header = [0u8; 16];
    header[..4].copy_from_slice(&DECMPFS_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&codec.compression_type().to_le_bytes());
    header[8..].copy_from_slice(&(raw_len as u64).to_le_bytes());
    header
}

/// Write a compression attribute without following symlinks; preserve syscall errors.
fn setxattr(path: &std::ffi::CStr, name: &std::ffi::CStr, value: &[u8]) -> Result<(), Error> {
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            XATTR_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io("setxattr"));
    }
    Ok(())
}

/// Rebuild compression from the supplied snapshot while retaining file permissions.
pub(crate) fn apply_inplace(path: &Path, snapshot: &[u8]) -> Result<(), Error> {
    // Fail-soft: skip if we can't write the original (by mode or ownership) — the
    // temp+rename below would otherwise replace even a file we can't open for write.
    let cpath = cstring(path)?;
    if unsafe { libc::access(cpath.as_ptr(), libc::W_OK) } != 0 {
        return Err(io("access"));
    }

    // `snapshot` is the file's bytes the caller already read for rollback — reuse
    // it instead of a second full read.
    let mode = std::fs::metadata(path).map(|m| m.permissions()).ok();
    apply_bytes(path, snapshot, mode)
}

/// Write `content` to `path` as a fresh decmpfs-compressed file in ONE pass — no
/// write-then-read-back. The decmpfs is built directly from `content`, dropped on a
/// sibling temp (empty data fork + the two xattrs + UF_COMPRESSED when compression
/// wins; ordinary data fork on no gain), then atomically renamed over `path`. A
/// crash can only leave the original or the finished file; the rename also gives a
/// fresh inode, the copy-break from any pnpm CAS hardlink siblings. This is the
/// one-pass core both `compress_bytes` (no original) and `apply_inplace` (read
/// first) share.
pub(crate) fn apply_bytes(
    path: &Path,
    content: &[u8],
    mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
    apply_bytes_with_streaming_threshold(path, content, mode, STREAMING_THRESHOLD)
}

/// Select buffered or streamed resource-fork output before atomically publishing the file.
fn apply_bytes_with_streaming_threshold(
    path: &Path,
    content: &[u8],
    mode: Option<std::fs::Permissions>,
    streaming_threshold: usize,
) -> Result<(), Error> {
    let stream = should_stream_resource_fork(content.len(), streaming_threshold);
    let in_memory_resource_fork = if stream {
        None
    } else {
        build_in_memory_resource_fork(content)?
    };

    let dir = path.parent().ok_or_else(|| io("parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io("file_name"))?
        .to_string_lossy();
    // Uniqueness beyond the PID: a crash can leave `.name.aube-fscompress-<pid>.tmp`, and a
    // later run that reuses that PID (common after reboot) would fail `create_new`
    // forever. PID + wall-clock nanos + a process-local counter makes a stale
    // sibling collision astronomically unlikely while keeping `create_new`'s
    // concurrent-writer safety.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{name}.aube-fscompress-{}-{nanos}-{seq}.tmp",
        std::process::id()
    ));

    let build = (|| -> Result<(), Error> {
        let create_temp = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|source| Error::Io {
                    context: "create temp",
                    source,
                })
        };
        let mut file = create_temp()?;
        let ctmp = cstring(&tmp)?;
        let codec = if stream {
            build_streaming_resource_fork(&tmp, content)?
        } else if let Some(resource_fork) = &in_memory_resource_fork {
            // Install the payload before publishing metadata that tells the kernel to
            // decode it. The temp inode is not visible at the destination yet either
            // way, but this ordering also keeps direct temp-path observers safe.
            setxattr(&ctmp, c"com.apple.ResourceFork", &resource_fork.bytes)?;
            Some(resource_fork.codec)
        } else {
            None
        };

        if let Some(codec) = codec {
            setxattr(
                &ctmp,
                c"com.apple.decmpfs",
                &decmpfs_header(codec, content.len()),
            )?;
            if unsafe { libc::fchflags(file.as_raw_fd(), UF_COMPRESSED) } != 0 {
                return Err(io("fchflags"));
            }
        } else {
            // A losing streamed attempt left a partial resource fork on the temp.
            // Recreate the inode rather than publish a plain file with stale fork data.
            if stream {
                drop(file);
                std::fs::remove_file(&tmp).map_err(|source| Error::Io {
                    context: "remove losing streamed temp",
                    source,
                })?;
                file = create_temp()?;
            }
            use std::io::Write;
            file.write_all(content).map_err(|source| Error::Io {
                context: "plain temp write",
                source,
            })?;
            file.sync_all().map_err(|source| Error::Io {
                context: "plain temp sync",
                source,
            })?;
        }
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(mode.unwrap_or_else(|| std::fs::Permissions::from_mode(0o644)))
            .map_err(|source| Error::Io {
                context: "set completed file permissions",
                source,
            })?;
        Ok(())
    })();

    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Preserve ownership across the rewrite. Running as root (a global npm install,
    // a Docker build) the temp is created owned by the current euid, so the rename
    // would otherwise change the file's owner. Match the original's uid/gid.
    // Best-effort: a no-op for a new path (nothing to preserve) and for a non-root
    // process (chown to another owner is EPERM — but then the file was already
    // ours, so nothing changes).
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
    }
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io {
            context: "rename",
            source,
        }
    })
}

/// Copy-on-write clone via `clonefile(2)` — shares the extents AND the decmpfs
/// state, so a compressed source stays compressed at zero cost. `Ok(false)`
/// means "cannot clone here" (cross-volume, unsupported FS, …) and the caller
/// falls back to a byte copy; a failed clonefile never leaves a partial
/// destination.
pub(crate) fn clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
    let csrc = cstring(src)?;
    let cdest = cstring(dest)?;
    Ok(unsafe { libc::clonefile(csrc.as_ptr(), cdest.as_ptr(), 0) } == 0)
}

#[cfg(test)]
mod resource_fork_tests {
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
                    u32::from_le_bytes(fork[table_start..table_start + 4].try_into().unwrap())
                        as usize;
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
}

#[cfg(test)]
mod local_codec_perf_probe {
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
        let outcome = super::super::compress_bytes(&target, &raw, &super::super::Gate::any())
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
                        let encoded_start = u32::from_le_bytes(
                            fork[table_start..table_start + 4].try_into().unwrap(),
                        ) as usize;
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
}
