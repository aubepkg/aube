//! Shared orchestration that gates every backend identically. Written once here
//! so a backend only implements `detect` / `is_already_compressed` /
//! `apply_inplace` and inherits all the safety invariants.

use std::path::Path;

use super::{Backend, Error, Outcome, SkipReason, verify};

/// Reached only when the backend reported `Supported`. Fail-soft: a permission,
/// read-only, or busy failure is a `Skipped` Outcome, never a hard `Err`. And if a
/// (broken) backend ever leaves the file no longer loadable, roll back to the
/// pre-apply bytes so a corrupt addon is never stranded.
pub(crate) fn apply_guarded<B: Backend>(backend: &B, path: &Path) -> Result<Outcome, Error> {
    // INV-idempotent.
    if backend.is_already_compressed(path)? {
        return Ok(Outcome::AlreadyCompressed {
            before: verify::on_disk_bytes(path)?,
        });
    }

    let before = verify::on_disk_bytes(path)?;

    // INV-loadable: snapshot the native-binary magic so we can confirm the file
    // still loads after compression (a post-compress *content* hash is vacuous —
    // the kernel decompresses on read, so it always matches).
    let magic_before = verify::magic_prefix(path)?;

    // INV-rollback: keep the original bytes so a backend that produces a non-loadable
    // result can be reverted. Cheap next to the one-time warm decompress.
    let snapshot = std::fs::read(path).map_err(|source| Error::Io {
        context: "snapshot",
        source,
    })?;

    // INV-fail-soft: EACCES/EPERM/EROFS -> Skipped(PermissionDenied); EBUSY/ETXTBSY
    // -> Skipped(Busy). A genuine, unclassifiable I/O error still propagates.
    if let Err(err) = backend.apply_inplace(path, &snapshot) {
        if let Error::Io { source, .. } = &err
            && let Some(reason) = classify_skip(source)
        {
            return Ok(Outcome::Skipped { reason });
        }
        return Err(err);
    }

    verify_loadable_or_restore(backend, path, before, magic_before, &snapshot)
}

/// Post-apply gate for the in-place path: if the file no longer carries its
/// native-binary magic the backend broke it, so restore the snapshot and report
/// `Skipped(NotLoadable)`; otherwise classify the win. Split out so the
/// not-loadable rollback is unit-testable without a backend that corrupts a file
/// (pass a `magic_before` the file no longer matches).
fn verify_loadable_or_restore<B: Backend>(
    backend: &B,
    path: &Path,
    before: u64,
    magic_before: [u8; 4],
    snapshot: &[u8],
) -> Result<Outcome, Error> {
    if verify::magic_prefix(path)? != magic_before {
        restore(path, snapshot)?;
        return Ok(classify_outcome(false, before, before, None));
    }

    // INV-verify: prefer the backend's authoritative signal (btrfs FIEMAP ENCODED —
    // st_blocks reports the logical size there, so a real win is invisible to it).
    // Where the backend has no special signal (APFS/NTFS), fall back to the generic
    // allocated-bytes drop.
    let after = verify::on_disk_bytes(path)?;
    Ok(classify_outcome(
        true,
        before,
        after,
        backend.compressed_on_disk(path)?,
    ))
}

/// Map the post-apply facts to an Outcome. Pure (no I/O) so every branch is unit
/// testable: not loadable → Skipped(NotLoadable); else the backend's compression
/// signal (or, absent one, an allocated-bytes drop) decides Compressed vs NoGain.
fn classify_outcome(loadable: bool, before: u64, after: u64, signal: Option<bool>) -> Outcome {
    if !loadable {
        return Outcome::Skipped {
            reason: SkipReason::NotLoadable,
        };
    }
    if signal.unwrap_or(after < before) {
        Outcome::Compressed { before, after }
    } else {
        Outcome::NoGain { before, after }
    }
}

/// Map a backend I/O failure to a non-fatal `Skipped` reason, or `None` to let it
/// propagate as a hard error. Uses both `ErrorKind` (cross-platform, esp. Windows)
/// and the POSIX errno (stable across Linux/macOS), so it needs no newer-than-1.0
/// `ErrorKind` variants.
fn classify_skip(err: &std::io::Error) -> Option<SkipReason> {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return Some(SkipReason::PermissionDenied);
    }
    classify_errno(err.raw_os_error()?)
}

