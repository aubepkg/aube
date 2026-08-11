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
    file.lock().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to lock global virtual store {} for pruning: {e}",
            global_virtual_store.display()
        )
    })?;
    Ok(GvsLock(file))
}

pub(crate) fn register_project(
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    std::fs::create_dir_all(&projects_dir).map_err(|e| registry_error(&projects_dir, e))?;
    let project_dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.into());
    let aube_dir = if aube_dir.is_absolute() {
        aube_dir.to_path_buf()
    } else {
        project_dir.join(aube_dir)
    };
    let key = blake3::hash(project_dir.as_os_str().as_encoded_bytes()).to_hex();
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
    aube_util::fs_atomic::atomic_write(&projects_dir.join(format!("{key}.json")), &bytes)
        .map_err(|e| registry_error(&projects_dir, e))
}

pub(crate) fn register_fast_path_project(project_dir: &Path) -> miette::Result<()> {
    let global_virtual_store = super::global_virtual_store_dir(project_dir);
    let aube_dir = super::resolve_virtual_store_dir_for_cwd(project_dir);
    if !project_links_into(&aube_dir, &global_virtual_store)? {
        return Ok(());
    }
    let _lock = lock_for_install(&global_virtual_store)?;
    register_project(&global_virtual_store, project_dir, &aube_dir)
}

pub(crate) fn prune(global_virtual_store: &Path, dry_run: bool) -> miette::Result<usize> {
    if !global_virtual_store.exists() {
        return Ok(0);
    }
    let _lock = lock_for_prune(global_virtual_store)?;
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    if !projects_dir.exists() {
        eprintln!("No registered projects for global virtual store");
        return Ok(0);
    }

    let mut reachable = HashSet::new();
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
                std::fs::remove_file(&path).map_err(|e| prune_error(&path, e))?;
            }
            continue;
        }
        mark_project_entries(global_virtual_store, &project.aube_dir, &mut reachable)?;
    }

    let mut removed = 0;
    for entry in read_dir(global_virtual_store)? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
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
    Ok(removed)
}

fn mark_project_entries(
    global_virtual_store: &Path,
    aube_dir: &Path,
    reachable: &mut HashSet<OsString>,
) -> miette::Result<()> {
    let canonical_gvs =
        std::fs::canonicalize(global_virtual_store).unwrap_or_else(|_| global_virtual_store.into());
    for entry in read_dir(aube_dir)? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let path = entry.path();
        let Ok(target) = std::fs::read_link(&path) else {
            continue;
        };
        let absolute = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(aube_dir).join(target)
        };
        let canonical_target = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        let Ok(relative) = canonical_target.strip_prefix(&canonical_gvs) else {
            continue;
        };
        if let Some(component) = relative.components().next() {
            reachable.insert(component.as_os_str().to_os_string());
        }
    }
    Ok(())
}

fn project_links_into(aube_dir: &Path, global_virtual_store: &Path) -> miette::Result<bool> {
    if !aube_dir.exists() {
        return Ok(false);
    }
    let canonical_gvs =
        std::fs::canonicalize(global_virtual_store).unwrap_or_else(|_| global_virtual_store.into());
    for entry in read_dir(aube_dir)? {
        let path = entry.path();
        let Ok(target) = std::fs::read_link(&path) else {
            continue;
        };
        let absolute = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(aube_dir).join(target)
        };
        let canonical_target = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        if canonical_target.starts_with(&canonical_gvs) {
            return Ok(true);
        }
    }
    Ok(false)
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
    fn prune_keeps_registered_entries_and_removes_orphans() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        let project = tmp.path().join("project");
        let aube_dir = project.join("node_modules/.aube");
        let live = gvs.join("live@1.0.0-deadbeefdeadbeef");
        let orphan = gvs.join("orphan@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&live).expect("live entry should be created");
        std::fs::create_dir_all(&orphan).expect("orphan entry should be created");
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        std::os::unix::fs::symlink(&live, aube_dir.join("live@1.0.0"))
            .expect("project link should be created");

        register_project(&gvs, &project, &aube_dir).expect("project should register");
        assert_eq!(prune(&gvs, true).expect("dry run should succeed"), 1);
        assert!(orphan.exists(), "dry run must preserve the orphan");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(live.exists(), "registered entry must survive");
        assert!(!orphan.exists(), "orphan entry must be removed");
    }

    #[test]
    fn prune_drops_registry_records_for_deleted_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
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
