//! Linux backend — btrfs (Stage 1); bcachefs later. ext4/xfs/ZFS cannot be done.
//!
//! Detection is an allowlist: only btrfs carries the per-file `FS_COMPR_FL` we set,
//! so every other f_type (incl. overlay/NFS) reports Unsupported. Apply uses the
//! SYNCHRONOUS copy+flag path (create a sibling, request the codec BEFORE writing
//! so the kernel compresses the bytes as they land, then atomic-rename over the
//! target) — NOT `BTRFS_IOC_DEFRAG_RANGE`, whose async completion races a
//! post-apply `st_blocks` check. The rename also gives a fresh inode: the
//! copy-break that isolates any pnpm CAS hardlink siblings.
//!
//! Codec preference is zstd → lzo → zlib (speed-over-ratio: zstd decodes faster
//! than zlib and tunes ratio, lzo is the fastest fallback). We request it the way
//! `btrfs property set <file> compression <algo>` does — a write to the
//! `btrfs.compression` xattr — and walk the list because an old kernel rejects an
//! unsupported algo (zstd pre-4.14, lzo pre-3.14) with EINVAL. If none take we
//! fall back to the bare `FS_COMPR_FL` flag (the mount's default algorithm).

use std::os::fd::AsRawFd;
use std::path::Path;

use super::{Error, Support, UnsupportedReason, cstring, io};

// Allowlist magic (statfs f_type). Only btrfs from the Stage-1 matrix.
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

// FS_IOC_{GET,SET}FLAGS are _IOR/_IOW('f', 1|2, long). The request value is a u32;
// `as _` at the call site casts it to whatever `libc::ioctl` expects per target
// (c_ulong on glibc, c_int on musl) — same 32 bits either way.
const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
// FS_IOC_FIEMAP = _IOWR('f', 11, struct fiemap). Used to read whether the data
// extents are ENCODED (compressed) — btrfs reports st_blocks as the LOGICAL size,
// so the on-disk win is invisible to stat/du and only FIEMAP/compsize reveals it.
const FS_IOC_FIEMAP: u32 = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x0001;
const FIEMAP_EXTENT_ENCODED: u32 = 0x0008;
// uapi/linux/fs.h: FS_COMPR_FL is 0x4 (0x20 is FS_APPEND_FL — append-only, which
// makes writes and renames EPERM).
const FS_COMPR_FL: libc::c_long = 0x0000_0004;

/// Read the filesystem type used to restrict compression to btrfs.
fn statfs_type(path: &Path) -> Result<i64, Error> {
    let cpath = cstring(path)?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return Err(io("statfs"));
    }
    Ok(buf.f_type as i64)
}

/// Read inode flags through the filesystem ioctl.
fn get_flags(fd: libc::c_int) -> Result<libc::c_long, Error> {
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS as _, &mut flags) } != 0 {
        return Err(io("FS_IOC_GETFLAGS"));
    }
    Ok(flags)
}

// Per-file codecs in preference order (speed-over-ratio). zstd: fast decode, good
// ratio, tunable. lzo: fastest decode, weakest ratio. zlib: legacy fallback.
const PREFERRED_ALGOS: [&[u8]; 3] = [b"zstd", b"lzo", b"zlib"];

