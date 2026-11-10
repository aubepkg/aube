// Platform backend and napi hybrid parsing code adapted from SocketDev/decmpfs v0.1.5.
// MIT-licensed; retained attribution and license are in compression/LICENSE.
// Aube-owned integration uses workspace zstd and local install/store contracts.
//! Aube-owned OS-compression backends for its opt-in native-addon CAS path. The
//! kernel transparently decompresses the stored bytes on reads.
//!
//! `compress_file(path)` detects the filesystem, applies compression, then verifies the
//! kernel reads the bytes back identically — rolling back on any failure. `probe(path)`
//! is the detect-only / capability-reporting half.
//!
//! Backends: btrfs (`FS_COMPR_FL` + the `btrfs.compression` property), NTFS
//! (`FSCTL_SET_COMPRESSION`), and the macOS APFS resource fork; other targets
//! use the plain-write fallback.
//!
//! Contract: every `Outcome` is a SUCCESS; `Err` is reserved for genuine I/O failures
//! that leave the file's integrity unknown. An unsupported FS, a permission/lock issue,
//! an incompressible or too-large file are non-fatal `Outcome`s.
//!
//! Panic-free invariant: the deny below keeps non-test code free of the obvious panic
//! sources; all slice indexing is length-guarded.

// Keep the source backend routines together so their safety and platform tests
// remain auditable. Aube currently calls only the gate, hybrid decoder, and
// one-pass writer; this module is private to aube-store.
#![allow(dead_code, unused_imports)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
// On a nightly `cargo llvm-cov` run, cargo-llvm-cov sets `coverage_nightly`,
// enabling `#[coverage(off)]` so test-only code is dropped from the report and it
// reflects PRODUCTION coverage. A no-op on stable (the cfg is unset), so ordinary
// builds and `cargo test` are unaffected.

use std::path::Path;

/// What happened to the file. Only `Err` is a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Applied and on-disk allocation actually decreased.
    Compressed { before: u64, after: u64 },
    /// Applied (or already set) but on-disk size did not drop — incompressible
    /// or sub-cluster. Content is byte-identical and fully loadable.
    NoGain { before: u64, after: u64 },
    /// Already carried the compression flag/xattr before we touched it.
    AlreadyCompressed { before: u64 },
    /// This FS/OS has no per-file transparent compression (ext4, xfs, ZFS, ReFS,
    /// FAT, tmpfs, overlay/network mounts). Caller falls through to the cache.
    Unsupported { reason: UnsupportedReason },
    /// Detected support but could not apply (permissions, lock, immutable,
    /// rollback). Warn-and-continue; never a hard error.
    Skipped { reason: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// Filesystem (by allowlist) has no transparent compression.
    Filesystem,
    /// Network/overlay/bind mount where the signal is unreliable.
    NetworkOrOverlay,
    /// Built for an OS with no backend (or skeleton: not yet implemented).
    PlatformBuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// EACCES / EPERM / EROFS — read-only or unowned (e.g. unprivileged container).
    PermissionDenied,
    /// A write handle is held / ETXTBSY / sharing violation; could not lock.
    Busy,
    /// UF_IMMUTABLE / SF_IMMUTABLE and we declined to toggle it.
    Immutable,
    /// EFS / FILE_ATTRIBUTE_ENCRYPTED.
    Encrypted,
    /// Applied, structural verification failed, rolled back to the original.
    IntegrityRevert,
    /// Post-apply loadability (magic-bytes) check failed, rolled back.
    NotLoadable,
    /// Exceeds a backend limit (e.g. decmpfs u32 offsets cap at 4 GiB).
    TooLarge,
    /// `compress_bytes` was handed a file the `Gate` excludes — written plain.
    GateExcluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Supported,
    AlreadyCompressed,
    Unsupported(UnsupportedReason),
}

