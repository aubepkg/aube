use crate::Error;
use aube_lockfile::DirectDep;
use std::collections::BTreeMap;
use std::path::Path;

/// Sweep orphan `.tmp-<pid>-*` directories in the virtual store.
///
/// Linker materializes each package into `.tmp-<pid>-<id>/`
/// then atomic-renames into `.aube/<subdir>/`. Crash or Ctrl-C
/// between materialize and rename leaves the tmp dir behind.
/// Nothing else cleans these up so they accumulate on every aborted
/// install. Small footprint per entry but a few hundred aborted
/// CI runs pile up gigabytes.
///
/// Called early in link_all so each fresh install reclaims space
/// from prior crashes. Only matches the exact prefix we produce so
/// user files named `.tmp-*` in the virtual store are safe.
pub fn sweep_stale_tmp_dirs(virtual_store: &Path) {
    let Ok(entries) = std::fs::read_dir(virtual_store) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match our exact prefix. Format: `.tmp-<pid>-<id>`
        // where pid is numeric.
        if !name.starts_with(".tmp-") {
            continue;
        }
        let rest = &name[".tmp-".len()..];
        let Some((pid_str, _rest)) = rest.split_once('-') else {
            continue;
        };
        if pid_str.chars().any(|c| !c.is_ascii_digit()) {
            continue;
        }
        // Do not touch the dir of our own still-running process.
        // Materialize path creates and removes its tmp dir in the
        // same call and crashes mid-way are the target here, the
        // active pid will not leave ones around that matter.
        if pid_str == std::process::id().to_string() {
            continue;
        }
        let _ = remove_dir_all_with_retry(&entry.path());
    }
}

/// Remove a directory with retry on Windows sharing violations.
///
/// Windows does not let you delete a file while another process holds
/// a handle open. Dev server, vitest watcher, tsc --watch all hold
/// .js / .node files inside node_modules. aube reinstall hits ERROR
/// 32 (SHARING_VIOLATION) or ERROR 5 (ACCESS_DENIED, AV scanner
/// mid-scan) and leaves a half-deleted virtual store. pnpm, npm,
/// rimraf all retry with backoff. Do the same. Unix passthrough.
///
/// Retries 10 times with exponential backoff starting at 50ms. Total
/// worst case around 10 seconds which is tolerable for an install
/// already paying for filesystem work.
pub fn remove_dir_all_with_retry(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }
    #[cfg(windows)]
    {
        use std::io::ErrorKind;
        let mut delay_ms = 50u64;
        for attempt in 0..10 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    // Sharing violation and PermissionDenied both
                    // map to retriable Windows errors. Bail on
                    // attempt 10.
                    let retriable =
                        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::Other)
                            || e.raw_os_error() == Some(32);
                    if !retriable || attempt == 9 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    delay_ms = (delay_ms * 2).min(2000);
                }
            }
        }
        // Unreachable, loop always returns by attempt 10.
        Ok(())
    }
}

/// Real workspace importer, not a peer-context bookkeeping entry.
///
/// pnpm v9 lockfiles record the peer-resolution view of each
/// workspace package reached through every nested `node_modules/`
/// traversal. Those virtual importer paths (e.g.
/// `packages/a/node_modules/@scope/b/node_modules/@scope/c`) describe
/// *how* a package looks from a particular context — they are reached
/// via the workspace-to-workspace symlink chain and have no
/// independent `node_modules/` to populate. When the link pipeline
/// treats them as physical importers it queues parallel symlink tasks
/// whose `link_path`s canonicalize to the same inode as a physical
/// importer's task, producing EEXIST races on large monorepos.
pub fn is_physical_importer(importer_path: &str) -> bool {
    importer_path == "." || !importer_path.contains("/node_modules/")
}