// Per-platform errno classification. Windows `raw_os_error()` is the Win32 space,
// which does NOT coincide with POSIX (e.g. 32 = SHARING_VIOLATION on Windows but
// EPIPE on unix), so the two maps are mutually exclusive by cfg.
#[cfg(not(windows))]
fn classify_errno(code: i32) -> Option<SkipReason> {
    match code {
        1 | 13 | 30 => Some(SkipReason::PermissionDenied), // EPERM/EACCES/EROFS
        16 | 26 => Some(SkipReason::Busy),                 // EBUSY/ETXTBSY
        27 => Some(SkipReason::TooLarge),                  // EFBIG
        _ => None,
    }
}
#[cfg(windows)]
fn classify_errno(code: i32) -> Option<SkipReason> {
    match code {
        5 | 19 => Some(SkipReason::PermissionDenied), // ACCESS_DENIED / WRITE_PROTECT
        32 | 33 => Some(SkipReason::Busy),            // SHARING_VIOLATION / LOCK_VIOLATION
        _ => None,
    }
}

/// One-pass guarded write of `content` to `path` as an OS-compressed file. Reached
/// only when the backend reported `Supported`. The backend writes the bytes AS the
/// file is created (decmpfs built from `content`, btrfs codec-then-write, NTFS
/// FSCTL-then-write) — no write-then-read-back. Fail-soft mirrors `apply_guarded`:
/// a permission/busy/too-large failure becomes a `Skipped` Outcome and the caller
/// is expected to fall back to a plain write; an unclassifiable I/O error
/// propagates. After a successful apply the kernel read-back is verified
/// byte-identical to `content` (the transparent-compression oracle), and the file
/// is restored to a plain write of `content` if it somehow doesn't match.
pub(crate) fn compress_bytes_guarded<B: Backend>(
    backend: &B,
    path: &Path,
    content: &[u8],
) -> Result<Outcome, Error> {
    if let Err(err) = backend.apply_bytes(path, content, None) {
        if let Error::Io { source, .. } = &err
            && let Some(reason) = classify_skip(source)
        {
            return Ok(Outcome::Skipped { reason });
        }
        return Err(err);
    }

    // Oracle: a normal read must hand back the exact bytes we asked to store.
    verify_readback_or_restore(backend, path, content)
}

/// Post-apply oracle for the one-pass path: a normal read must hand back exactly
/// `content`. If the backend produced something that doesn't decode identically,
/// restore a plain write of `content` and report `Skipped(IntegrityRevert)` so an
/// install is never left with a corrupt file; otherwise classify the win. Split
/// out so the mismatch-rollback is unit-testable without a backend that corrupts
/// the read-back (point it at a file whose bytes differ from `content`).
fn verify_readback_or_restore<B: Backend>(
    backend: &B,
    path: &Path,
    content: &[u8],
) -> Result<Outcome, Error> {
    let after = verify::on_disk_bytes(path)?;
    if !verify::readback_matches(path, content)? {
        restore(path, content)?;
        return Ok(Outcome::Skipped {
            reason: SkipReason::IntegrityRevert,
        });
    }

    let before = content.len() as u64;
    Ok(classify_outcome(
        true,
        before,
        after,
        backend.compressed_on_disk(path)?,
    ))
}

/// Atomic restore of the pre-apply bytes (sibling temp + rename). Returns `Err`
/// when the rollback itself fails (e.g. `ENOSPC`, a read-only dir) — the caller
/// MUST surface that as a hard error rather than a benign `Skipped`, else a
/// corrupted file is left on disk while the outcome reads as non-fatal.
fn restore(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| Error::Io {
        context: "rollback restore: path has no parent",
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })?;
    let tmp = dir.join(format!(
        ".aube-fscompress-restore-{}.tmp",
        std::process::id()
    ));
    let wrote = std::fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    wrote.map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io {
            context: "rollback restore",
            source,
        }
    })
}
