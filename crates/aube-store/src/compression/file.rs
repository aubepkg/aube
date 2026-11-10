//! File identity and atomic plain-file publication.

use super::{Error, Path, copy_file};

/// True when both paths name the same underlying file — same dev+inode on
/// unix, or same volume serial + file index on Windows. Guards [`copy_file`]'s
/// replace-by-default from removing its own source.
#[cfg(unix)]
pub(super) fn is_same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(meta_a), Ok(meta_b)) => meta_a.dev() == meta_b.dev() && meta_a.ino() == meta_b.ino(),
        _ => false,
    }
}

#[cfg(windows)]
pub(super) fn is_same_file(a: &Path, b: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    fn identity(path: &Path) -> Option<(u32, u64)> {
        let file = std::fs::File::open(path).ok()?;
        let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return None;
        }
        Some((
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }

    match (identity(a), identity(b)) {
        (Some(id_a), Some(id_b)) => id_a == id_b,
        // Preserve the exact-path fast path if a file-information query is denied.
        _ => match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(canon_a), Ok(canon_b)) => canon_a == canon_b,
            _ => false,
        },
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(canon_a), Ok(canon_b)) => canon_a == canon_b,
        _ => false,
    }
}

/// Fail-soft plain atomic write: sibling temp + fsync + rename. The never-break-the
/// -install floor under every `compress_bytes` fallback.
pub(super) fn plain_write(path: &Path, content: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| Error::Io {
        context: "no parent dir",
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })?;
    let mut temp = tempfile::Builder::new()
        .prefix(".aube-plain-")
        .tempfile_in(dir)
        .map_err(|source| Error::Io {
            context: "plain write temp",
            source,
        })?;
    temp.write_all(content)
        .and_then(|()| {
            // Keep incomplete bytes private; publish the completed CAS file
            // with the same permissions as the ordinary store writer.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temp.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o644))?;
            }
            temp.as_file().sync_all()
        })
        .map_err(|source| Error::Io {
            context: "plain write temp",
            source,
        })?;
    // Close our handle before replacement; Windows can temporarily deny a
    // rename while another writer or reader still holds the destination open.
    let temp = temp.into_temp_path();
    for attempt in 0..5 {
        match std::fs::rename(&temp, path) {
            Ok(()) => return Ok(()),
            Err(source)
                if cfg!(windows)
                    && matches!(source.raw_os_error(), Some(5 | 32 | 33))
                    && attempt < 4 =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20 << attempt));
            }
            Err(source) => {
                return Err(Error::Io {
                    context: "plain write rename",
                    source,
                });
            }
        }
    }
    unreachable!("the last rename attempt always returns")
}
