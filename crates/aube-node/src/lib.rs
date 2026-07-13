//! Minimal Node-API surface for proving that aube can run inside Bun.

use aube::commands::install::{FrozenMode, InstallOptions};
use napi_derive::napi;
use std::path::PathBuf;

/// Install the dependencies declared by the package at `project_dir`.
///
/// The proof of concept deliberately exposes one coarse async operation.
/// Package manifest editing, progress callbacks, and cancellation belong in
/// a later production API after the host integration has validated layout and
/// runtime compatibility.
#[napi]
pub async fn install(project_dir: String) -> napi::Result<()> {
    // Direct command-layer embedders register their identity before invoking
    // a command. The setter is first-wins, so repeated addon calls are safe.
    aube_util::set_embedder(&aube_util::AUBE);

    let mut options = InstallOptions::with_mode(FrozenMode::Prefer);
    options.project_dir = Some(PathBuf::from(project_dir));
    options.ignore_scripts = true;
    options.skip_root_lifecycle = true;

    aube::commands::install::run(options)
        .await
        .map_err(|error| napi::Error::from_reason(format!("{error:?}")))
}