/// Genuine failures only. A capability/permission gap is an `Outcome`, not an `Error`.
#[derive(Debug)]
pub enum Error {
    Io {
        context: &'static str,
        source: std::io::Error,
    },
    NotFound(std::path::PathBuf),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "io error at {context}: {source}"),
            Error::NotFound(p) => write!(f, "file not found: {}", p.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::NotFound(_) => None,
        }
    }
}

/// Wrap the last OS error with context — shared by every backend.
pub(crate) fn io(context: &'static str) -> Error {
    Error::Io {
        context,
        source: std::io::Error::last_os_error(),
    }
}

/// A NUL-checked C string from a path, for the unix backends that hand paths to
/// libc.
#[cfg(unix)]
pub(crate) fn cstring(path: &Path) -> Result<std::ffi::CString, Error> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Io {
        context: "path has interior NUL",
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })
}

/// Detect-only, no mutation — for dry-run / capability reporting.
pub fn probe(path: &Path) -> Result<Support, Error> {
    backend::detect(path)
}

/// THE entry point: detect → gate → apply → verify → rollback-on-failure.
/// Idempotent. Never panics. Never corrupts the file.
pub fn compress_file(path: &Path) -> Result<Outcome, Error> {
    compress_file_with(&Os, path)
}

/// `compress_file` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the otherwise-dead `AlreadyCompressed`/`Unsupported` arms
/// with a fake.
fn compress_file_with<B: Backend>(backend: &B, path: &Path) -> Result<Outcome, Error> {
    if !path.exists() {
        return Err(Error::NotFound(path.to_path_buf()));
    }
    match backend.detect(path)? {
        Support::Unsupported(reason) => Ok(Outcome::Unsupported { reason }),
        Support::AlreadyCompressed => Ok(Outcome::AlreadyCompressed {
            before: verify::on_disk_bytes(path)?,
        }),
        Support::Supported => safety::apply_guarded(backend, path),
    }
}

/// THE install-time entry point: write `content` to `path` as an OS-compressed file
/// in ONE pass — never a write-then-read-back-recompress.
///
/// The caller (a package manager's CAS writer) has already decoded the raw addon
/// and matched it against `gate`. `compress_bytes` writes that exact byte stream
/// directly as a transparently-compressed file: macOS encodes the decmpfs from the
/// bytes onto a fresh inode; btrfs requests the codec on the empty temp then writes;
/// NTFS sets FSCTL_SET_COMPRESSION on the fresh handle then writes.
///
/// Fail-soft is the contract — this NEVER breaks an install. On an unsupported FS,
/// a permission/busy/too-large skip, or any backend error, it falls back to a plain
/// atomic write of `content` and reports the corresponding `Outcome` (the plain
/// write still lands the file). The kernel read-back is verified identical to
/// `content` before returning a compressed Outcome.
///
/// `gate` is honored here as a convenience: if `content` does not match the gate,
/// the file is written plain and `Outcome::Skipped { reason: GateExcluded }` is
/// returned. A caller that already gated can pass `&Gate::any()`.
pub fn compress_bytes(path: &Path, content: &[u8], gate: &Gate) -> Result<Outcome, Error> {
    compress_bytes_with(&Os, path, content, gate)
}