/// Whether `dedupeDirectDeps` leaves out workspace member `importer_path`'s
/// link to `dep`, because Node walking up from the member reaches the
/// root's identical link first. That holds only for a member inside the
/// root (Node never walks from `../sibling` into the root's
/// `node_modules`) and when no importer in between links a different
/// version of `dep`, which Node would find before the root's.
pub fn dedupe_skips_member_link(
    importers: &BTreeMap<String, Vec<DirectDep>>,
    importer_path: &str,
    dep: &DirectDep,
) -> bool {
    let declares_other_version = |importer: &str| {
        importers.get(importer).is_some_and(|deps| {
            deps.iter()
                .any(|d| d.name == dep.name && d.dep_path != dep.dep_path)
        })
    };
    importer_resolves_through_root(importer_path)
        && importers.get(".").is_some_and(|root| {
            root.iter()
                .any(|d| d.name == dep.name && d.dep_path == dep.dep_path)
        })
        && !Path::new(importer_path)
            .ancestors()
            .skip(1)
            .filter_map(Path::to_str)
            .filter(|ancestor| !ancestor.is_empty())
            .any(declares_other_version)
}

fn importer_resolves_through_root(importer_path: &str) -> bool {
    importer_path != "."
        && !Path::new(importer_path)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

/// Wipe `path` when it looks like a linker-managed `.aube/node_modules`
/// tree. If a previously-tampered install (or attacker) replaced the
/// tree with a symlink / junction pointing elsewhere on disk, refuse
/// to recurse into it — modern Rust `remove_dir_all` already declines
/// to follow symlinks, mirroring the invariant at the call site keeps
/// the intent explicit and catches any future regression in the
/// callee.
pub(crate) fn remove_hidden_hoist_tree(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            let _ = std::fs::remove_file(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_dir_all(path);
        }
        Err(_) => {}
    }
}

/// Best-effort unlink of `path` regardless of whether it's a file,
/// symlink, junction, or directory. Errors are intentionally ignored
/// because this is a "clear the slot" operation — the caller is about
/// to place something else here and any residual entry that survives
/// will surface as a downstream error.
pub(crate) fn try_remove_entry(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    let _ = std::fs::remove_file(path);
}

/// `xx::file::mkdirp` wrapped with the linker's `Error::Xx` conversion.
/// Every materialize pass calls this before creating a symlink /
/// junction, so the lossy `.to_string()` wrap lives in exactly one
/// place.
pub fn mkdirp(dir: &Path) -> Result<(), Error> {
    xx::file::mkdirp(dir).map_err(|e| Error::Xx(e.to_string()))
}

/// Classification of a `.aube/<dep_path>` symlink relative to the
/// current hashed global entry the linker wants to point at.
#[derive(Copy, Clone)]
pub(crate) enum EntryState {
    /// The symlink already points at `expected` and the target exists —
    /// nothing to do. Caller can bump a `packages_cached` counter and
    /// move on.
    Fresh,
    /// No entry at `link_path` yet. Caller needs to materialize and
    /// create the symlink, but there's nothing to unlink first.
    Missing,
    /// An entry exists but is stale (different target, dangling link,
    /// or an `Err` read that isn't NotFound). Caller must unlink
    /// before resymlinking.
    Stale,
}

