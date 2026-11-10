//! macOS backend — APFS/HFS+ decmpfs transparent compression.
//!
//! The kernel reads LZFSE (type 12) or LZVN (type 8) blocks from the resource
//! fork. Prefer LZFSE, falling back to LZVN if it cannot shrink the input.
//! Large files stream their output to avoid a full output allocation and the
//! `setxattr` size limit. Both codecs use the system libcompression library.
//!
//! The decmpfs xattr contains the magic, codec type, and logical size. The
//! resource fork contains little-endian u32 block offsets followed by the blocks.
//! Build both on a sibling file, set UF_COMPRESSED, then rename into place.
//! The kernel round-trip tests validate this format; afsctool and
//! `ditto --hfsCompression` are the reference implementations.

use std::os::fd::AsRawFd;
use std::path::Path;

use super::{Error, Support, UnsupportedReason, cstring, io};

const UF_COMPRESSED: u32 = 0x0000_0020;
const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' (XNU sys/decmpfs.h); LE on disk = "fpmc"
const XATTR_NOFOLLOW: libc::c_int = 0x0001;

// Keep ordinary addons in memory; stream larger files to bound output memory
// and avoid the resource-fork xattr size limit.
pub(crate) const STREAMING_THRESHOLD: usize = 64 * 1024 * 1024;

/// Use streamed output only when the raw input exceeds the in-memory threshold.
fn should_stream_resource_fork(raw_len: usize, threshold: usize) -> bool {
    raw_len > threshold
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

#[path = "macos/resource_fork.rs"]
mod resource_fork;
use resource_fork::{Codec, build_in_memory_resource_fork, build_streaming_resource_fork};
