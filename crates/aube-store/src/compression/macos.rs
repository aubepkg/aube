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
// bindings at 1.9–5.7 MiB. Keeping <=64 MiB on parallel LZVN + one `setxattr`
// leaves 27 MiB of headroom above that observed upper tail; larger assets stream
// to `..namedfork/rsrc` so peak output memory stays bounded. Re-benchmark this
// boundary as native-addon sizes or the streaming implementation changes.
pub(crate) const STREAMING_THRESHOLD: usize = 64 * 1024 * 1024;

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

fn st_flags(path: &Path) -> Result<u32, Error> {
    let cpath = cstring(path)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(cpath.as_ptr(), &mut st) } != 0 {
        return Err(io("lstat"));
    }
    Ok(st.st_flags)
}

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

#[derive(Debug, PartialEq, Eq)]
enum ResourceForkPlan {
    /// The fork would not make the file smaller. Keep the ordinary data fork.
    Plain,
    /// The encoded fork is smaller and every offset fits the on-disk u32 table.
    Compressed { table_len: usize, total_len: usize },
}

fn resource_fork_table_len(num_blocks: usize) -> Result<usize, Error> {
    num_blocks
        .checked_add(1)
        .and_then(|entries| entries.checked_mul(std::mem::size_of::<u32>()))
        .ok_or_else(resource_fork_too_large)
}

/// Decide from lengths alone whether the fork is useful and representable. The
/// raw length in `com.apple.decmpfs` is u64; only offsets inside the resource
/// fork are u32. This allows a raw file beyond the old 3.9 GB cutoff whenever its
/// encoded fork is smaller than both the raw file and `u32::MAX`.
fn plan_resource_fork(
    raw_len: usize,
    num_blocks: usize,
    encoded_len: usize,
) -> Result<ResourceForkPlan, Error> {
    let table_len = resource_fork_table_len(num_blocks)?;
    let total_len = table_len
        .checked_add(encoded_len)
        .ok_or_else(resource_fork_too_large)?;

    if total_len >= raw_len {
        return Ok(ResourceForkPlan::Plain);
    }
    if total_len > u32::MAX as usize {
        return Err(resource_fork_too_large());
    }
    Ok(ResourceForkPlan::Compressed {
        table_len,
        total_len,
    })
}

fn compress_blocks(raw: &[u8], codec: Codec) -> Option<Vec<Vec<u8>>> {
    let num_blocks = raw.len().div_ceil(BLOCK).max(1);
    let scratch_len = unsafe { compression_encode_scratch_buffer_size(codec.algorithm()) };

    // The 64 KiB blocks are independent, so fan them across cores. Each worker
    // owns its libcompression scratch buffer. Contiguous regions keep the output
    // in block order without a sort.
    let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
        1
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(num_blocks)
    };
    if workers <= 1 || num_blocks < 8 {
        let mut scratch = vec![0u8; scratch_len];
        return raw
            .chunks(BLOCK)
            .map(|chunk| compress_block_with_codec(chunk, &mut scratch, codec))
            .collect();
    }

    let bytes_per_worker = num_blocks.div_ceil(workers) * BLOCK;
    let parts: Vec<Option<Vec<Vec<u8>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = raw
            .chunks(bytes_per_worker)
            .map(|region| {
                scope.spawn(move || {
                    let mut scratch = vec![0u8; scratch_len];
                    region
                        .chunks(BLOCK)
                        .map(|chunk| compress_block_with_codec(chunk, &mut scratch, codec))
                        .collect::<Option<Vec<Vec<u8>>>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().ok().flatten())
            .collect()
    });
    let mut out = Vec::with_capacity(num_blocks);
    for part in parts {
        out.extend(part?);
    }
    Some(out)
}

/// Build the com.apple.ResourceFork blob for `raw` in the LZVN/LZFSE decmpfs
/// layout (what `ditto` writes): `(numBlocks+1)` u32 LE offsets, then the blocks.
/// `offset[0]` = table size; `offset[i+1]` = end of block i; last = total size.
/// `Ok(None)` means this codec did not shrink the input.
fn build_resource_fork_with_codec(raw: &[u8], codec: Codec) -> Result<Option<Vec<u8>>, Error> {
    let num_blocks = raw.len().div_ceil(BLOCK).max(1);
    let Some(blocks) = compress_blocks(raw, codec) else {
        return Ok(None);
    };

    let encoded_len = blocks
        .iter()
        .try_fold(0usize, |sum, block| sum.checked_add(block.len()))
        .ok_or_else(resource_fork_too_large)?;
    let ResourceForkPlan::Compressed {
        table_len,
        total_len,
    } = plan_resource_fork(raw.len(), num_blocks, encoded_len)?
    else {
        return Ok(None);
    };

    let mut out = Vec::with_capacity(total_len);
    // Offset table: numBlocks+1 entries. offset[i] is where block i starts.
    let mut offset = u32::try_from(table_len).map_err(|_| resource_fork_too_large())?;
    out.extend_from_slice(&offset.to_le_bytes());
    for block in &blocks {
        offset = offset
            .checked_add(u32::try_from(block.len()).map_err(|_| resource_fork_too_large())?)
            .ok_or_else(resource_fork_too_large)?;
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for block in &blocks {
        out.extend_from_slice(block);
    }
    debug_assert_eq!(out.len(), total_len);
    Ok(Some(out))
}

struct InMemoryResourceFork {
    codec: Codec,
    bytes: Vec<u8>,
}

/// Common addons prefer LZVN decode speed. Only a no-gain LZVN attempt pays for
/// the stronger LZFSE pass.
fn build_in_memory_resource_fork(raw: &[u8]) -> Result<Option<InMemoryResourceFork>, Error> {
    for codec in [Codec::Lzvn, Codec::Lzfse] {
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

fn decmpfs_header(codec: Codec, raw_len: usize) -> [u8; 16] {
    let mut header = [0u8; 16];
    header[..4].copy_from_slice(&DECMPFS_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&codec.compression_type().to_le_bytes());
    header[8..].copy_from_slice(&(raw_len as u64).to_le_bytes());
    header
}

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
        Ok(())
    })();

    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(perm) = mode {
        let _ = std::fs::set_permissions(&tmp, perm);
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