/// `compress_bytes` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the plain-write fallback arms (a guarded skip/error, or a
/// non-compressing FS) that a real APFS write never reaches.
fn compress_bytes_with<B: Backend>(
    backend: &B,
    path: &Path,
    content: &[u8],
    gate: &Gate,
) -> Result<Outcome, Error> {
    let name = path.to_string_lossy();
    let normalized = name.replace('\\', "/");
    if !gate.matches(&normalized, content.len() as u64) {
        plain_write(path, content)?;
        return Ok(Outcome::Skipped {
            reason: SkipReason::GateExcluded,
        });
    }
    // The target usually doesn't exist yet (a fresh CAS write), so the FS capability
    // probe goes against the parent directory; `detect` statfs's / opens its argument
    // and would error on a missing path.
    let probe_target = if path.exists() {
        path.to_path_buf()
    } else {
        match path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => path.to_path_buf(),
        }
    };
    match backend.detect(&probe_target) {
        Ok(Support::Supported) => match safety::compress_bytes_guarded(backend, path, content) {
            Ok(Outcome::Skipped { .. }) | Err(_) => {
                // A guarded skip/error already restored or never wrote — ensure the file
                // lands plain so the install is never missing the addon.
                plain_write(path, content)?;
                Ok(Outcome::Skipped {
                    reason: SkipReason::IntegrityRevert,
                })
            }
            other => other,
        },
        Ok(Support::AlreadyCompressed) | Ok(Support::Unsupported(_)) | Err(_) => {
            plain_write(path, content)?;
            Ok(Outcome::Unsupported {
                reason: UnsupportedReason::Filesystem,
            })
        }
    }
}

/// What a [`copy_file`] did — a SUCCESS shape, same contract as [`Outcome`]:
/// `Err` is reserved for genuine I/O failures; the copy itself always lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// Copy-on-write clone (`clonefile` / `FICLONE`) — the extents are shared,
    /// so the source's compression state carries over at zero cost.
    Cloned { compressed: bool },
    /// Byte copy plus one-pass recompression at the destination (a compressed
    /// source that could not be cloned — cross-volume, non-reflink FS).
    CopiedCompressed { before: u64, after: u64 },
    /// Byte copy landed plain: the source was plain, the destination FS has no
    /// transparent compression, or a fail-soft skip (`skipped` carries the
    /// reason when the source WAS compressed but the state could not follow).
    CopiedPlain { skipped: Option<SkipReason> },
}

/// Attempt a copy-on-write clone of `src` at `dest` (`clonefile(2)` on macOS,
/// the `FICLONE` ioctl on Linux) — the zero-cost way to copy a compressed file
/// WITH its compression. `Ok(true)` = cloned; `Ok(false)` = this pairing can't
/// clone (cross-volume, non-reflink FS, an existing destination on macOS,
/// Windows) and the caller decides the fallback — [`copy_file`] is the
/// clone-then-fallback composition, and a Node-`COPYFILE_FICLONE_FORCE`-shaped
/// caller turns `false` into its error.
pub fn try_clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
    if !src.exists() {
        return Err(Error::NotFound(src.to_path_buf()));
    }
    Os.clone_file(src, dest)
}

/// Copy `src` to `dest` preserving transparent filesystem compression — the
/// `fs.copyFile` the OS never shipped. A plain byte copy silently re-inflates
/// a compressed file (the kernel hands every reader the full logical bytes,
/// and that is what gets written back out); this copy keeps the on-disk
/// savings.
///
/// Strategy, in order:
/// 1. A copy-on-write clone (`clonefile(2)` on macOS, the `FICLONE` ioctl on
///    Linux) shares the extents, so a compressed source stays compressed at
///    zero cost — and a plain source clones plain.
/// 2. When cloning is impossible (cross-volume, non-reflink FS, Windows), the
///    logical bytes are copied and, if the source was compressed, the
///    destination is written back compressed via the same guarded one-pass
///    path as [`compress_bytes`].
/// 3. A plain source is copied plain — a copy never changes compression state.
///
/// `fs.copyFile` parity: an existing `dest` is replaced, and the source's
/// permission bits carry over. Fail-soft like the rest of the crate: on any
/// backend skip the plain copy still stands, reported in the outcome.
pub fn copy_file(src: &Path, dest: &Path) -> Result<CopyOutcome, Error> {
    copy_file_with(&Os, src, dest)
}

