use miette::miette;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const PROJECTS_DIR: &str = ".projects";
const LOCK_FILE: &str = ".prune.lock";

#[derive(Debug, Serialize, Deserialize)]
struct RegisteredProject {
    project_dir: PathBuf,
    aube_dir: PathBuf,
}

pub(crate) struct GvsLock(std::fs::File);

impl Drop for GvsLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn open_lock(global_virtual_store: &Path) -> miette::Result<std::fs::File> {
    std::fs::create_dir_all(global_virtual_store).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to create global virtual store {}: {e}",
            global_virtual_store.display()
        )
    })?;
    let path = global_virtual_store.join(LOCK_FILE);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
                "failed to open global virtual store lock {}: {e}",
                path.display()
            )
        })
}

pub(crate) fn lock_for_install(global_virtual_store: &Path) -> miette::Result<GvsLock> {
    let file = open_lock(global_virtual_store)?;
    file.lock_shared().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to lock global virtual store {} for install: {e}",
            global_virtual_store.display()
        )
    })?;
    Ok(GvsLock(file))
}

fn lock_for_prune(global_virtual_store: &Path) -> miette::Result<GvsLock> {
    let file = open_lock(global_virtual_store)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            crate::progress::safe_eprintln(
                "Waiting for a running global virtual store install to finish before pruning",
            );
            file.lock()
                .map_err(|e| lock_error(global_virtual_store, e))?;
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(lock_error(global_virtual_store, e));
        }
    }
    Ok(GvsLock(file))
}

fn lock_error(global_virtual_store: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to lock global virtual store {} for pruning: {error}",
        global_virtual_store.display()
    )
}

pub(crate) fn register_project(
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    std::fs::create_dir_all(global_virtual_store)
        .map_err(|e| registry_error(global_virtual_store, e))?;
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    std::fs::create_dir_all(&projects_dir).map_err(|e| registry_error(&projects_dir, e))?;
    let project_dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.into());
    let aube_dir = if aube_dir.is_absolute() {
        aube_dir.to_path_buf()
    } else {
        project_dir.join(aube_dir)
    };
    let record = RegisteredProject {
        project_dir,
        aube_dir,
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to encode global virtual store project registry entry: {e}"
        )
    })?;
    aube_util::fs_atomic::atomic_write(
        &project_record_path(&projects_dir, &record.project_dir),
        &bytes,
    )
    .map_err(|e| registry_error(&projects_dir, e))
}

pub(crate) fn unregister_if_unreferenced(
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    if project_links_into(aube_dir, global_virtual_store)? {
        return register_project(global_virtual_store, project_dir, aube_dir);
    }
    let project_dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.into());
    let path = project_record_path(&global_virtual_store.join(PROJECTS_DIR), &project_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(registry_error(&path, e)),
    }
}

fn project_record_path(projects_dir: &Path, project_dir: &Path) -> PathBuf {
    let key = blake3::hash(project_dir.as_os_str().as_encoded_bytes()).to_hex();
    projects_dir.join(format!("{key}.json"))
}

pub(crate) fn register_fast_path_project(
    _lock: &GvsLock,
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    if !project_links_into(aube_dir, global_virtual_store)? {
        return Ok(());
    }
    register_project(global_virtual_store, project_dir, aube_dir)
}

pub(crate) fn prune(global_virtual_store: &Path, dry_run: bool) -> miette::Result<usize> {
    if !global_virtual_store.exists() {
        return Ok(0);
    }
    let _lock = lock_for_prune(global_virtual_store)?;
    let mut reachable = HashSet::new();
    let mut stale_records = Vec::new();
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    if !projects_dir.exists() {
        if !graph_entries(global_virtual_store)?.is_empty() {
            return Err(miette!(
                code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
                "global virtual store project registry {} is missing while entries exist\nhelp: run {} in active projects before pruning again",
                projects_dir.display(),
                aube_util::cmd("install")
            ));
        }
        if !dry_run {
            std::fs::create_dir_all(&projects_dir).map_err(|e| registry_error(&projects_dir, e))?;
        }
    }

    if projects_dir.exists() {
        for entry in read_dir(&projects_dir)? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| prune_error(&path, e))?;
            let project: RegisteredProject =
                serde_json::from_slice(&bytes).map_err(|e| invalid_registry_error(&path, e))?;
            if !project.project_dir.exists() || !project.aube_dir.exists() {
                if !dry_run {
                    stale_records.push(path);
                }
                continue;
            }
            let current = project_entries(global_virtual_store, &project.aube_dir)?;
            reachable.extend(current);
        }
    }

    let mut removed = 0;
    for entry in read_dir(global_virtual_store)? {
        let name = entry.file_name();
        if !is_graph_entry_name(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|e| prune_error(&entry.path(), e))?;
        if !file_type.is_dir() || reachable.contains(&name) {
            continue;
        }
        if !dry_run {
            aube_linker::remove_dir_all_with_retry(&entry.path())
                .map_err(|e| prune_error(&entry.path(), e))?;
        }
        removed += 1;
    }
    for path in stale_records {
        std::fs::remove_file(&path).map_err(|e| prune_error(&path, e))?;
    }
    Ok(removed)
}

fn is_graph_entry_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    !name.starts_with('.') && name != "node_modules"
}

fn graph_entries(global_virtual_store: &Path) -> miette::Result<Vec<OsString>> {
    let mut entries = Vec::new();
    for entry in read_dir(global_virtual_store)? {
        let name = entry.file_name();
        if !is_graph_entry_name(&name) {
            continue;
        }
        if entry
            .file_type()
            .map_err(|e| prune_error(&entry.path(), e))?
            .is_dir()
        {
            entries.push(name);
        }
    }
    Ok(entries)
}

