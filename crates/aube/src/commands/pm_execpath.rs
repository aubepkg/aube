//! The `npm_execpath` aube hands every lifecycle script.
//!
//! npm, pnpm, Yarn, and Bun all export the package-manager executable that
//! drove an install, and packages use it to re-enter *that same* manager —
//! `install-artifact-from-github`, for one, verifies a downloaded prebuilt
//! binary with `${npm_execpath} run verify-build`. Standalone aube can name
//! its own binary and that pattern works.
//!
//! An embedder links this crate into *its* executable, so
//! `std::env::current_exe()` is `mise` (or another host), whose CLI is its
//! own: `mise run verify-build` means "run the mise task `verify-build`",
//! not "run the package script". Naming the host there silently sends every
//! such script into the wrong command surface, and the package reads the
//! resulting failure as a bad artifact — the verification never runs.
//!
//! So under an embedder aube exports a shim instead. It re-enters aube's own
//! CLI through the host, using the private `__aube-cli` argv token that
//! [`crate::cli_main`] strips — the same trampoline contract the `node-gyp`
//! shims already use via `AUBE_NODE_GYP_EXE` /`__node-gyp-bootstrap`. A host
//! that does not implement it is no worse off than before this shim existed;
//! a host that does gets real `run` / `exec` / `install` semantics inside
//! lifecycle scripts.

use std::path::{Path, PathBuf};

use miette::miette;

use crate::commands::shim_file::write_if_stale;

/// The private argv token the shim passes to the host executable, and that
/// [`crate::cli_main`] drops before parsing. An embedding host must forward
/// an argv it sees starting with this token into `cli_main`; it is
/// re-exported as [`crate::embed::CLI_TRAMPOLINE_ARG`] so a host matches on
/// the constant rather than a copied literal.
pub const CLI_TRAMPOLINE_ARG: &str = "__aube-cli";

/// The `npm_execpath` value, or `None` to leave the variable unset.
///
/// Unset is the honest answer when aube is embedded and the shim cannot be
/// written: there is no reachable npm-compatible executable, and consumers
/// treat a missing `npm_execpath` as "not run by a known package manager"
/// and fall back to `npm` — which beats naming a binary that would misparse
/// the command.
pub(crate) fn pm_execpath() -> Option<PathBuf> {
    if !aube_util::is_embedded() {
        return std::env::current_exe().ok();
    }
    match shim_path() {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::debug!("could not write the npm_execpath shim: {err:?}");
            None
        }
    }
}

/// Materialize the shim and return its path. Cheap on the steady state —
/// see [`crate::commands::shim_file`].
fn shim_path() -> miette::Result<PathBuf> {
    let dir = aube_store::dirs::cache_dir()
        .ok_or_else(|| miette!("could not resolve cache dir for the npm_execpath shim"))?
        .join("tools")
        .join("pm-exec");
    shim_path_in(&dir)
}

fn shim_path_in(dir: &Path) -> miette::Result<PathBuf> {
    #[cfg(windows)]
    let (path, contents) = (dir.join("aube.cmd"), CMD_SHIM);
    #[cfg(not(windows))]
    let (path, contents) = (dir.join("aube"), SH_SHIM);
    write_if_stale(&path, contents)?;
    Ok(path)
}

/// Re-enters aube's CLI through the embedding host. Falls back to `npm` when
/// the env marker is gone — a script that stashed `npm_execpath` and re-ran
/// outside aube's wrappers still gets a working package manager rather than
/// a broken path.
#[cfg(not(windows))]
const SH_SHIM: &str = r#"#!/usr/bin/env sh
# aube's npm_execpath stand-in under an embedding host. See pm_execpath.rs.
set -eu
if [ -n "${AUBE_CLI_EXE:-}" ]; then
  exec "$AUBE_CLI_EXE" __aube-cli "$@"
fi
exec npm "$@"
"#;

#[cfg(windows)]
const CMD_SHIM: &str = r#"@echo off
rem aube's npm_execpath stand-in under an embedding host. See pm_execpath.rs.
if not defined AUBE_CLI_EXE goto :npm
"%AUBE_CLI_EXE%" __aube-cli %*
exit /b %ERRORLEVEL%
:npm
call npm %*
exit /b %ERRORLEVEL%
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// No embedder is registered in aube's own test process, so the
    /// execpath is the running binary — standalone behavior is unchanged.
    #[test]
    fn standalone_names_the_running_binary() {
        assert!(!aube_util::is_embedded());
        assert_eq!(pm_execpath(), std::env::current_exe().ok());
    }

    /// The shim is a real, runnable file — writing it is what makes the
    /// execpath usable, and a stand-in that is not executable would fail
    /// the moment a package script invoked it.
    #[test]
    fn shim_is_written_executable() {
        let dir = std::env::temp_dir().join(format!(
            "aube-pm-exec-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = shim_path_in(&dir).unwrap();
        assert!(path.starts_with(&dir));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(CLI_TRAMPOLINE_ARG));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, crate::commands::shim_file::SHIM_MODE);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shim's contract with `cli_main`: whatever token the shim passes
    /// is the token the CLI strips.
    #[cfg(not(windows))]
    #[test]
    fn shim_passes_the_token_cli_main_strips() {
        assert!(SH_SHIM.contains(&format!("\"$AUBE_CLI_EXE\" {CLI_TRAMPOLINE_ARG} \"$@\"")));
        assert!(SH_SHIM.contains(aube_scripts::CLI_EXE_ENV));
    }

    #[cfg(windows)]
    #[test]
    fn shim_passes_the_token_cli_main_strips() {
        assert!(CMD_SHIM.contains(&format!("\"%AUBE_CLI_EXE%\" {CLI_TRAMPOLINE_ARG} %*")));
        assert!(CMD_SHIM.contains(aube_scripts::CLI_EXE_ENV));
    }
}
