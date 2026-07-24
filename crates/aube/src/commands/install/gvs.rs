use crate::state;
use miette::miette;
use std::path::Path;

pub(super) fn resolve_global_virtual_store_override(
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    manifests: &[(String, aube_manifest::PackageJson)],
    env_snapshot: &[(String, String)],
) -> Option<bool> {
    let explicit = aube_settings::resolved::enable_global_virtual_store(settings_ctx);
    explicit.or_else(|| {
        let triggers =
            aube_settings::resolved::disable_global_virtual_store_for_packages(settings_ctx);
        let triggered_by = super::settings::find_gvs_incompatible_trigger(manifests, &triggers);
        let ci_mode = env_snapshot.iter().any(|(k, _)| k == "CI");
        let virtual_store_only_setting = aube_settings::resolved::virtual_store_only(settings_ctx);
        if let Some(name) = triggered_by
            && !ci_mode
            && !virtual_store_only_setting
        {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_GVS_INCOMPATIBLE,
                "`{name}` isn't compatible with aube's global virtual store — \
                 installing per-project instead. Install still succeeds; repeat \
                 installs of this project just won't share materialized packages \
                 across projects. Fixing this requires an upstream change in \
                 `{name}` itself (please file it with that project, not aube). \
                 To silence this warning, run `aube config set \
                 enableGlobalVirtualStore false --location project` — or set \
                 `disableGlobalVirtualStoreForPackages=[]` to opt out of this \
                 auto-detection entirely. \
                 Details: https://aube.jdx.dev/package-manager/global-virtual-store"
            );
            Some(false)
        } else {
            None
        }
    })
}

pub(super) fn planned_global_virtual_store(
    use_global_virtual_store_override: Option<bool>,
    env_snapshot: &[(String, String)],
) -> bool {
    use_global_virtual_store_override
        .unwrap_or_else(|| !env_snapshot.iter().any(|(k, _)| k == "CI"))
}

/// Write the pnpm-compatible metadata Vite 8.1+ uses to allow files
/// served from a virtual store outside the workspace root.
///
/// `.modules.yaml` is not aube's install-state file — `.aube-state`
/// remains authoritative. This is a narrow compatibility surface, and
/// preserving unknown keys lets aube coexist with metadata left by
/// pnpm or other tools.
pub(super) fn write_modules_metadata(
    workspace_root: &Path,
    graph: &aube_lockfile::LockfileGraph,
    modules_dir_name: &str,
    virtual_store_dir: &Path,
) -> std::io::Result<()> {
    let virtual_store_dir = virtual_store_dir.to_string_lossy().into_owned();
    for importer_path in graph.importers.keys() {
        let project_dir =
            super::workspace::importer_project_dir(workspace_root, importer_path.as_str());
        let metadata_path = project_dir.join(modules_dir_name).join(".modules.yaml");
        let mut metadata = match std::fs::read(&metadata_path) {
            Ok(bytes) => {
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
                    .unwrap_or_default()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(err) => return Err(err),
        };
        metadata.insert(
            "virtualStoreDir".to_string(),
            serde_json::Value::String(virtual_store_dir.clone()),
        );
        let bytes = serde_json::to_vec_pretty(&metadata)?;
        aube_util::fs_atomic::atomic_write(&metadata_path, &bytes)?;
    }
    Ok(())
}

pub(super) fn reset_on_mode_change(
    cwd: &Path,
    aube_dir: &Path,
    modules_dir_name: &str,
    planned_gvs: bool,
) -> miette::Result<()> {
    let Some(existing_gvs) = super::settings::detect_aube_dir_gvs_mode(aube_dir) else {
        return Ok(());
    };
    if existing_gvs == planned_gvs {
        return Ok(());
    }

    let from = if existing_gvs { "enabled" } else { "disabled" };
    let to = if planned_gvs { "enabled" } else { "disabled" };
    let modules_dir_path = cwd.join(modules_dir_name);
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_GVS_MODE_CHANGED,
        "global virtual store {from} → {to}; removing {} and reinstalling from scratch",
        modules_dir_path.display()
    );
    remove_dir_all_if_exists(&modules_dir_path).map_err(|e| {
        miette!(
            "global virtual store transition: failed to remove {}: {e}",
            modules_dir_path.display()
        )
    })?;
    if !aube_dir.starts_with(&modules_dir_path) {
        remove_dir_all_if_exists(aube_dir).map_err(|e| {
            miette!(
                "global virtual store transition: failed to remove {}: {e}",
                aube_dir.display()
            )
        })?;
    }
    state::remove_state(cwd).map_err(|e| {
        miette!("global virtual store transition: failed to remove install state: {e}")
    })
}

fn remove_dir_all_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_vite_metadata_for_each_importer_and_preserves_unknown_keys() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("node_modules"))
            .expect("root node_modules should be created");
        std::fs::create_dir_all(root.join("packages/app/node_modules"))
            .expect("member node_modules should be created");
        std::fs::write(
            root.join("node_modules/.modules.yaml"),
            br#"{"packageManager":"pnpm@11.0.0","virtualStoreDir":".pnpm"}"#,
        )
        .expect("existing metadata should be written");

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(".".to_string(), Default::default());
        graph
            .importers
            .insert("packages/app".to_string(), Default::default());
        let store = root.join("shared/virtual-store");

        write_modules_metadata(root, &graph, "node_modules", &store)
            .expect("metadata should be written");

        for project in [root.to_path_buf(), root.join("packages/app")] {
            let bytes = std::fs::read(project.join("node_modules/.modules.yaml"))
                .expect("metadata should exist");
            let metadata: serde_json::Value =
                serde_json::from_slice(&bytes).expect("metadata should be JSON-compatible YAML");
            assert_eq!(
                metadata["virtualStoreDir"],
                store.to_string_lossy().as_ref()
            );
        }
        let root_metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("node_modules/.modules.yaml"))
                .expect("root metadata should exist"),
        )
        .expect("root metadata should parse");
        assert_eq!(root_metadata["packageManager"], "pnpm@11.0.0");
    }
}