/// Sweep stale entries out of a `node_modules/` directory while
/// preserving everything in `preserve` (bare names like `lodash` and
/// scope prefixes like `@babel`), dotfiles, and — if set — the
/// virtual-store leaf (`aube_dir_leaf`) sitting right under `nm`
/// with a non-dotfile name (the `virtualStoreDir=node_modules/vstore`
/// case). For `@scope` entries we recurse one level and drop any
/// `@scope/<pkg>` whose full `@scope/pkg` name is not in `preserve`;
/// an empty scope directory left behind by the sweep is removed so
/// the next install doesn't trip over a phantom scope tombstone.
pub(crate) fn sweep_stale_top_level_entries(
    nm: &Path,
    preserve: &std::collections::HashSet<&str>,
    aube_dir_leaf: Option<&std::ffi::OsStr>,
) {
    let scope_prefixes: std::collections::HashSet<&str> = preserve
        .iter()
        .filter_map(|n| n.split_once('/').map(|(scope, _)| scope))
        .collect();
    let Ok(entries) = std::fs::read_dir(nm) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if aube_dir_leaf == Some(name.as_os_str()) {
            continue;
        }
        if preserve.contains(name_str.as_ref()) {
            continue;
        }
        if scope_prefixes.contains(name_str.as_ref()) {
            let scope_dir = entry.path();
            if let Ok(inner) = std::fs::read_dir(&scope_dir) {
                for inner_entry in inner.flatten() {
                    let inner_name = inner_entry.file_name();
                    let full = format!("{}/{}", name_str, inner_name.to_string_lossy());
                    if !preserve.contains(full.as_str()) {
                        try_remove_entry(&inner_entry.path());
                    }
                }
            }
            // If the scope dir is now empty (every member was stale),
            // drop the tombstone directory too.
            if std::fs::read_dir(&scope_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&scope_dir);
            }
            continue;
        }
        try_remove_entry(&entry.path());
    }
}

/// Reconcile the project-owned hidden hoist without rebuilding every link.
/// Never descend through a scope symlink: the hidden tree may have been
/// modified since the last install, and only real scope directories are ours
/// to sweep. The caller checks each retained package link's target afterward.
pub(crate) fn sweep_stale_hidden_hoist_entries(
    hidden: &Path,
    preserve: &rustc_hash::FxHashSet<&str>,
) {
    match std::fs::symlink_metadata(hidden) {
        Ok(md) if md.file_type().is_symlink() => {
            remove_hidden_hoist_tree(hidden);
            return;
        }
        Ok(md) if !md.is_dir() => {
            try_remove_entry(hidden);
            return;
        }
        Ok(_) => {}
        Err(_) => return,
    }
    let scopes: rustc_hash::FxHashSet<&str> = preserve
        .iter()
        .filter_map(|name| name.split_once('/').map(|(scope, _)| scope))
        .collect();
    let Ok(entries) = std::fs::read_dir(hidden) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if scopes.contains(name.as_ref()) {
            let is_real_dir = std::fs::symlink_metadata(&path)
                .is_ok_and(|md| md.is_dir() && !md.file_type().is_symlink());
            if !is_real_dir {
                try_remove_entry(&path);
                continue;
            }
            if let Ok(inner) = std::fs::read_dir(&path) {
                for child in inner.flatten() {
                    let full = format!("{name}/{}", child.file_name().to_string_lossy());
                    if !preserve.contains(full.as_str()) {
                        try_remove_entry(&child.path());
                    }
                }
            }
        } else if !preserve.contains(name.as_ref()) {
            try_remove_entry(&path);
        }
    }
}

/// Sweep broken entries from a shared hidden-hoist directory without
/// deleting live links owned by other projects. The GVS hidden hoist is
/// global, so "not in this project's graph" is not stale enough: another
/// project may still need that link. Only entries whose target no longer
/// exists (or non-link junk) are reclaimed here; current-project names are
/// still target-reconciled by `reconcile_top_level_link` below.
pub(crate) fn sweep_dead_hidden_hoist_entries(hidden: &Path) {
    let Ok(entries) = std::fs::read_dir(hidden) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if name_str.starts_with('@') {
            match std::fs::symlink_metadata(&path) {
                Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                    sweep_dead_hidden_hoist_scope(&path);
                    if std::fs::read_dir(&path)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(&path);
                    }
                }
                Ok(_) => sweep_dead_hidden_hoist_entry(&path),
                Err(_) => {}
            }
            continue;
        }
        sweep_dead_hidden_hoist_entry(&path);
    }
}

fn sweep_dead_hidden_hoist_scope(scope_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(scope_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        sweep_dead_hidden_hoist_entry(&entry.path());
    }
}

