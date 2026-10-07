use miette::{Context, IntoDiagnostic, miette};
use sha2::Digest;

#[cfg(test)]
const SIDE_EFFECTS_CACHE_MARKER: &str = ".aube-side-effects-cache";
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
        copy_dir(&self.path, package_dir, CopyMode::HardlinkOrCopy).wrap_err_with(|| {
            format!(
                "failed to restore side effects cache from {}",
                self.path.display()
            )
        })?;
        self.write_marker(package_dir)?;
        tracing::debug!("side-effects-cache: restored {}", self.path.display());
        Ok(SideEffectsCacheRestore::Restored)
    }

    pub(super) fn save(
        &self,
        package_dir: &std::path::Path,
        overwrite_existing: bool,
    ) -> miette::Result<()> {
        if self.path.is_dir() {
            if overwrite_existing {
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
        sweep_stale_side_effects_tmp_dirs(parent);
        self.write_marker(package_dir)?;

        let tmp = parent.join(format!(
            "{SIDE_EFFECTS_CACHE_TMP_PREFIX}{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to remove {}", tmp.display()))?;
        }
        copy_dir(package_dir, &tmp, CopyMode::Copy).wrap_err_with(|| {
            format!(
                "failed to write side effects cache into {}",
                self.path.display()
            )
        })?;
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

fn sweep_stale_side_effects_tmp_dirs(parent: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if should_remove_side_effects_tmp_dir(&entry) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn should_remove_side_effects_tmp_dir(entry: &std::fs::DirEntry) -> bool {
    if !entry
        .file_name()
        .to_string_lossy()
        .starts_with(SIDE_EFFECTS_CACHE_TMP_PREFIX)
    {
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
        .join("side-effects-v1")
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
            cache_dir.join("side-effects-v1")
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
