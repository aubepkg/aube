//! Fallback for any OS without a backend — always Unsupported{PlatformBuild}.
use std::path::Path;

use super::{Error, Support, UnsupportedReason};

pub(crate) fn detect(_path: &Path) -> Result<Support, Error> {
    Ok(Support::Unsupported(UnsupportedReason::PlatformBuild))
}

pub(crate) fn is_already_compressed(_path: &Path) -> Result<bool, Error> {
    Ok(false)
}

pub(crate) fn apply_inplace(_path: &Path, _snapshot: &[u8]) -> Result<(), Error> {
    Ok(())
}

/// Never reached — `detect` always reports Unsupported, so `compress_bytes` takes
/// the plain-write fallback and never calls this. Present so the backend surface is
/// uniform across every cfg target.
pub(crate) fn apply_bytes(
    _path: &Path,
    _content: &[u8],
    _mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
    Ok(())
}

/// No FS-specific on-disk signal — apply_guarded falls back to the generic
/// allocated-bytes measurement (st_blocks / GetCompressedFileSizeW), which DOES
/// reflect the win on APFS and NTFS.
pub(crate) fn compressed_on_disk(_path: &Path) -> Result<Option<bool>, Error> {
    Ok(None)
}

/// No clone primitive on an unsupported platform — the caller takes the plain
/// byte-copy path.
pub(crate) fn clone_file(_src: &Path, _dest: &Path) -> Result<bool, Error> {
    Ok(false)
}