// Request a codec the way `btrfs property set <file> compression <algo>` does — by
// writing the `btrfs.compression` xattr — trying most-preferred first. A kernel
// without a given algo rejects it (EINVAL), so we walk the list; if none take we
// fall back to the bare FS_COMPR_FL flag (the mount's default codec). Called on the
// empty temp file so the bytes compress as they're written.
/// Select a supported codec, falling back to the mount default only if all names are rejected.
fn request_codec(fd: libc::c_int) -> Result<(), Error> {
    let selected = try_preferred_codec(|algo| {
        let rc = unsafe {
            libc::fsetxattr(
                fd,
                c"btrfs.compression".as_ptr(),
                algo.as_ptr().cast(),
                algo.len(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
    .map_err(|source| Error::Io {
        context: "set btrfs.compression",
        source,
    })?;
    if selected {
        return Ok(());
    }
    let flags = get_flags(fd)? | FS_COMPR_FL;
    if unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS as _, &flags) } != 0 {
        return Err(io("FS_IOC_SETFLAGS"));
    }
    Ok(())
}

/// Select the first supported codec. Only `EINVAL` means that this kernel
/// rejected a codec name; every other error must reach the caller unchanged.
fn try_preferred_codec(
    mut set_codec: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    for algo in PREFERRED_ALGOS {
        match set_codec(algo) {
            Ok(()) => return Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod codec_tests {
    use super::try_preferred_codec;

    #[test]
    fn retries_unsupported_codecs_in_preference_order() {
        let mut tried = Vec::new();
        let selected = try_preferred_codec(|codec| {
            tried.push(codec.to_vec());
            if codec == b"zlib" {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(libc::EINVAL))
            }
        })
        .unwrap();
        assert!(selected);
        assert_eq!(tried, [b"zstd".to_vec(), b"lzo".to_vec(), b"zlib".to_vec()]);
    }

    #[test]
    fn preserves_non_codec_errors_without_retrying() {
        let mut attempts = 0;
        let error = try_preferred_codec(|_| {
            attempts += 1;
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn reports_when_all_codecs_are_unsupported() {
        assert!(
            !try_preferred_codec(|_| { Err(std::io::Error::from_raw_os_error(libc::EINVAL)) })
                .unwrap()
        );
    }
}

/// Request compression on the empty file before streaming bytes into it.
pub(crate) fn prepare_stream(file: &std::fs::File) -> Result<(), Error> {
    request_codec(file.as_raw_fd())
}

/// Enable the backend only when the path belongs to a btrfs filesystem.
pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
    if statfs_type(path)? == BTRFS_SUPER_MAGIC {
        Ok(Support::Supported)
    } else {
        Ok(Support::Unsupported(UnsupportedReason::Filesystem))
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

#[repr(C)]
struct FiemapHeader {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

const _: () = assert!(std::mem::size_of::<FiemapHeader>() == 32);
const _: () = assert!(std::mem::size_of::<FiemapExtent>() == 56);

/// True if any data extent carries FIEMAP_EXTENT_ENCODED (i.e. is compressed on
/// disk). This is the only reliable btrfs win signal — st_blocks reflects the
/// logical size. FIEMAP_FLAG_SYNC flushes pending writes before mapping.
fn compressed_via_fiemap(path: &Path) -> Result<bool, Error> {
    const COUNT: usize = 64;
    let file = std::fs::File::open(path).map_err(|source| Error::Io {
        context: "open for fiemap",
        source,
    })?;
    let mut buf = vec![
        0u8;
        std::mem::size_of::<FiemapHeader>()
            + COUNT * std::mem::size_of::<FiemapExtent>()
    ];
    // SAFETY: buf is sized for the header + COUNT extents; the kernel fills it.
    unsafe {
        let header = buf.as_mut_ptr().cast::<FiemapHeader>();
        (*header).fm_start = 0;
        (*header).fm_length = u64::MAX;
        (*header).fm_flags = FIEMAP_FLAG_SYNC;
        (*header).fm_extent_count = COUNT as u32;
        (*header).fm_mapped_extents = 0;
        (*header).fm_reserved = 0;
        if libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, buf.as_mut_ptr()) != 0 {
            return Err(io("FS_IOC_FIEMAP"));
        }
        let mapped = (*header).fm_mapped_extents as usize;
        let extents = std::slice::from_raw_parts(
            buf.as_ptr()
                .add(std::mem::size_of::<FiemapHeader>())
                .cast::<FiemapExtent>(),
            mapped.min(COUNT),
        );
        Ok(extents
            .iter()
            .any(|e| e.fe_flags & FIEMAP_EXTENT_ENCODED != 0))
    }
}

/// btrfs win measurement is authoritative via FIEMAP, not st_blocks.
pub(crate) fn compressed_on_disk(path: &Path) -> Result<Option<bool>, Error> {
    Ok(Some(compressed_via_fiemap(path)?))
}

/// Check whether the inode requests compression for subsequent writes.
pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
    let file = std::fs::File::open(path).map_err(|source| Error::Io {
        context: "open",
        source,
    })?;
    Ok(get_flags(file.as_raw_fd())? & FS_COMPR_FL != 0)
}

/// Rewrite the existing snapshot atomically, retaining the original permissions.
pub(crate) fn apply_inplace(path: &Path, snapshot: &[u8]) -> Result<(), Error> {
    // `snapshot` is the file's bytes the caller already read for rollback — reuse
    // it instead of a second full read.
    let mode = std::fs::metadata(path).map(|m| m.permissions()).ok();
    apply_bytes(path, snapshot, mode)
}

/// A collision-resistant sibling temp path for the atomic write. PID +
/// wall-clock nanos + a process-local counter means two threads/processes
/// compressing the SAME destination never derive the same temp name (a PID-only
/// name with create+truncate would interleave their writes, or silently adopt a
/// crashed run's stale temp). Paired with `create_new`, a collision is detected
/// rather than truncated through. Mirrors the macOS backend's scheme.
fn unique_tmp(dir: &Path, name: &str) -> std::path::PathBuf {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        ".{name}.aube-fscompress-{}-{nanos}-{seq}.tmp",
        std::process::id()
    ))
}

/// Write `content` to `path` as a fresh btrfs-compressed file in ONE pass: create
/// the sibling temp, request the codec on the EMPTY file (so the bytes compress as
/// they land — never a write-then-recompress), write, fsync, then atomic-rename
/// over `path`. The rename gives a fresh inode, the copy-break from any pnpm CAS
/// hardlink siblings. Shared by `compress_bytes` (no original) and `apply_inplace`.
pub(crate) fn apply_bytes(
    path: &Path,
    content: &[u8],
    mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
    use std::io::Write;

    let dir = path.parent().ok_or_else(|| Error::Io {
        context: "no parent dir",
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })?;
    let name = path.file_name().map_or_else(
        || std::borrow::Cow::Borrowed("addon"),
        |n| n.to_string_lossy(),
    );
    let tmp = unique_tmp(dir, &name);

    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|source| Error::Io {
                context: "create temp",
                source,
            })?;
        let fd = file.as_raw_fd();
        // Request the codec on the empty file FIRST so writes compress on the way in.
        request_codec(fd)?;
        file.write_all(content).map_err(|source| Error::Io {
            context: "write temp",
            source,
        })?;
        file.sync_all().map_err(|source| Error::Io {
            context: "fsync temp",
            source,
        })?;
        Ok(())
    })();

    if result.is_ok() {
        if let Some(perm) = mode {
            let _ = std::fs::set_permissions(&tmp, perm);
        }
        // Preserve ownership across the rewrite. As root (a global npm install, a
        // Docker build) the temp is owned by the current euid, so the rename would
        // otherwise change the file's owner. Match the original's uid/gid.
        // Best-effort: a no-op for a new path and for a non-root process (chown to
        // another owner is EPERM — but then the file was already ours).
        if let Ok(meta) = std::fs::metadata(path) {
            use std::os::unix::fs::MetadataExt;
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
    }

    match result {
        Ok(()) => std::fs::rename(&tmp, path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            Error::Io {
                context: "rename",
                source,
            }
        }),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Copy-on-write clone via the `FICLONE` ioctl (btrfs / XFS reflink) — shares
/// the extents AND their compression property, so a compressed source stays
/// compressed at zero cost. `Ok(false)` means "cannot clone here"
/// (cross-device, non-reflink FS, …) and the caller falls back to a byte copy;
/// a failed clone removes the empty destination it created.
pub(crate) fn clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
    use std::os::fd::AsRawFd;
    let src_file = match std::fs::File::open(src) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    let dest_file = match std::fs::File::create(dest) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    // FICLONE = _IOW(0x94, 9, int) — stable since Linux 4.5. musl's ioctl takes
    // the request as i32; glibc's takes c_ulong — same 31-bit-safe constant.
    #[cfg(target_env = "musl")]
    const FICLONE: libc::c_int = 0x4004_9409 as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let cloned = unsafe { libc::ioctl(dest_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) } == 0;
    if !cloned {
        drop(dest_file);
        let _ = std::fs::remove_file(dest);
    }
    Ok(cloned)
}
