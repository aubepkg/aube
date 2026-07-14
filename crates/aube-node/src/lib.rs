//! Node-API surface for embedding aube in Bun-based hosts such as OpenCode.

use aube::commands::add::{AddToProjectOptions, add_to_project};
use aube::commands::install::{FrozenMode, InstallOptions};
use napi::Status;
use napi_derive::napi;
use std::path::{Path, PathBuf};

static OPENCODE: aube_util::Embedder = aube_util::Embedder {
    name: "opencode",
    display_name: "OpenCode",
    vendor: None,
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("opencode-aube/", env!("CARGO_PKG_VERSION")),
    self_names: &[],
    compatible_names: &["npm", "pnpm", "bun", "yarn"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "opencode-aube",
    data_namespace: "opencode-aube",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
};

#[napi(object)]
pub struct PackageToAdd {
    pub name: String,
    pub version: Option<String>,
}

#[napi(object)]
pub struct InstallInput {
    pub add: Option<Vec<PackageToAdd>>,
    pub force: Option<bool>,
    pub offline: Option<bool>,
}

#[napi(object)]
pub struct InstallResult {
    pub project_dir: String,
    pub added: Vec<String>,
}

/// Install a project's declared dependencies and optionally add packages.
///
/// The call shape intentionally mirrors OpenCode's current npm service:
/// `install(dir, { add: [{ name, version }] })`. Added packages are saved as
/// exact production dependencies, and lifecycle scripts are always disabled.
#[napi]
pub async fn install(
    project_dir: String,
    input: Option<InstallInput>,
) -> napi::Result<InstallResult> {
    initialize_embedder();

    let project_dir = prepare_project_dir(Path::new(&project_dir)).await?;
    let input = input.unwrap_or(InstallInput {
        add: None,
        force: None,
        offline: None,
    });
    let packages = input
        .add
        .unwrap_or_default()
        .into_iter()
        .map(|package| match package.version {
            Some(version) if !version.is_empty() => format!("{}@{version}", package.name),
            _ => package.name,
        })
        .collect::<Vec<_>>();

    if !packages.is_empty() {
        add_to_project(
            &project_dir,
            &packages,
            AddToProjectOptions {
                save_exact: true,
                ignore_scripts: true,
                force: input.force.unwrap_or(false),
                offline: input.offline.unwrap_or(false),
            },
        )
        .await
        .map_err(to_napi_error)?;
    } else {
        let mut options = InstallOptions::with_mode(FrozenMode::Prefer);
        options.project_dir = Some(project_dir.clone());
        options.ignore_scripts = true;
        options.skip_root_lifecycle = true;
        options.force = input.force.unwrap_or(false);
        if input.offline.unwrap_or(false) {
            options.network_mode = aube_registry::NetworkMode::Offline;
        }

        aube::commands::install::run(options)
            .await
            .map_err(to_napi_error)?;
    }

    Ok(InstallResult {
        project_dir: project_dir.to_string_lossy().into_owned(),
        added: packages,
    })
}

fn initialize_embedder() {
    aube_util::set_embedder(&OPENCODE);
    aube_settings::set_embedder_defaults(vec![
        ("nodeLinker".to_string(), "hoisted".to_string()),
        ("minimumReleaseAge".to_string(), "0".to_string()),
    ]);
}

async fn prepare_project_dir(project_dir: &Path) -> napi::Result<PathBuf> {
    tokio::fs::create_dir_all(project_dir)
        .await
        .map_err(|error| {
            invalid_project_error(project_dir, format!("failed to create directory: {error}"))
        })?;
    let project_dir = tokio::fs::canonicalize(project_dir)
        .await
        .map_err(|error| {
            invalid_project_error(project_dir, format!("failed to resolve directory: {error}"))
        })?;
    let manifest = project_dir.join("package.json");
    if !tokio::fs::try_exists(&manifest).await.map_err(|error| {
        invalid_project_error(
            &project_dir,
            format!("failed to inspect package.json: {error}"),
        )
    })? {
        tokio::fs::write(&manifest, b"{}\n")
            .await
            .map_err(|error| {
                invalid_project_error(
                    &project_dir,
                    format!("failed to create package.json: {error}"),
                )
            })?;
    }
    Ok(project_dir)
}

fn invalid_project_error(project_dir: &Path, detail: String) -> napi::Error {
    napi::Error::new(
        Status::InvalidArg,
        format!(
            "[{}] invalid project directory {}: {detail}",
            aube_codes::errors::ERR_AUBE_EMBED_INVALID_PROJECT,
            project_dir.display()
        ),
    )
}

fn to_napi_error(error: miette::Report) -> napi::Error {
    let code = error
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "ERR_AUBE_UNKNOWN".to_string());
    napi::Error::new(Status::GenericFailure, format!("[{code}] {error}"))
}
