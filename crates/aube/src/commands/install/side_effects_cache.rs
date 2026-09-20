use miette::{Context, IntoDiagnostic, miette};
use sha2::Digest;

#[cfg(test)]
const SIDE_EFFECTS_CACHE_MARKER: &str = ".aube-side-effects-cache";
/// Names the listing every published entry carries of its own contents.
///
/// An entry is published atomically, but nothing stops an external cleaner from
/// emptying one afterwards — mise's cache prune walked into one through a
/// symlink and unlinked files by age. Restoring from a half-emptied entry would
/// install a package missing files and record it as built, so an entry is
/// checked against this listing before it is used.
/// Names the cache root. Entries carry their payload and listing in a shape
/// `v1` entries do not have, and an older aube keeps reading `v1`, so the two
/// live side by side rather than one misreading the other. A cache pre-warmed
/// by an older aube is not read after an upgrade and has to be filled again.
const SIDE_EFFECTS_CACHE_DIR: &str = "side-effects-v2";
const SIDE_EFFECTS_CACHE_ENTRY_MANIFEST: &str = ".aube-side-effects-entry";
/// Holds the built package inside an entry, so nothing a package ships can
/// collide with the listing that sits beside it.
const SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD: &str = "payload";
const SIDE_EFFECTS_CACHE_RESTORE_PREFIX: &str = ".tmp-side-effects-restore-";
/// Distinguishes working directories a single process asks for. The clock alone
/// does not: its granularity is coarse on some platforms, and two names taken
/// within one tick would collide — a restore would then stage into, and delete,
/// the directory it was about to move the package into.
static WORKING_DIR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn working_dir_name(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        WORKING_DIR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
const SIDE_EFFECTS_CACHE_TMP_PREFIX: &str = ".tmp-side-effects-";
const SIDE_EFFECTS_CACHE_TMP_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy)]
pub(crate) enum SideEffectsCacheConfig<'a> {
    Disabled,
    RestoreOnly(&'a std::path::Path),
    RestoreAndSave(&'a std::path::Path),
    SaveOnlyOverwrite(&'a std::path::Path),
}

impl<'a> SideEffectsCacheConfig<'a> {
    pub(super) fn root(self) -> Option<&'a std::path::Path> {
        match self {
            Self::Disabled => None,
            Self::RestoreOnly(root)
            | Self::RestoreAndSave(root)
            | Self::SaveOnlyOverwrite(root) => Some(root),
        }
    }

    pub(super) fn should_restore(self) -> bool {
        matches!(self, Self::RestoreOnly(_) | Self::RestoreAndSave(_))
    }

    pub(super) fn overwrite_existing(self) -> bool {
        matches!(self, Self::SaveOnlyOverwrite(_))
    }

    pub(super) fn should_save(self) -> bool {
        matches!(self, Self::RestoreAndSave(_) | Self::SaveOnlyOverwrite(_))
    }
}

#[derive(Debug, Clone)]
pub(super) struct SideEffectsCacheEntry {
    already_applied: bool,
    input_hash: String,
    marker_path: std::path::PathBuf,
    path: std::path::PathBuf,
}

struct SideEffectsMarker {
    input_hash: String,
    output_hash: String,
}

pub(super) enum SideEffectsCacheRestore {
    Miss,
    Restored,
    AlreadyApplied,
}

impl SideEffectsCacheEntry {
    pub(super) fn new(
        root: &std::path::Path,
        name: &str,
        version: &str,
        package_dir: &std::path::Path,
    ) -> miette::Result<Self> {
        let marker_path = side_effects_marker_path(package_dir, name)?;
        let current_hash = hash_dir_for_side_effects_cache(package_dir)?;
        let marker = read_valid_side_effects_marker(&marker_path);
        let already_applied = marker
            .as_ref()
            .is_some_and(|marker| marker.output_hash == current_hash);
        let input_hash = match marker {
            Some(marker) if already_applied || marker.input_hash == current_hash => {
                marker.input_hash
            }
            _ => current_hash,
        };
        let safe_name = name.replace('/', "__");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        Ok(Self {
            already_applied,
            path: root
                .join(format!("{safe_name}@{version}"))
                .join(platform)
                .join(&input_hash),
            input_hash,
            marker_path,
        })
    }

    pub(super) fn restore_if_available(
        &self,
        package_dir: &std::path::Path,
    ) -> miette::Result<SideEffectsCacheRestore> {
        // The installer-owned marker sits outside package content and records
        // both the pre-build input and post-build output hashes. A swept
        // reusable cache therefore does not invalidate an intact build, while
        // missing or modified generated output still forces restore/rebuild.
        if self.already_applied {
            tracing::debug!(
                "side-effects-cache: already applied {}",
                self.path.display()
            );
            return Ok(SideEffectsCacheRestore::AlreadyApplied);
        }
        if !self.path.is_dir() {
            return Ok(SideEffectsCacheRestore::Miss);
        }
        let payload = self.path.join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD);
        if !payload.is_dir() {
            self.discard("no payload");
            return Ok(SideEffectsCacheRestore::Miss);
        }
        // An entry published before entries carried a listing, or one whose
        // listing an outside cleaner took, cannot be vouched for.
        let Some(published) = read_entry_manifest(&self.path) else {
            self.discard("no listing");
            return Ok(SideEffectsCacheRestore::Miss);
        };

        // Staged beside the package directory rather than over it: what gets
        // checked is then exactly what gets installed, so an entry emptied
        // while this restore runs cannot slip past the check, and a rejected
        // entry leaves the package directory as the extraction left it.
        if let Some(parent) = package_dir.parent() {
            sweep_stale_tmp_dirs(parent, SIDE_EFFECTS_CACHE_RESTORE_PREFIX);
        }
        let staged = restore_staging_dir(package_dir)?;
        if let Err(err) = copy_dir(&payload, &staged, CopyMode::HardlinkOrCopy) {
            // Whatever went wrong reading the entry, rebuilding is the answer.
            tracing::debug!(
                "side-effects-cache: could not stage {}: {err}",
                self.path.display()
            );
            let _ = std::fs::remove_dir_all(&staged);
            return Ok(SideEffectsCacheRestore::Miss);
        }
        if entry_manifest(&staged).ok().as_deref() != Some(published.as_str()) {
            let _ = std::fs::remove_dir_all(&staged);
            self.discard("incomplete");
            return Ok(SideEffectsCacheRestore::Miss);
        }

        install_staged_restore(&staged, package_dir).wrap_err_with(|| {
            format!(
                "failed to restore side effects cache from {}",
                self.path.display()
            )
        })?;
        self.write_marker(package_dir)?;
        tracing::debug!("side-effects-cache: restored {}", self.path.display());
        Ok(SideEffectsCacheRestore::Restored)
    }

    /// Drops an entry that cannot be used. A failure to remove it is not fatal:
    /// `save` republishes over an entry that does not check out.
    fn discard(&self, why: &str) {
        tracing::debug!(
            "side-effects-cache: discarding {} entry {}",
            why,
            self.path.display()
        );
        if let Err(err) = std::fs::remove_dir_all(&self.path) {
            tracing::debug!(
                "side-effects-cache: could not remove {}: {err}",
                self.path.display()
            );
        }
    }

    pub(super) fn save(
        &self,
        package_dir: &std::path::Path,
        overwrite_existing: bool,
    ) -> miette::Result<()> {
        if self.path.is_dir() {
            // An entry that no longer checks out is replaced even when
            // overwriting is off. Keeping it would leave every later install
            // rebuilding against an entry none of them can ever use.
            if overwrite_existing || !entry_is_intact(&self.path) {
                std::fs::remove_dir_all(&self.path)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to remove {}", self.path.display()))?;
            } else {
                self.write_marker(package_dir)?;
                return Ok(());
            }
        }
        let parent = self.path.parent().ok_or_else(|| {
            miette!(
                "invalid side effects cache path has no parent: {}",
                self.path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        sweep_stale_tmp_dirs(parent, SIDE_EFFECTS_CACHE_TMP_PREFIX);
        self.write_marker(package_dir)?;

        let tmp = parent.join(working_dir_name(SIDE_EFFECTS_CACHE_TMP_PREFIX));
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to remove {}", tmp.display()))?;
        }
        copy_dir(
            package_dir,
            &tmp.join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD),
            CopyMode::Copy,
        )
        .wrap_err_with(|| {
            format!(
                "failed to write side effects cache into {}",
                self.path.display()
            )
        })?;
        // Written before the rename publishes `tmp`, so an entry never exists
        // without the listing it is checked against.
        write_entry_manifest(&tmp)?;
        match aube_util::fs_atomic::rename_with_retry(&tmp, &self.path) {
            Ok(()) => {
                tracing::debug!("side-effects-cache: saved {}", self.path.display());
                Ok(())
            }
            Err(e) if self.path.is_dir() => {
                tracing::debug!(
                    "side-effects-cache: cache appeared while saving {}: {e}",
                    self.path.display()
                );
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(e)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to publish {}", self.path.display()))
            }
        }
    }

    fn write_marker(&self, package_dir: &std::path::Path) -> miette::Result<()> {
        let output_hash = hash_dir_for_side_effects_cache(package_dir)?;
        write_side_effects_marker(&self.marker_path, &self.input_hash, &output_hash)
    }
}

/// Clears working directories an earlier run left behind. Both the staging a
/// save publishes from and the staging a restore swaps in are whole package
/// trees, so a run cut short — or a filesystem that refused a cleanup — must
/// not leave them to pile up.
fn sweep_stale_tmp_dirs(parent: &std::path::Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if should_remove_stale_tmp_dir(&entry, prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn should_remove_stale_tmp_dir(entry: &std::fs::DirEntry, prefix: &str) -> bool {
    if !entry.file_name().to_string_lossy().starts_with(prefix) {
        return false;
    }
    entry
        .metadata()
        .and_then(|m| m.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age >= SIDE_EFFECTS_CACHE_TMP_STALE_AFTER)
}

pub(crate) fn side_effects_cache_root(store: &aube_store::Store) -> std::path::PathBuf {
    let virtual_store_dir = store.virtual_store_dir();
    let virtual_store_root = if virtual_store_dir.file_name()
        == Some(std::ffi::OsStr::new(
            crate::commands::settings_context::GVS_REGISTRY_NAMESPACE_VERSION,
        )) {
        virtual_store_dir
            .parent()
            .unwrap_or(virtual_store_dir.as_path())
    } else {
        virtual_store_dir.as_path()
    };
    virtual_store_root
        .parent()
        .unwrap_or_else(|| store.root())
        .join(SIDE_EFFECTS_CACHE_DIR)
}

fn side_effects_marker_path(
    package_dir: &std::path::Path,
    name: &str,
) -> miette::Result<std::path::PathBuf> {
    let parent = package_dir.parent().ok_or_else(|| {
        miette!(
            "package directory has no parent for side effects marker: {}",
            package_dir.display()
        )
    })?;
    let name_hash = sha2::Sha256::digest(name.as_bytes());
    Ok(parent.join(format!(
        ".aube-side-effects-cache-{}",
        hex::encode(name_hash)
    )))
}

fn read_valid_side_effects_marker(marker_path: &std::path::Path) -> Option<SideEffectsMarker> {
    let marker = std::fs::read_to_string(marker_path).ok()?;
    let mut lines = marker.lines();
    let version = lines.next()?;
    let input_hash = lines.next()?;
    let output_hash = lines.next()?;
    if version != "v1"
        || lines.next().is_some()
        || !is_side_effects_cache_hash(input_hash)
        || !is_side_effects_cache_hash(output_hash)
    {
        return None;
    }
    Some(SideEffectsMarker {
        input_hash: input_hash.to_ascii_lowercase(),
        output_hash: output_hash.to_ascii_lowercase(),
    })
}

fn is_side_effects_cache_hash(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_side_effects_marker(
    marker_path: &std::path::Path,
    input_hash: &str,
    output_hash: &str,
) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(
        marker_path,
        format!("v1\n{input_hash}\n{output_hash}\n").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err_with(|| {
        format!(
            "failed to write side effects cache marker {}",
            marker_path.display()
        )
    })
}

/// Lists a payload's contents: one line per path, sorted, naming what the path
/// is and enough about it to notice a file that has been removed or truncated.
///
/// Contents are not read. This answers whether the payload is still whole,
/// which is what an outside cleaner takes away; it is not an integrity check
/// against deliberate tampering.
///
/// Every field is escaped, so no two different trees can describe themselves
/// with the same text: a name holding a space or a newline cannot be made to
/// read as the end of one field and the start of another.
fn entry_manifest(payload: &std::path::Path) -> miette::Result<String> {
    let mut lines = vec!["v1".to_string()];
    entry_manifest_inner(payload, payload, &mut lines)?;
    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn escape_manifest_field(field: &str) -> String {
    let mut escaped = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            ' ' => escaped.push_str("\\s"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn entry_manifest_inner(
    base: &std::path::Path,
    current: &std::path::Path,
    lines: &mut Vec<String>,
) -> miette::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(current)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to relativize {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let rel = escape_manifest_field(&rel);
        let meta = std::fs::symlink_metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read symlink {}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            lines.push(format!("l {} {rel}", escape_manifest_field(&target)));
        } else if meta.is_dir() {
            lines.push(format!("d {rel}"));
            entry_manifest_inner(base, &path, lines)?;
        } else if meta.is_file() {
            lines.push(format!("f {} {rel}", meta.len()));
        }
    }
    Ok(())
}

/// Writes the listing beside the payload, in the staging directory the rename
/// has not published yet. The path is aube's own, never one a package supplied.
fn write_entry_manifest(entry: &std::path::Path) -> miette::Result<()> {
    let manifest = entry_manifest(&entry.join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD))?;
    let path = entry.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST);
    std::fs::write(&path, manifest)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", path.display()))
}

fn read_entry_manifest(entry: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(entry.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST)).ok()
}

/// Whether the entry still holds everything it was published with.
fn entry_is_intact(entry: &std::path::Path) -> bool {
    let Some(published) = read_entry_manifest(entry) else {
        return false;
    };
    entry_manifest(&entry.join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD))
        .is_ok_and(|current| current == published)
}

/// A private directory beside the package directory, on the same filesystem so
/// the restored tree can be renamed into place and so hardlinks survive.
fn restore_staging_dir(package_dir: &std::path::Path) -> miette::Result<std::path::PathBuf> {
    let parent = package_dir
        .parent()
        .ok_or_else(|| miette!("package directory has no parent: {}", package_dir.display()))?;
    std::fs::create_dir_all(parent)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    Ok(parent.join(working_dir_name(SIDE_EFFECTS_CACHE_RESTORE_PREFIX)))
}

/// Swaps the checked tree in for the package directory, keeping the old one
/// until the swap has gone through.
fn install_staged_restore(
    staged: &std::path::Path,
    package_dir: &std::path::Path,
) -> miette::Result<()> {
    let replaced = restore_staging_dir(package_dir)?;
    let had_package = package_dir.symlink_metadata().is_ok();
    if had_package && let Err(err) = std::fs::rename(package_dir, &replaced) {
        let _ = std::fs::remove_dir_all(staged);
        return Err(err)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to move aside {}", package_dir.display()));
    }
    if let Err(err) = std::fs::rename(staged, package_dir) {
        if had_package {
            let _ = std::fs::rename(&replaced, package_dir);
        }
        let _ = std::fs::remove_dir_all(staged);
        return Err(err)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to move into {}", package_dir.display()));
    }
    if had_package && let Err(err) = std::fs::remove_dir_all(&replaced) {
        // Left for the sweep at the start of the next restore.
        tracing::debug!(
            "side-effects-cache: could not remove {}: {err}",
            replaced.display()
        );
    }
    Ok(())
}

fn hash_dir_for_side_effects_cache(package_dir: &std::path::Path) -> miette::Result<String> {
    let mut hasher = sha2::Sha512::new();
    hash_dir_inner(package_dir, package_dir, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn hash_dir_inner(
    base: &std::path::Path,
    current: &std::path::Path,
    hasher: &mut sha2::Sha512,
) -> miette::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(current)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to relativize {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
        hasher.update(rel.as_bytes());
        if meta.file_type().is_symlink() {
            hasher.update(b"\0symlink\0");
            let target = std::fs::read_link(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read symlink {}", path.display()))?;
            hasher.update(target.to_string_lossy().as_bytes());
        } else if meta.is_dir() {
            hasher.update(b"\0dir\0");
            hash_dir_inner(base, &path, hasher)?;
        } else if meta.is_file() {
            hasher.update(b"\0file\0");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hasher.update((meta.permissions().mode() & 0o7777).to_le_bytes());
            }
            let bytes = std::fs::read(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            hasher.update(bytes);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum CopyMode {
    Copy,
    HardlinkOrCopy,
}

pub(super) fn copy_dir(
    src: &std::path::Path,
    dst: &std::path::Path,
    mode: CopyMode,
) -> miette::Result<()> {
    if dst.symlink_metadata().is_ok() {
        remove_path(dst)?;
    }
    std::fs::create_dir_all(dst)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", dst.display()))?;
    copy_dir_inner(src, src, dst, mode)
}

fn copy_dir_inner(
    base: &std::path::Path,
    current: &std::path::Path,
    dst_root: &std::path::Path,
    mode: CopyMode,
) -> miette::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(current)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to relativize {}", path.display()))?;
        let dst = dst_root.join(rel);
        let meta = std::fs::symlink_metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            create_symlink_like(&path, &dst, meta.file_type())?;
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dst)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create {}", dst.display()))?;
            copy_dir_inner(base, &path, dst_root, mode)?;
        } else if meta.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            match mode {
                CopyMode::Copy => {
                    std::fs::copy(&path, &dst)
                        .into_diagnostic()
                        .wrap_err_with(|| format!("failed to copy {}", dst.display()))?;
                }
                CopyMode::HardlinkOrCopy => {
                    if let Err(e) = std::fs::hard_link(&path, &dst) {
                        tracing::debug!(
                            "side-effects-cache: hardlink failed for {} -> {}: {e}; copying",
                            path.display(),
                            dst.display()
                        );
                        std::fs::copy(&path, &dst)
                            .into_diagnostic()
                            .wrap_err_with(|| format!("failed to copy {}", dst.display()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_path(path: &std::path::Path) -> miette::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .into_diagnostic()
    .wrap_err_with(|| format!("failed to remove {}", path.display()))
}

#[cfg(unix)]
fn create_symlink_like(
    src: &std::path::Path,
    dst: &std::path::Path,
    _file_type: std::fs::FileType,
) -> miette::Result<()> {
    let target = std::fs::read_link(src)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read symlink {}", src.display()))?;
    std::os::unix::fs::symlink(&target, dst)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to symlink {}", dst.display()))
}

#[cfg(windows)]
fn create_symlink_like(
    src: &std::path::Path,
    dst: &std::path::Path,
    file_type: std::fs::FileType,
) -> miette::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let target = std::fs::read_link(src)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read symlink {}", src.display()))?;
    if file_type.is_symlink_dir() {
        aube_linker::create_dir_link(&target, dst)
    } else {
        std::os::windows::fs::symlink_file(&target, dst)
    }
    .into_diagnostic()
    .wrap_err_with(|| format!("failed to symlink {}", dst.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_is_a_sibling_of_the_versioned_virtual_store_root() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let store = aube_store::Store::with_dirs(dir.path().join("store/files"), cache_dir.clone())
            .with_virtual_store_dir(cache_dir.join("virtual-store/v1"));

        assert_eq!(
            side_effects_cache_root(&store),
            cache_dir.join(SIDE_EFFECTS_CACHE_DIR)
        );
    }

    #[test]
    fn cache_path_segregates_by_platform() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        let entry = SideEffectsCacheEntry::new(dir.path(), "p", "1.0.0", &pkg).unwrap();
        let s = entry.path.to_string_lossy().into_owned();
        let segment = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        assert!(
            s.contains(&segment),
            "cache path lacks platform segment {segment}: {s}"
        );
    }

    #[test]
    fn side_effects_marker_accepts_only_sha512_hex() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join(SIDE_EFFECTS_CACHE_MARKER);

        std::fs::write(&marker_path, "../../evil").unwrap();
        assert!(read_valid_side_effects_marker(&marker_path).is_none());

        std::fs::write(
            &marker_path,
            format!("v1\n{}\n{}\n", "A".repeat(128), "B".repeat(128)),
        )
        .unwrap();
        let marker = read_valid_side_effects_marker(&marker_path).unwrap();
        assert_eq!(marker.input_hash, "a".repeat(128));
        assert_eq!(marker.output_hash, "b".repeat(128));
    }

    #[test]
    fn applied_marker_survives_reusable_cache_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();

        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        std::fs::write(pkg.join("built.node"), "built").unwrap();
        entry.save(&pkg, false).unwrap();
        std::fs::remove_dir_all(&cache).unwrap();

        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        assert!(!entry.path.exists());
        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::AlreadyApplied
        ));
    }

    /// Builds a package, publishes the result, then returns `pkg` to the state a
    /// fresh extraction leaves it in and hands back the entry the next install
    /// would look up.
    fn published_entry(cache: &std::path::Path, pkg: &std::path::Path) -> SideEffectsCacheEntry {
        std::fs::create_dir_all(pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        let entry = SideEffectsCacheEntry::new(cache, "p", "1.0.0", pkg).unwrap();

        std::fs::create_dir_all(pkg.join("build")).unwrap();
        std::fs::write(pkg.join("build/built.node"), "built").unwrap();
        entry.save(pkg, false).unwrap();

        std::fs::remove_dir_all(pkg.join("build")).unwrap();
        std::fs::remove_file(side_effects_marker_path(pkg, "p").unwrap()).unwrap();
        let next = SideEffectsCacheEntry::new(cache, "p", "1.0.0", pkg).unwrap();
        assert_eq!(next.path, entry.path, "the next install looks elsewhere");
        next
    }

    #[test]
    fn a_published_entry_restores() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);

        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Restored
        ));
        assert!(pkg.join("build/built.node").exists());
    }

    #[test]
    fn an_entry_emptied_by_an_outside_cleaner_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);

        // What a cache pruner does: unlink a file, leave the tree standing.
        std::fs::remove_file(
            entry
                .path
                .join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD)
                .join("build/built.node"),
        )
        .unwrap();

        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
        assert!(
            !entry.path.exists(),
            "an incomplete entry was left for the next install to hit"
        );
        assert!(
            !pkg.join("build/built.node").exists(),
            "the package was restored from an incomplete entry"
        );
    }

    #[test]
    fn an_entry_without_a_manifest_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);

        // Stands in for an entry published before entries carried a manifest.
        std::fs::remove_file(entry.path.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST)).unwrap();

        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
        assert!(!entry.path.exists());
    }

    #[test]
    fn the_manifest_stays_out_of_the_restored_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);

        assert!(entry.path.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST).exists());
        entry.restore_if_available(&pkg).unwrap();

        assert!(!pkg.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_removed_symlink_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(pkg.join("packages/inner")).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        // What a postinstall does: link a workspace directory by absolute path.
        std::os::unix::fs::symlink(pkg.join("packages/inner"), pkg.join("linked")).unwrap();
        entry.save(&pkg, false).unwrap();

        std::fs::remove_file(
            entry
                .path
                .join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD)
                .join("linked"),
        )
        .unwrap();

        assert!(!entry_is_intact(&entry.path));
    }

    /// The listing sits beside the payload, so a package is free to ship a file
    /// of the same name.
    #[test]
    fn a_package_file_named_like_the_listing_survives_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        std::fs::write(pkg.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST), "package owned").unwrap();
        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();

        std::fs::write(pkg.join("built.node"), "built").unwrap();
        entry.save(&pkg, false).unwrap();

        std::fs::remove_file(pkg.join("built.node")).unwrap();
        std::fs::remove_file(side_effects_marker_path(&pkg, "p").unwrap()).unwrap();
        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Restored
        ));

        assert_eq!(
            std::fs::read_to_string(pkg.join(SIDE_EFFECTS_CACHE_ENTRY_MANIFEST)).unwrap(),
            "package owned"
        );
    }

    /// An entry an older aube wrote lives under the previous cache root, so it
    /// is never read as a v2 entry rather than misread as a damaged one.
    #[test]
    fn an_older_cache_root_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let store = aube_store::Store::with_dirs(dir.path().join("store/files"), cache_dir.clone())
            .with_virtual_store_dir(cache_dir.join("virtual-store/v1"));
        let previous = cache_dir.join("side-effects-v1");
        std::fs::create_dir_all(previous.join("p@1.0.0")).unwrap();

        let root = side_effects_cache_root(&store);

        assert_ne!(root, previous);
        assert!(previous.join("p@1.0.0").exists());
    }

    /// Within this layout, an entry with no payload is damaged: a cleaner took
    /// it. It is dropped like any other entry that does not check out.
    #[test]
    fn an_entry_without_a_payload_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);
        std::fs::remove_dir_all(entry.path.join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD)).unwrap();

        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
        assert!(!entry.path.exists(), "an unusable entry was left behind");
    }

    /// Two working directories asked for in the same clock tick must still be
    /// two directories: a restore stages into one and moves the package aside
    /// into the other.
    #[test]
    fn working_directories_do_not_collide_within_a_tick() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("node_modules").join("p");
        std::fs::create_dir_all(&package).unwrap();

        let first = restore_staging_dir(&package).unwrap();
        let second = restore_staging_dir(&package).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn a_damaged_entry_is_republished_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        let entry = published_entry(&cache, &pkg);
        let built = entry
            .path
            .join(SIDE_EFFECTS_CACHE_ENTRY_PAYLOAD)
            .join("build/built.node");
        std::fs::remove_file(&built).unwrap();

        // The rebuild that follows the miss, saving without overwrite asked for.
        std::fs::create_dir_all(pkg.join("build")).unwrap();
        std::fs::write(pkg.join("build/built.node"), "built").unwrap();
        entry.save(&pkg, false).unwrap();

        assert!(built.exists(), "a damaged entry blocked its own repair");
        assert!(entry_is_intact(&entry.path));
    }

    /// Two different trees must not describe themselves the same way. Without
    /// escaping, a file named "a\nl x" and a symlink "b" -> "y" produce the same
    /// text as a file named "a" and a symlink "b" -> "x\nl y".
    #[cfg(unix)]
    #[test]
    fn the_listing_cannot_be_forged_with_a_crafted_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("a"), "").unwrap();
        std::os::unix::fs::symlink("x\nl y", first.join("b")).unwrap();

        let second = dir.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("a\nl x"), "").unwrap();
        std::os::unix::fs::symlink("y", second.join("b")).unwrap();

        assert_ne!(
            entry_manifest(&first).unwrap(),
            entry_manifest(&second).unwrap()
        );
    }

    /// The same, with a space: a link "c" -> "a b" against a link "b c" -> "a".
    #[cfg(unix)]
    #[test]
    fn the_listing_cannot_be_forged_with_a_space() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        std::os::unix::fs::symlink("a b", first.join("c")).unwrap();

        let second = dir.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        std::os::unix::fs::symlink("a", second.join("b c")).unwrap();

        assert_ne!(
            entry_manifest(&first).unwrap(),
            entry_manifest(&second).unwrap()
        );
    }

    /// A restore stages a whole package tree beside the package, so anything a
    /// cut-short run leaves behind has to be cleared rather than accumulate.
    #[cfg(unix)]
    #[test]
    fn a_restore_directory_left_behind_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("node_modules");
        std::fs::create_dir_all(&parent).unwrap();
        let left_behind = parent.join(format!("{SIDE_EFFECTS_CACHE_RESTORE_PREFIX}1-1"));
        std::fs::create_dir_all(left_behind.join("build")).unwrap();
        std::fs::write(left_behind.join("build/built.node"), "built").unwrap();
        let in_flight = parent.join(format!("{SIDE_EFFECTS_CACHE_RESTORE_PREFIX}2-2"));
        std::fs::create_dir_all(&in_flight).unwrap();
        let package = parent.join("p");
        let entry = published_entry(&dir.path().join("cache"), &package);

        let stale = std::time::SystemTime::now()
            - SIDE_EFFECTS_CACHE_TMP_STALE_AFTER
            - std::time::Duration::from_secs(60);
        std::fs::File::open(&left_behind)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_accessed(stale)
                    .set_modified(stale),
            )
            .unwrap();

        assert!(matches!(
            entry.restore_if_available(&package).unwrap(),
            SideEffectsCacheRestore::Restored
        ));

        assert!(!left_behind.exists(), "a stale restore tree was kept");
        assert!(in_flight.exists(), "another install's restore was removed");
        assert!(
            package.join("build/built.node").exists(),
            "the restore did not land"
        );
    }

    #[test]
    fn stale_marker_does_not_skip_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();

        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        std::fs::write(pkg.join("built.node"), "built").unwrap();
        entry.save(&pkg, false).unwrap();
        std::fs::remove_dir_all(&cache).unwrap();
        std::fs::remove_file(pkg.join("built.node")).unwrap();

        let entry = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }

    #[test]
    fn changed_package_does_not_restore_stale_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            "{\"name\":\"p\",\"revision\":1}\n",
        )
        .unwrap();

        let original = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        std::fs::write(pkg.join("built.node"), "old build").unwrap();
        original.save(&pkg, false).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            "{\"name\":\"p\",\"revision\":2}\n",
        )
        .unwrap();
        std::fs::remove_file(pkg.join("built.node")).unwrap();

        let changed = SideEffectsCacheEntry::new(&cache, "p", "1.0.0", &pkg).unwrap();
        assert_ne!(changed.path, original.path);
        assert!(matches!(
            changed.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }

    #[test]
    fn package_supplied_marker_is_not_installer_state() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        std::fs::write(
            pkg.join(SIDE_EFFECTS_CACHE_MARKER),
            format!("v1\n{}\n{}\n", "a".repeat(128), "b".repeat(128)),
        )
        .unwrap();

        let entry = SideEffectsCacheEntry::new(dir.path(), "p", "1.0.0", &pkg).unwrap();
        assert!(matches!(
            entry.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }
}
