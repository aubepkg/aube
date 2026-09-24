//! The hidden lockfile: a copy of the install's lockfile graph kept at
//! `<modulesDir>/.aube-lock.yaml`, pnpm's `node_modules/.pnpm/lock.yaml`
//! counterpart.
//!
//! Every install that settles on a graph writes it here (always in
//! aube-lock.yaml format, whatever format the project lockfile uses).
//! When the project has no lockfile of any supported kind, install
//! seeds itself from this copy instead of re-resolving every package
//! from the registry. The seed goes through the same drift checks an
//! on-disk lockfile would, so manifest edits still re-resolve.
//!
//! It lives in the modules dir root rather than the virtual store:
//! `virtualStoreDir` can point outside `node_modules`, and the root is
//! present in isolated, hoisted, and global-virtual-store layouts
//! alike. The leading dot keeps it clear of every node_modules sweep,
//! and `rm -rf node_modules` removes it along with the tree it
//! describes. It is never hashed into the install state, so it can't
//! make a stale install look fresh.

use aube_lockfile::LockfileGraph;
use std::path::{Path, PathBuf};

pub(super) fn path(cwd: &Path, modules_dir_name: &str) -> PathBuf {
    cwd.join(modules_dir_name)
        .join(format!(".{}", aube_util::embedder().lockfile_basename))
}

/// Whether an install in `mode` may seed from the hidden lockfile when
/// the project has no lockfile. Mirrors pnpm: prefer-frozen and
/// `--fix-lockfile` installs reuse it, and so does the auto-CI frozen
/// default (pnpm's `frozenLockfileIfExists` only freezes a lockfile
/// that exists). An explicit `--frozen-lockfile` (`strict_no_lockfile`)
/// still fails on the missing lockfile, and `--no-frozen-lockfile`
/// always resolves from scratch.
pub(super) fn seed_allowed(mode: super::FrozenMode, strict_no_lockfile: bool) -> bool {
    match mode {
        super::FrozenMode::Prefer | super::FrozenMode::Fix => true,
        super::FrozenMode::Frozen => !strict_no_lockfile,
        super::FrozenMode::No => false,
    }
}

/// Parse the hidden lockfile at `path`. A missing file is `None`; an
/// unreadable or corrupt one is `None` plus a warning, so the install
/// falls back to a normal resolve instead of failing.
pub(super) fn read(path: &Path, options: aube_lockfile::ParseOptions) -> Option<LockfileGraph> {
    if !path.exists() {
        return None;
    }
    match aube_lockfile::pnpm::parse_with_options(path, options) {
        Ok(graph) => Some(graph),
        Err(e) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_HIDDEN_LOCKFILE_BROKEN,
                "ignoring broken hidden lockfile at {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Write `graph` to the hidden lockfile (atomic tempfile + rename via
/// the lockfile writer). Best-effort: the hidden lockfile is only an
/// accelerator, so a failure drops any stale copy and never fails the
/// install.
pub(super) fn write(path: &Path, graph: &LockfileGraph, manifest: &aube_manifest::PackageJson) {
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| aube_lockfile::Error::Io(path.to_path_buf(), e))
        .and_then(|()| aube_lockfile::pnpm::write(path, graph, manifest));
    if let Err(e) = result {
        tracing::debug!("failed to write hidden lockfile {}: {e}", path.display());
        remove(path);
    }
}

/// Remove the hidden lockfile so an install that can't keep it current
/// doesn't leave a stale copy behind for a later install to seed from.
pub(super) fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!("removed hidden lockfile {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!("failed to remove hidden lockfile {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with(name: &str, version: &str) -> LockfileGraph {
        let dep_path = format!("{name}@{version}");
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some(format!("^{version}")),
            }],
        );
        graph.packages.insert(
            dep_path.clone(),
            aube_lockfile::LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                integrity: Some("sha512-AAAA".to_string()),
                dep_path,
                ..Default::default()
            },
        );
        graph
    }

    fn manifest_with(name: &str, range: &str) -> aube_manifest::PackageJson {
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .dependencies
            .insert(name.to_string(), range.to_string());
        manifest
    }

    #[test]
    fn hidden_lockfile_round_trips_through_modules_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(dir.path(), "node_modules");
        assert_eq!(
            path,
            dir.path().join("node_modules").join(".aube-lock.yaml")
        );
        // The modules dir doesn't exist yet: write creates it.
        write(
            &path,
            &graph_with("is-odd", "3.0.1"),
            &manifest_with("is-odd", "^3.0.1"),
        );
        let graph = read(&path, aube_lockfile::ParseOptions::default()).unwrap();
        assert_eq!(graph.importers["."][0].name, "is-odd");
        assert_eq!(graph.packages["is-odd@3.0.1"].version, "3.0.1");
    }

    #[test]
    fn seed_allowed_matches_pnpm_frozen_semantics() {
        use super::super::FrozenMode;
        assert!(seed_allowed(FrozenMode::Prefer, false));
        assert!(seed_allowed(FrozenMode::Fix, false));
        // Auto-CI frozen default: no lockfile to freeze.
        assert!(seed_allowed(FrozenMode::Frozen, false));
        // Explicit --frozen-lockfile / `aube ci`.
        assert!(!seed_allowed(FrozenMode::Frozen, true));
        assert!(!seed_allowed(FrozenMode::No, false));
    }

    #[test]
    fn missing_hidden_lockfile_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(dir.path(), "node_modules");
        assert!(read(&path, aube_lockfile::ParseOptions::default()).is_none());
    }

    #[test]
    fn broken_hidden_lockfile_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(dir.path(), "node_modules");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "lockfileVersion: [\n  : not yaml").unwrap();
        assert!(read(&path, aube_lockfile::ParseOptions::default()).is_none());
    }

    #[test]
    fn remove_drops_hidden_lockfile_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(dir.path(), "node_modules");
        write(
            &path,
            &graph_with("is-odd", "3.0.1"),
            &manifest_with("is-odd", "^3.0.1"),
        );
        assert!(path.exists());
        remove(&path);
        assert!(!path.exists());
        remove(&path);
    }
}
