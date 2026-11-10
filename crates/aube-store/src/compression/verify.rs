//! Platform-agnostic measurement + loadability surface.

use std::io::Read;
use std::path::Path;

use super::Error;

/// On-disk *allocated* bytes — `st_blocks * 512` on POSIX, `GetCompressedFileSizeW`
/// on Windows. Never `st_size`: transparent compression holds the logical size
/// constant, so only the allocation reveals the win.
pub(crate) fn on_disk_bytes(path: &Path) -> Result<u64, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).map_err(|source| Error::Io {
            context: "stat",
            source,
        })?;
        Ok(meta.blocks().saturating_mul(512))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut high: u32 = 0;
        // Returns the actual allocated size (post-NTFS-compression), low dword as the
        // return value + high dword via the out-param. INVALID_FILE_SIZE (u32::MAX) is
        // only an error if GetLastError is non-zero (it can also be a legit low dword).
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low == u32::MAX {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error().unwrap_or(0) != 0 {
                return Err(Error::Io {
                    context: "GetCompressedFileSizeW",
                    source: err,
                });
            }
        }
        Ok(((high as u64) << 32) | low as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let meta = std::fs::metadata(path).map_err(|source| Error::Io {
            context: "stat",
            source,
        })?;
        Ok(meta.len())
    }
}

/// First 4 bytes — the native-binary magic (ELF `7f454c46`, Mach-O `cffaedfe`/…,
/// PE `4d5a`). Compared before/after apply to assert the file still loads, in
/// place of a content hash that transparent compression would render vacuous.
pub(crate) fn magic_prefix(path: &Path) -> Result<[u8; 4], Error> {
    let mut file = std::fs::File::open(path).map_err(|source| Error::Io {
        context: "open",
        source,
    })?;
    let mut buf = [0u8; 4];
    file.read(&mut buf).map_err(|source| Error::Io {
        context: "read",
        source,
    })?;
    Ok(buf)
}

/// Stream-compare the file at `path` against `expected`, letting the kernel
/// decompress transparently, through a fixed reusable 64 KiB buffer — never
/// materializing a second full copy of the file (the read-back oracle runs on
/// every compressed write, so a `std::fs::read` here would heap-allocate a whole
/// extra copy of every addon). Returns `false` on the first differing byte or any
/// length divergence (a short OR long read-back), and short-circuits on mismatch.
pub(crate) fn readback_matches(path: &Path, expected: &[u8]) -> Result<bool, Error> {
    let mut file = std::fs::File::open(path).map_err(|source| Error::Io {
        context: "read-back",
        source,
    })?;
    let mut buf = [0u8; 64 * 1024];
    let mut off = 0usize;
    loop {
        let n = file.read(&mut buf).map_err(|source| Error::Io {
            context: "read-back",
            source,
        })?;
        if n == 0 {
            break;
        }
        // A read-back that overruns `expected`, or a chunk that differs, is a
        // mismatch — bail without reading the rest.
        if off + n > expected.len() || buf[..n] != expected[off..off + n] {
            return Ok(false);
        }
        off += n;
    }
    // Equal only if the on-disk length matched exactly (a truncated read-back is a
    // mismatch the loop can't otherwise catch).
    Ok(off == expected.len())
}