/// `copy_file` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the clone and fallback arms with fakes.
fn copy_file_with<B: Backend>(backend: &B, src: &Path, dest: &Path) -> Result<CopyOutcome, Error> {
    if !src.exists() {
        return Err(Error::NotFound(src.to_path_buf()));
    }
    if dest.exists() {
        // A copy onto itself (same path, a hardlink, a symlink alias) is a no-op
        // success — otherwise the replace step below would clobber the copy's own
        // source. The extents are 100% shared, which is what `Cloned` states.
        if is_same_file(src, dest) {
            return Ok(CopyOutcome::Cloned {
                compressed: backend.is_already_compressed(src).unwrap_or(false),
            });
        }
        // clonefile refuses an existing destination; replace-by-default is the
        // `fs.copyFile` contract this mirrors.
        std::fs::remove_file(dest).map_err(|source| Error::Io {
            context: "replace existing destination",
            source,
        })?;
    }
    // On a filesystem with no compression signal this is a capability gap, not a
    // failure — treat it as "plain source" and keep copying.
    let compressed_src = backend.is_already_compressed(src).unwrap_or(false);
    if backend.clone_file(src, dest)? {
        return Ok(CopyOutcome::Cloned {
            compressed: compressed_src,
        });
    }
    // A normal read hands back the full logical bytes regardless of the
    // source's on-disk representation.
    let content = std::fs::read(src).map_err(|source| Error::Io {
        context: "read copy source",
        source,
    })?;
    let mode = std::fs::metadata(src).ok().map(|meta| meta.permissions());
    if !compressed_src {
        plain_write(dest, &content)?;
        if let Some(mode) = mode {
            let _ = std::fs::set_permissions(dest, mode);
        }
        return Ok(CopyOutcome::CopiedPlain { skipped: None });
    }
    let outcome = compress_bytes_with(backend, dest, &content, &Gate::any())?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(dest, mode);
    }
    Ok(match outcome {
        Outcome::Compressed { before, after } | Outcome::NoGain { before, after } => {
            CopyOutcome::CopiedCompressed { before, after }
        }
        // Unreachable from a fresh destination (compress_bytes maps an
        // already-compressed detect to a plain write), kept total + truthful.
        Outcome::AlreadyCompressed { before } => CopyOutcome::CopiedCompressed {
            before,
            after: before,
        },
        Outcome::Unsupported { .. } => CopyOutcome::CopiedPlain { skipped: None },
        Outcome::Skipped { reason } => CopyOutcome::CopiedPlain {
            skipped: Some(reason),
        },
    })
}

/// True when both paths name the same underlying file — same dev+inode on
/// unix, or same volume serial + file index on Windows. Guards [`copy_file`]'s
/// replace-by-default from removing its own source.
#[cfg(unix)]
fn is_same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(meta_a), Ok(meta_b)) => meta_a.dev() == meta_b.dev() && meta_a.ino() == meta_b.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn is_same_file(a: &Path, b: &Path) -> bool {
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
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(canon_a), Ok(canon_b)) => canon_a == canon_b,
        _ => false,
    }
}

/// Fail-soft plain atomic write: sibling temp + fsync + rename. The never-break-the
/// -install floor under every `compress_bytes` fallback.
fn plain_write(path: &Path, content: &[u8]) -> Result<(), Error> {
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
    temp.persist(path).map_err(|error| Error::Io {
        context: "plain write rename",
        source: error.error,
    })?;
    Ok(())
}

/// Filesystem-compression state of a path — one call that coalesces the
/// otherwise-separate size + backend-signal reads (the compress/copy paths
/// previously did a `stat` AND an `lstat`/attr read per file). Follows symlinks:
/// compression is a property of the target file, never a symlink.
pub struct Stat {
    /// FS-compressed on disk. Uses the backend's authoritative signal where it has
    /// one (`UF_COMPRESSED` on APFS, FIEMAP-encoded extents on btrfs, the
    /// compressed attribute on NTFS); elsewhere inferred from allocated < logical.
    pub compressed: bool,
    /// Apparent (logical) size — constant whether or not the file is compressed.
    pub logical: u64,
    /// Allocated (physical) bytes on disk — where the compression win shows.
    pub physical: u64,
}