fn project_links_into(aube_dir: &Path, global_virtual_store: &Path) -> miette::Result<bool> {
    Ok(!project_entries(global_virtual_store, aube_dir)?.is_empty())
}

fn project_entries(
    global_virtual_store: &Path,
    aube_dir: &Path,
) -> miette::Result<HashSet<OsString>> {
    let mut entries = HashSet::new();
    if !aube_dir.exists() {
        return Ok(entries);
    }
    let canonical_gvs =
        std::fs::canonicalize(global_virtual_store).unwrap_or_else(|_| global_virtual_store.into());
    for entry in read_dir(aube_dir)? {
        if !is_graph_entry_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let Some(canonical_target) = resolved_link_target(&path, aube_dir) else {
            continue;
        };
        let Ok(relative) = canonical_target.strip_prefix(&canonical_gvs) else {
            continue;
        };
        if let Some(component) = relative.components().next() {
            entries.insert(component.as_os_str().to_os_string());
        }
    }
    Ok(entries)
}

fn resolved_link_target(path: &Path, base: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(path).ok()?;
    let absolute = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(base).join(target)
    };
    Some(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn read_dir(path: &Path) -> miette::Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(path)
        .map_err(|e| prune_error(path, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| prune_error(path, e))
}

fn prune_error(path: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to prune global virtual store path {}: {error}",
        path.display()
    )
}

fn invalid_registry_error(path: &Path, error: serde_json::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "invalid global virtual store project registry entry {}: {error}",
        path.display()
    )
}

fn registry_error(path: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to update global virtual store project registry {}: {error}",
        path.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_registered_and_removes_historical_entries() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");
        drop(lock_for_install(&gvs).expect("legacy snapshot should initialize"));
        let project = tmp.path().join("project");
        let aube_dir = project.join("node_modules/.aube");
        let live = gvs.join("live@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&live).expect("live entry should be created");
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        std::os::unix::fs::symlink(&live, aube_dir.join("live@1.0.0"))
            .expect("project link should be created");

        register_project(&gvs, &project, &aube_dir).expect("project should register");
        let orphan = gvs.join("orphan@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&orphan).expect("orphan entry should be created");
        let orphan_project = tmp.path().join("orphan-project");
        let orphan_aube_dir = orphan_project.join("node_modules/.aube");
        std::fs::create_dir_all(&orphan_aube_dir)
            .expect("orphan project virtual store should be created");
        std::os::unix::fs::symlink(&orphan, orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be created");
        register_project(&gvs, &orphan_project, &orphan_aube_dir)
            .expect("orphan project should register");
        std::fs::remove_file(orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be removed");
        assert_eq!(prune(&gvs, true).expect("dry run should succeed"), 1);
        assert!(orphan.exists(), "dry run must not remove candidates");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(live.exists(), "registered entry must survive");
        assert!(!orphan.exists(), "unlinked historical claim must be pruned");
    }

    #[test]
    fn prune_removes_entries_from_deleted_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        let orphan = gvs.join("orphan@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&orphan).expect("new orphan should be created");
        let orphan_project = tmp.path().join("orphan-project");
        let orphan_aube_dir = orphan_project.join("node_modules/.aube");
        std::fs::create_dir_all(&orphan_aube_dir)
            .expect("orphan project virtual store should be created");
        std::os::unix::fs::symlink(&orphan, orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be created");
        register_project(&gvs, &orphan_project, &orphan_aube_dir)
            .expect("orphan project should register");
        std::fs::remove_dir_all(&orphan_project).expect("orphan project should be removed");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(!orphan.exists(), "stale project entry must be pruned");
    }

    #[test]
    fn failed_link_cleanup_removes_only_unreferenced_records() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let project = tmp.path().join("project");
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register");

        unregister_if_unreferenced(&gvs, &project, &aube_dir)
            .expect("empty project record should be removed");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            0
        );

        let live = gvs.join("live@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&live).expect("live entry should be created");
        std::os::unix::fs::symlink(&live, aube_dir.join("live@1.0.0"))
            .expect("project link should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register again");
        unregister_if_unreferenced(&gvs, &project, &aube_dir)
            .expect("referenced project record should be preserved");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            1
        );
    }

    #[test]
    fn first_prune_initializes_an_empty_registry() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");

        assert_eq!(prune(&gvs, false).expect("empty prune should succeed"), 0);
        assert!(gvs.join(PROJECTS_DIR).exists());
    }

    #[test]
    fn prune_fails_closed_when_an_initialized_registry_disappears() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let untracked = gvs.join("untracked@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&untracked).expect("untracked entry should be created");

        let error = prune(&gvs, false).expect_err("missing registry must fail closed");
        assert!(
            error.to_string().contains("project registry"),
            "unexpected error: {error}"
        );
        assert!(untracked.exists(), "failed prune must not delete entries");
    }

    #[test]
    fn prune_removes_untracked_entries_from_interrupted_installs() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        std::fs::create_dir_all(gvs.join(PROJECTS_DIR))
            .expect("project registry should be initialized");
        let interrupted = gvs.join("interrupted@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&interrupted).expect("untracked entry should be created");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(!interrupted.exists(), "untracked entry must be pruned");
    }

    #[test]
    fn prune_drops_registry_records_for_deleted_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let project = tmp.path().join("project");
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register");
        std::fs::remove_dir_all(&project).expect("project should be removed");

        prune(&gvs, false).expect("prune should remove stale registry record");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            0
        );
    }
}