fn sweep_dead_hidden_hoist_entry(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() && path.exists() => {}
        Ok(md) if md.file_type().is_symlink() => {
            try_remove_entry(path);
        }
        Ok(md) if md.is_dir() => {
            try_remove_entry(path);
        }
        Ok(_) => {
            try_remove_entry(path);
        }
        Err(_) => {}
    }
}

/// Classify `link_path` against `expected` without the double-check
/// (`read_link` then `exists`) that ate ~1.4k ENOENT syscalls per
/// install on the medium fixture. Fresh means "points at expected
/// AND the target still exists"; everything else is Missing or
/// Stale. The fast path returns without touching disk a second time.
#[inline]
pub(crate) fn classify_entry_state(link_path: &Path, expected: &Path) -> EntryState {
    match std::fs::read_link(link_path) {
        Ok(existing) if existing == expected => {
            if link_path.exists() {
                EntryState::Fresh
            } else {
                EntryState::Stale
            }
        }
        Ok(_) => EntryState::Stale,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EntryState::Missing,
        // Some other error (permission, etc.): treat as Stale and
        // let the removal/recreate path try its best-effort cleanup
        // + surface the real error on symlink creation if unlucky.
        Err(_) => EntryState::Stale,
    }
}

/// Classify a project-local virtual-store entry. A real directory is
/// already materialized and reusable; symlinks and other file types are
/// stale shapes left by a different layout mode.
#[inline]
pub(crate) fn classify_local_entry_state(path: &Path) -> EntryState {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return EntryState::Stale;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return EntryState::Stale;
                }
            }
            if metadata.file_type().is_dir() {
                EntryState::Fresh
            } else {
                EntryState::Stale
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EntryState::Missing,
        Err(_) => EntryState::Stale,
    }
}
/// Create a directory link at `link_path`, tolerating a concurrent
/// creator that already put the *expected* link there.
///
/// The top-level symlink pass runs over a rayon pool, so two tasks can
/// target the same `node_modules/<name>` path — both see it missing,
/// both create, and the loser gets `AlreadyExists`. That is benign
/// when the winner stored the same target we were about to store, so
/// re-check with `reconcile_dir_link` and only surface the error when
/// the path holds something else. Without this a harmless race aborts
/// the whole install with `File exists (os error 17)` partway through
/// linking.
pub(crate) fn create_dir_link_idempotent(target: &Path, link_path: &Path) -> Result<(), Error> {
    if let Err(create_err) = crate::sys::create_dir_link(target, link_path) {
        let won_race = create_err.kind() == std::io::ErrorKind::AlreadyExists
            && reconcile_dir_link(link_path, target).unwrap_or(false);
        if !won_race {
            return Err(Error::Io(link_path.to_path_buf(), create_err));
        }
    }
    Ok(())
}

