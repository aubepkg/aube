//! Idempotent writer for the tiny executable shims aube materializes into
//! its cache (`node-gyp` stand-ins, the `npm_execpath` stand-in).
//!
//! Every one of them is written on a hot path — once per `aube run`, once
//! per dependency during install lifecycle scripts — so the steady state
//! has to cost a read rather than a create-dir + write-temp + rename +
//! chmod. Content-addressed rather than pinned: a shipped shim fix
//! self-heals on the first run of the new binary, because the bytes change,
//! the comparison misses, and the file is rewritten.
//!
//! Not writing unless the content changed also stops concurrent lifecycle
//! jobs from renaming over each other's shims, and stops interrupted-write
//! temp files from accumulating in the cache dir.

use std::path::Path;

use miette::IntoDiagnostic;

#[cfg(unix)]
pub(crate) const SHIM_MODE: u32 = 0o755;

/// Write one shim, skipping the write when the file on disk already
/// matches.
///
/// The comparison reads through a single open handle and takes the mode
/// from that same handle's `fstat`, so a hit costs open + fstat + read +
/// close and touches nothing. A miss (absent, stale content, or an exec
/// bit that got stripped) falls through to an atomic write + chmod, which
/// is also what repairs the file.
pub(crate) fn write_if_stale(path: &Path, contents: &str) -> miette::Result<()> {
    if is_current(path, contents) {
        return Ok(());
    }
    // `atomic_write` creates the parent dir, so the fast path above can
    // skip `create_dir_all` entirely: a matching file proves the dir.
    aube_util::fs_atomic::atomic_write(path, contents.as_bytes()).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(SHIM_MODE))
            .into_diagnostic()?;
    }
    Ok(())
}

/// True when `path` already holds exactly `contents` and (on unix) is
/// still executable. Any error — missing file, permission trouble,
/// unreadable — reports "not current" so the caller rewrites it.
pub(crate) fn is_current(path: &Path, contents: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    if !meta.is_file() || meta.len() != contents.len() as u64 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Compare only the permission bits; `st_mode` also carries the
        // file type, which `is_file` above has already vetted.
        if meta.permissions().mode() & 0o777 != SHIM_MODE {
            return false;
        }
    }
    let mut on_disk = Vec::with_capacity(contents.len());
    f.read_to_end(&mut on_disk).is_ok() && on_disk == contents.as_bytes()
}
