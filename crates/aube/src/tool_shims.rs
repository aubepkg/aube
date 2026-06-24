use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub(crate) const SHIM_DIR_ENV: &str = "AUBE_SHIM_DIR";

pub(crate) const TOOL_SHIMS: &[&str] = &["node", "npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg"];

pub(crate) fn shim_dir() -> Option<PathBuf> {
    let ns = aube_util::embedder().data_namespace;
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join(ns).join("shims"));
    }
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()?.join(".local/share"),
    };
    Some(data_home.join(ns).join("shims"))
}

pub(crate) fn sanitize_process_path() {
    let Some(path) = path_without_shim_dir(std::env::var_os("PATH")) else {
        return;
    };
    // SAFETY: called from the synchronous CLI startup path before the
    // tokio runtime spawns worker threads.
    unsafe {
        std::env::set_var("PATH", path);
    }
}

fn path_without_shim_dir(path: Option<OsString>) -> Option<OsString> {
    let shim_dir = std::env::var_os(SHIM_DIR_ENV).filter(|v| !v.is_empty())?;
    strip_path_entry(path?, Path::new(&shim_dir))
}

fn strip_path_entry(path: OsString, entry_to_remove: &Path) -> Option<OsString> {
    let original: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let kept: Vec<PathBuf> = original
        .iter()
        .filter(|entry| entry.as_path() != entry_to_remove)
        .cloned()
        .collect();
    if kept.len() == original.len() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

pub(crate) fn stem_of_argv0(argv0: &OsStr) -> String {
    std::path::Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aube")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_matching_path_entry() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = OsString::from(format!("/a{sep}/b{sep}/c"));
        assert_eq!(
            strip_path_entry(path, Path::new("/b")),
            Some(OsString::from(format!("/a{sep}/c")))
        );
    }
}