/// Reconcile a directory link against its expected target.
///
/// Returns `Ok(true)` when the existing link stores the expected target.
/// Missing, incorrectly-targeted, and non-link entries are removed and return
/// `Ok(false)` so the caller can recreate the link. The target is not probed,
/// so a dangling link that stores the expected target is considered current.
pub(crate) fn reconcile_dir_link(link_path: &Path, expected_target: &Path) -> Result<bool, Error> {
    #[cfg(windows)]
    {
        // NTFS junctions store the normalized absolute target
        // `create_dir_link` computed, sometimes read back with a `\\?\`
        // prefix. Compare that stored target, as the Unix branch does, and
        // not the canonical destination: with the global virtual store,
        // `node_modules/<name>` reached through the old and the new
        // `virtualStoreDir` canonicalizes to the same shared entry, which
        // kept links into a relocated virtual store from being rewritten.
        let expected = if expected_target.is_absolute() {
            expected_target.to_path_buf()
        } else {
            link_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(expected_target)
        };
        // `Linker` accepts relative roots, so `link_path` (and thus
        // `expected`) can be relative while the junction stores an absolute
        // target. `absolute` resolves against the cwd without touching the
        // filesystem, the same base `create_dir_link` resolved against.
        let expected_abs = std::path::absolute(&expected).unwrap_or(expected);
        // The link destination is mutable during reconciliation and shared
        // installs can repair it concurrently, so it must never be cached.
        if let Ok(existing) = std::fs::read_link(link_path)
            && same_windows_path(&existing, &expected_abs)
        {
            return Ok(true);
        }
        if link_path.symlink_metadata().is_err() {
            return Ok(false);
        }
        match std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path)) {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(link_path.to_path_buf(), e)),
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::read_link(link_path) {
            Ok(existing) if existing == expected_target => Ok(true),
            Ok(_) => {
                let _ = std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => {
                let _ =
                    std::fs::remove_dir_all(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
        }
    }
}

/// Whether two Windows paths name the same location, ignoring a `\\?\`
/// prefix and `.`/`..` segments. Paths that differ only in case usually
/// do, but not inside a directory made case-sensitive (`fsutil file
/// setCaseSensitiveInfo`), so the filesystem settles those.
#[cfg(windows)]
fn same_windows_path(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| {
        let normalized = crate::sys::normalize_path(p);
        let text = normalized.to_string_lossy();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    };
    let (a_plain, b_plain) = (plain(a), plain(b));
    a_plain == b_plain
        || (a_plain.eq_ignore_ascii_case(&b_plain)
            && matches!(
                (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                (Ok(a), Ok(b)) if a == b
            ))
}

#[cfg(test)]
mod tests {
    /// `Store\pkg` and `store\pkg` under `root`, with `node_modules\pkg`
    /// linked to the former.
    #[cfg(windows)]
    fn link_into_case_variant_stores(root: &std::path::Path) -> std::path::PathBuf {
        for store in ["Store", "store"] {
            std::fs::create_dir_all(root.join(store).join("pkg")).unwrap();
        }
        std::fs::create_dir(root.join("node_modules")).unwrap();
        let link = root.join("node_modules").join("pkg");
        crate::sys::create_dir_link(&root.join("Store").join("pkg"), &link).unwrap();
        link
    }

    #[cfg(windows)]
    #[test]
    fn reconcile_keeps_a_junction_whose_target_differs_only_in_case() {
        let tmp = tempfile::tempdir().unwrap();
        let link = link_into_case_variant_stores(tmp.path());
        // `store` is the same directory as `Store` here.
        let expected = tmp.path().join("store").join("pkg");
        assert!(super::reconcile_dir_link(&link, &expected).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn reconcile_rewrites_a_junction_into_a_case_variant_of_a_case_sensitive_store() {
        let tmp = tempfile::tempdir().unwrap();
        let enabled = std::process::Command::new("fsutil")
            .args(["file", "setCaseSensitiveInfo"])
            .arg(tmp.path())
            .arg("enable")
            .output()
            .is_ok_and(|out| out.status.success());
        if !enabled {
            eprintln!("skipping: this system can't make a directory case-sensitive");
            return;
        }
        let link = link_into_case_variant_stores(tmp.path());
        let expected = tmp.path().join("store").join("pkg");
        assert!(!super::reconcile_dir_link(&link, &expected).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn reconcile_rewrites_a_junction_into_another_store_with_the_same_destination() {
        use super::reconcile_dir_link;
        // `old/pkg` and `new/pkg` are both junctions into `shared`, like a
        // global-virtual-store entry reached through two `virtualStoreDir`s.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("shared")).unwrap();
        for store in ["old", "new"] {
            std::fs::create_dir(root.join(store)).unwrap();
            crate::sys::create_dir_link(&root.join("shared"), &root.join(store).join("pkg"))
                .unwrap();
        }
        std::fs::create_dir(root.join("node_modules")).unwrap();
        let link = root.join("node_modules").join("pkg");
        crate::sys::create_dir_link(std::path::Path::new(r"..\old\pkg"), &link).unwrap();

        assert!(reconcile_dir_link(&link, std::path::Path::new(r"..\old\pkg")).unwrap());
        assert!(!reconcile_dir_link(&link, std::path::Path::new(r"..\new\pkg")).unwrap());
        assert!(
            link.symlink_metadata().is_err(),
            "stale link should be removed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn reconcile_keeps_a_current_junction_given_a_relative_link_path() {
        use super::reconcile_dir_link;
        // `Linker` accepts relative roots, so `link_path` can be relative
        // while the junction stores an absolute target.
        let cwd = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir_in(&cwd).unwrap();
        let root = tmp.path().strip_prefix(&cwd).unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        let link = root.join("link");
        crate::sys::create_dir_link(std::path::Path::new("target"), &link).unwrap();
        assert!(link.is_relative());
        assert!(reconcile_dir_link(&link, std::path::Path::new("target")).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn reconcile_keeps_a_dangling_junction_that_stores_the_expected_target() {
        use super::reconcile_dir_link;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("target")).unwrap();
        let link = root.join("link");
        crate::sys::create_dir_link(std::path::Path::new("target"), &link).unwrap();
        std::fs::remove_dir(root.join("target")).unwrap();
        assert!(reconcile_dir_link(&link, std::path::Path::new("target")).unwrap());
    }

    use super::{dedupe_skips_member_link, importer_resolves_through_root, is_physical_importer};
    use aube_lockfile::{DepType, DirectDep};
    use std::collections::BTreeMap;

    fn dep(dep_path: &str) -> DirectDep {
        DirectDep {
            name: "is-odd".to_string(),
            dep_path: dep_path.to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }
    }

    #[test]
    fn only_members_inside_the_root_resolve_through_it() {
        assert!(importer_resolves_through_root("packages/app"));
        assert!(!importer_resolves_through_root("."));
        assert!(!importer_resolves_through_root("../sibling"));
        assert!(!importer_resolves_through_root("packages/../../elsewhere"));
    }

    #[test]
    fn dedupe_keeps_a_member_link_an_importer_in_between_would_shadow() {
        let importers = BTreeMap::from([
            (".".to_string(), vec![dep("is-odd@3.0.1")]),
            ("packages/outer".to_string(), vec![dep("is-odd@3.0.0")]),
            (
                "packages/outer/inner".to_string(),
                vec![dep("is-odd@3.0.1")],
            ),
            ("packages/other".to_string(), vec![dep("is-odd@3.0.1")]),
            (
                "packages/other/inner".to_string(),
                vec![dep("is-odd@3.0.1")],
            ),
        ]);
        let skips = |importer: &str, dep_path: &str| {
            dedupe_skips_member_link(&importers, importer, &dep(dep_path))
        };
        // `packages/outer` links 3.0.0, which Node would find first.
        assert!(!skips("packages/outer/inner", "is-odd@3.0.1"));
        assert!(!skips("packages/outer", "is-odd@3.0.0"));
        // An importer in between with the root's version links nothing
        // that differs from it.
        assert!(skips("packages/other/inner", "is-odd@3.0.1"));
        assert!(skips("packages/other", "is-odd@3.0.1"));
        assert!(!skips(".", "is-odd@3.0.1"));
    }

    #[test]
    fn root_is_physical() {
        assert!(is_physical_importer("."));
    }

    #[test]
    fn workspace_paths_are_physical() {
        assert!(is_physical_importer("packages/dev/core"));
        assert!(is_physical_importer("apps/web"));
        assert!(is_physical_importer("libs/@scope/name"));
    }

    #[test]
    fn nested_peer_context_paths_are_virtual() {
        // pnpm v9 emits these for every peer-resolution view reachable
        // through the workspace symlink chain. They describe the graph,
        // they are not directories to populate.
        assert!(!is_physical_importer(
            "packages/dev/addons/node_modules/@dev/core"
        ));
        assert!(!is_physical_importer(
            "packages/a/node_modules/@s/b/node_modules/@s/c"
        ));
    }
}