/// Inspect the FS-compression state of `path` (see [`Stat`]).
pub fn stat(path: &Path) -> Result<Stat, Error> {
    stat_with(&Os, path)
}

/// [`stat`] over an injectable [`Backend`] so the no-signal (allocated-bytes)
/// inference arm is testable without a real filesystem.
fn stat_with<B: Backend>(backend: &B, path: &Path) -> Result<Stat, Error> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat",
        source,
    })?;
    let logical = meta.len();
    // One metadata read yields both size + allocation on unix (the coalesce);
    // Windows needs GetCompressedFileSizeW for the post-compression allocation.
    #[cfg(unix)]
    let physical = {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    };
    #[cfg(not(unix))]
    let physical = verify::on_disk_bytes(path)?;
    // Prefer the backend's authoritative signal; fall back to the
    // allocated-vs-logical inference when there is no signal (e.g. NTFS) OR the
    // probe isn't supported on this filesystem (e.g. FIEMAP on tmpfs) — a stat is
    // an inspection and must never fail over a best-effort compression check.
    let compressed = match backend.compressed_on_disk(path) {
        Ok(Some(signal)) => signal,
        Ok(None) | Err(_) => logical > 0 && physical < logical,
    };
    Ok(Stat {
        compressed,
        logical,
        physical,
    })
}

pub mod addon;
mod gate;
mod safety;
mod verify;

pub use gate::{DEFAULT_GLOB, Gate};

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod backend;

/// The OS compression backend as a trait, so the orchestration in `safety` can be
/// driven by a fake in tests — a real filesystem never produces a non-loadable
/// result or a mismatched read-back, so the rollback and plain-write fallback paths
/// are otherwise unreachable. Production always threads [`Os`]; static dispatch
/// monomorphizes it to the same code as a direct backend call (no vtable, no size
/// cost in a release build).
pub(crate) trait Backend {
    fn detect(&self, path: &Path) -> Result<Support, Error>;
    fn is_already_compressed(&self, path: &Path) -> Result<bool, Error>;
    /// Compress `path` in place. `snapshot` is the already-read file contents (the
    /// caller holds them for rollback); backends that rewrite via temp+rename reuse
    /// it instead of reading the file a second time, and backends that flag the
    /// existing file in place (Windows) ignore it.
    fn apply_inplace(&self, path: &Path, snapshot: &[u8]) -> Result<(), Error>;
    fn apply_bytes(
        &self,
        path: &Path,
        content: &[u8],
        mode: Option<std::fs::Permissions>,
    ) -> Result<(), Error>;
    fn compressed_on_disk(&self, path: &Path) -> Result<Option<bool>, Error>;
    /// Copy-on-write clone. `Ok(false)` = "cannot clone here" → the caller falls
    /// back to a byte copy. Defaulted so fakes without a clone path exercise the
    /// fallback arms.
    fn clone_file(&self, _src: &Path, _dest: &Path) -> Result<bool, Error> {
        Ok(false)
    }
}

/// The real, cfg-selected OS backend.
pub(crate) struct Os;

impl Backend for Os {
    fn detect(&self, path: &Path) -> Result<Support, Error> {
        backend::detect(path)
    }
    fn is_already_compressed(&self, path: &Path) -> Result<bool, Error> {
        backend::is_already_compressed(path)
    }
    fn apply_inplace(&self, path: &Path, snapshot: &[u8]) -> Result<(), Error> {
        backend::apply_inplace(path, snapshot)
    }
    fn apply_bytes(
        &self,
        path: &Path,
        content: &[u8],
        mode: Option<std::fs::Permissions>,
    ) -> Result<(), Error> {
        backend::apply_bytes(path, content, mode)
    }
    fn compressed_on_disk(&self, path: &Path) -> Result<Option<bool>, Error> {
        backend::compressed_on_disk(path)
    }
    fn clone_file(&self, src: &Path, dest: &Path) -> Result<bool, Error> {
        backend::clone_file(src, dest)
    }
}
