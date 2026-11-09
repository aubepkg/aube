//! Windows backend — NTFS LZNT1 via `FSCTL_SET_COMPRESSION`.
//!
//! Codec choice is LZNT1, not WOF XPRESS/LZX, even though WOF-XPRESS decodes faster
//! and packs tighter. WOF is "write-once": opening the file for write strips the
//! compression back to plain, so every package-manager **reinstall** that rewrites
//! the `.node` would silently lose it — exactly the workload this targets. LZNT1
//! compresses the existing file IN PLACE (no copy), survives open-for-write, and
//! hardlink siblings share the file record so they compress together (same content;
//! acceptable). For a load-once addon the LZNT1-vs-XPRESS decode delta is marginal,
//! so reinstall-survival wins. Reversal condition: if a consumer re-applies
//! compression on every install (so write-strip is harmless), switch to WOF-XPRESS
//! (`FSCTL_SET_EXTERNAL_BACKING`) for the better ratio + faster decode.
//!
//! Detection gates on the volume's per-file-compression capability, which ReFS/FAT
//! lack.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use std::io::Write;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesW, GetVolumeInformationByHandleW, MoveFileExW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{Error, Support, UnsupportedReason, io};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const OPEN_EXISTING: u32 = 3;
const CREATE_NEW: u32 = 1;
// MoveFileExW flags: replace an existing target atomically, and don't return
// until the rename is flushed to disk.
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
const FSCTL_SET_COMPRESSION: u32 = 0x0009_C040;
const COMPRESSION_FORMAT_DEFAULT: u16 = 1; // LZNT1
const FILE_FILE_COMPRESSION: u32 = 0x0000_0010; // volume supports per-file compression
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
// Required to obtain a handle to a DIRECTORY (CreateFileW fails on a dir without
// it). `detect()` probes the parent directory of a not-yet-created target on a
// fresh install, so every open must set it; harmless on a regular file.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Owning wrapper so the handle is always closed, even on the error paths.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn open(path: &Path, access: u32) -> Result<Handle, Error> {
    open_with(path, access, OPEN_EXISTING)
}

fn open_with(path: &Path, access: u32, disposition: u32) -> Result<Handle, Error> {
    let wpath = wide(path);
    let handle = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            access,
            FILE_SHARE_READ,
            std::ptr::null(),
            disposition,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io("CreateFileW"));
    }
    Ok(Handle(handle))
}

/// Set LZNT1 compression on an open handle (the empty fresh file or an existing
/// one). Shared by `apply_inplace` and `apply_bytes`.
fn set_compression(handle: HANDLE) -> Result<(), Error> {
    let format: u16 = COMPRESSION_FORMAT_DEFAULT;
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_COMPRESSION,
            (&format as *const u16).cast(),
            std::mem::size_of::<u16>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io("FSCTL_SET_COMPRESSION"));
    }
    Ok(())
}

pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
    let handle = open(path, GENERIC_READ)?;
    let mut flags: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle.0,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(io("GetVolumeInformationByHandleW"));
    }
    if flags & FILE_FILE_COMPRESSION != 0 {
        Ok(Support::Supported)
    } else {
        Ok(Support::Unsupported(UnsupportedReason::Filesystem))
    }
}

pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
    let attrs = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(io("GetFileAttributesW"));
    }
    Ok(attrs & FILE_ATTRIBUTE_COMPRESSED != 0)
}

// `_snapshot` is unused on Windows: FSCTL_SET_COMPRESSION flags the EXISTING
// file in place (the kernel recompresses), so there's no temp+rewrite that would
// reuse the caller's bytes.
pub(crate) fn apply_inplace(path: &Path, _snapshot: &[u8]) -> Result<(), Error> {
    let handle = open(path, GENERIC_READ | GENERIC_WRITE)?;
    set_compression(handle.0)
}

/// A collision-resistant sibling temp path. PID + wall-clock nanos + a
/// process-local counter so two writers on the SAME destination never share a
/// temp; paired with CREATE_NEW a collision errors rather than clobbering.
/// Mirrors the macOS/Linux backends' scheme.
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

/// Atomically publish `tmp` over `dest` (replace + write-through).
fn move_file_replace(tmp: &Path, dest: &Path) -> Result<(), Error> {
    let wtmp = wide(tmp);
    let wdest = wide(dest);
    let ok = unsafe {
        MoveFileExW(
            wtmp.as_ptr(),
            wdest.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(io("MoveFileExW"));
    }
    Ok(())
}

pub(crate) fn prepare_stream(file: &std::fs::File) -> Result<(), Error> {
    use std::os::windows::io::AsRawHandle;
    set_compression(file.as_raw_handle() as HANDLE)
}

pub(crate) fn publish_stream(tmp: &Path, dest: &Path) -> Result<(), Error> {
    move_file_replace(tmp, dest)
}

/// Write `content` to `path` as a fresh NTFS-compressed file in ONE pass, then
/// publish it atomically: create a sibling temp with CREATE_NEW,
/// FSCTL_SET_COMPRESSION on the EMPTY handle (so writes compress on the way in —
/// never a write-then-recompress), write + flush, then MoveFileEx over `path`.
/// The temp+rename matches the macOS/Linux backends and fixes the old
/// CREATE_ALWAYS-in-place write, which truncated `path` before streaming — a
/// crash or a concurrent open saw a partial `.node`. `mode` is unused on Windows
/// (no POSIX bits). Shared by `compress_bytes`.
pub(crate) fn apply_bytes(
    path: &Path,
    content: &[u8],
    _mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
    let dir = path.parent().ok_or_else(|| io("no parent dir"))?;
    let name = path.file_name().map_or_else(
        || std::borrow::Cow::Borrowed("addon"),
        |n| n.to_string_lossy(),
    );
    let tmp = unique_tmp(dir, &name);

    let result = (|| {
        let handle = open_with(&tmp, GENERIC_READ | GENERIC_WRITE, CREATE_NEW)?;
        set_compression(handle.0)?;
        // Borrow the raw handle into a File for the write, then forget it so the
        // Handle wrapper (not File) does the single CloseHandle.
        use std::os::windows::io::FromRawHandle;
        let mut file = unsafe { std::fs::File::from_raw_handle(handle.0 as _) };
        let res = file
            .write_all(content)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all());
        std::mem::forget(file);
        res.map_err(|source| Error::Io {
            context: "write temp",
            source,
        })
    })();

    match result {
        Ok(()) => move_file_replace(&tmp, path).inspect_err(|_| {
            // A failed publish (e.g. the dest is loaded → SHARING_VIOLATION) leaves the
            // original file untouched; drop the orphan temp.
            let _ = std::fs::remove_file(&tmp);
        }),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// No FS-specific on-disk signal — apply_guarded falls back to the generic
/// allocated-bytes measurement (st_blocks / GetCompressedFileSizeW), which DOES
/// reflect the win on APFS and NTFS.
pub(crate) fn compressed_on_disk(_path: &Path) -> Result<Option<bool>, Error> {
    Ok(None)
}

/// NTFS has no reflink/clone primitive (ReFS block-cloning is out of scope) —
/// always report "cannot clone" so the caller takes the byte-copy path, which
/// re-applies `FSCTL_SET_COMPRESSION` at the destination.
pub(crate) fn clone_file(_src: &Path, _dest: &Path) -> Result<bool, Error> {
    Ok(false)
}
