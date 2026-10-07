//! Global install layout — `aube add -g`, `aube remove -g`, `aube list -g`.
//!
//! The per-install-dir *shape* follows pnpm v11, but the directories are
//! aube's own — everything hangs off the tool's data root
//! (`$XDG_DATA_HOME/aube`, `~/.local/share/aube`, `%LOCALAPPDATA%\aube`),
//! alongside `store/`, `nodejs/`, and `shims/`:
//!
//! ```text
//! <data_root>/                     # `aube prefix -g`
//! ├── bin/                         # <bin_dir>: on PATH; bins symlink into here
//! │   └── some-bin     -> <pkg_dir>/<install>/node_modules/.bin/some-bin
//! └── global-aube/                 # <pkg_dir>: one subdir per global package
//!     ├── <pid>-<ts>/              # physical install dir (normal aube project)
//!     │   ├── package.json
//!     │   └── node_modules/
//!     └── <hash>           -> <pid>-<ts>  # stable pointer keyed on aliases
//! ```
//!
//! Each `aube add -g <pkg>` runs a full normal install into a fresh
//! `<pid>-<ts>` directory, then:
//!   1. Computes a hash of the resolved aliases.
//!   2. Creates `<pkg_dir>/<hash>` as a symlink to the install dir. Any
//!      existing installs of the same aliases are removed first.
//!   3. Symlinks each package's bins from the install dir into `<global_bin>`.
//!
//! `remove -g` / `list -g` walk the hash symlinks in `<pkg_dir>` to find
//! installed packages.

use miette::{Context, IntoDiagnostic, miette};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where aube puts globally-installed packages and their PATH-visible bins.
///
/// `bin_dir` is the directory the user is expected to have on `$PATH` —
/// it's where bin symlinks live. `pkg_dir` is where the per-install
/// directories and hash pointers live; it's a tool-specific subdir so two
/// tools sharing one explicitly-set home don't step on each other.
#[derive(Debug, Clone)]
pub struct GlobalLayout {
    pub bin_dir: PathBuf,
    pub pkg_dir: PathBuf,
}

impl GlobalLayout {
    pub fn resolve() -> miette::Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_default();

        // `bin_dir` and `pkg_dir` are independent: `globalBinDir` controls
        // where bin symlinks go (on PATH), `globalDir` controls where
        // package installs live. Neither inherits from the other — both
        // fall back to their own default (<PREFIX>_HOME → the data root).
        let (setting_bin, setting_pkg) = super::with_settings_ctx(&cwd, |ctx| {
            let bin = aube_settings::resolved::global_bin_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            let pkg = aube_settings::resolved::global_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            (bin, pkg)
        });

        let bin_dir = setting_bin.map_or_else(default_bin_dir, Ok)?;
        // Package-install subdir named after the active embedder so two
        // tools sharing an explicitly-set `<PREFIX>_HOME` don't collide.
        // Standalone aube → `global-aube`.
        let pkg_subdir = format!("global-{}", aube_util::embedder().name);
        let pkg_dir = setting_pkg
            .map_or_else(|| default_pkg_dir(&pkg_subdir), |p| Ok(p.join(&pkg_subdir)))?;

        warn_on_legacy_global_dir(&pkg_dir, &pkg_subdir);
        Ok(Self { bin_dir, pkg_dir })
    }
}

/// The branded home override (standalone aube → `AUBE_HOME`). When set it
/// *is* the PATH-visible bin dir, and package installs go in a subdir of
/// it — the pre-existing contract for people who opted in explicitly. An
/// embedder with no `env_prefix` skips the branded var.
fn branded_home() -> Option<PathBuf> {
    let prefix = aube_util::embedder().env_prefix?;
    std::env::var(format!("{prefix}_HOME"))
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The tool's own data root: `$XDG_DATA_HOME/<ns>`, falling back to
/// `~/.local/share/<ns>` (`%LOCALAPPDATA%\<ns>` on Windows). Same
/// resolution `aube_store::dirs::store_dir` uses, so global installs land
/// beside `store/`, `nodejs/`, and `shims/` instead of in a directory
/// named after another package manager. `<ns>` is the active embedder's
/// `data_namespace` (standalone aube → `aube`).
///
/// XDG is honored on every Unix, macOS included — aube already does that
/// for the store and the packument cache, and the previous `~/Library/pnpm`
/// special case was the one place a macOS user's explicit `XDG_DATA_HOME`
/// was ignored (Discussion #1219).
///
/// Precedence matches `store_dir` exactly, including `%LOCALAPPDATA%`
/// winning over `XDG_DATA_HOME` on Windows: the global dir and the content
/// store must not end up under different roots on the same machine.
fn data_root() -> miette::Result<PathBuf> {
    let ns = aube_util::embedder().data_namespace;
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA")
        && !local.is_empty()
    {
        return Ok(PathBuf::from(local).join(ns));
    }
    // Reached on every Unix, and on Windows when `%LOCALAPPDATA%` is
    // missing — where an explicitly-set `XDG_DATA_HOME` is a better answer
    // than failing outright, again mirroring `store_dir`.
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()
            .ok_or_else(|| miette!("HOME is not set; can't locate global directory"))?
            .join(".local/share"),
    };
    Ok(data_home.join(ns))
}

/// Default for `globalBinDir` — the directory the user puts on `$PATH`.
/// `<data_root>/bin` rather than the data root itself, so the PATH entry
/// holds bins and nothing else.
fn default_bin_dir() -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home);
    }
    data_root().map(|d| d.join("bin"))
}

/// Default for `globalDir` — where the physical per-package install dirs
/// and their hash pointers live. A sibling of `bin/`, not a child: the
/// PATH entry stays a directory of executables.
fn default_pkg_dir(pkg_subdir: &str) -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home.join(pkg_subdir));
    }
    data_root().map(|d| d.join(pkg_subdir))
}

/// Resolve the global prefix root. This is distinct from `globalBinDir`:
/// users may point global bin symlinks somewhere else while the prefix
/// itself still comes from `AUBE_HOME` / the platform default.
pub fn prefix_dir() -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home);
    }
    data_root()
}

/// Directories a pre-2.0 aube used as its global root, in the order that
/// version consulted them. Read only to warn: aube never installs into,
/// reads packages out of, or deletes anything under a pnpm-owned path.
fn legacy_home_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(v) = std::env::var("PNPM_HOME")
        && !v.is_empty()
    {
        out.push(PathBuf::from(v));
    }
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join("pnpm"));
        }
    } else if cfg!(target_os = "macos")
        && let Some(home) = aube_util::env::home_dir()
    {
        out.push(home.join("Library/pnpm"));
    }
    if !cfg!(windows) {
        match aube_util::env::xdg_data_home() {
            Some(xdg) => out.push(xdg.join("pnpm")),
            None => {
                if let Some(home) = aube_util::env::home_dir() {
                    out.push(home.join(".local/share/pnpm"));
                }
            }
        }
    }
    out
}

/// True when `pkg_dir` holds at least one hash pointer — i.e. at least one
/// global package is installed there.
fn has_global_installs(pkg_dir: &Path) -> bool {
    std::fs::read_dir(pkg_dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
    })
}

/// Warn once per process when the caller has global packages stranded in
/// a pre-2.0 (pnpm-named) location and none in the current one. Without
/// this, `aube list -g` just comes back empty and the bins already on
/// `$PATH` keep working while `remove -g` claims they aren't installed —
/// the failure mode is silent, so the warning is the migration path.
fn warn_on_legacy_global_dir(pkg_dir: &Path, pkg_subdir: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if has_global_installs(pkg_dir) {
            return;
        }
        let Some(legacy) = legacy_home_candidates()
            .into_iter()
            .find(|home| has_global_installs(&home.join(pkg_subdir)))
        else {
            return;
        };
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_GLOBAL_DIR_LEGACY_LOCATION,
            legacy_dir = %legacy.display(),
            current_dir = %pkg_dir.display(),
            "global packages from an older aube are still in {}; aube now keeps its own global \
             directory at {}. Reinstall them with `{}`, or set {}_HOME={} to keep using the old \
             location.",
            legacy.display(),
            pkg_dir.display(),
            aube_util::cmd("add -g <pkg>"),
            aube_util::embedder().env_prefix.unwrap_or("AUBE"),
            legacy.display(),
        );
    });
}

/// Whether `bin_dir` is one of the directories in `path_var`. Compared
/// canonically so a `$PATH` entry that reaches the same directory through a
/// symlink (or a `~`-relative vs absolute spelling) still counts as a
/// match; entries that don't resolve are compared verbatim.
///
/// `None` (an unset `PATH`) is not on `PATH` — nothing is — so it answers
/// `false` rather than being treated as "can't tell, assume fine".
fn bin_dir_on_path(bin_dir: &Path, path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path_var else {
        return false;
    };
    let want = std::fs::canonicalize(bin_dir).unwrap_or_else(|_| bin_dir.to_path_buf());
    std::env::split_paths(path).any(|entry| std::fs::canonicalize(&entry).unwrap_or(entry) == want)
}

/// Warn when `bin_dir` is absent from `$PATH`.
pub fn warn_if_bin_dir_not_on_path(bin_dir: &Path) {
    if bin_dir_on_path(bin_dir, std::env::var_os("PATH").as_deref()) {
        return;
    }
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_GLOBAL_BIN_DIR_NOT_ON_PATH,
        bin_dir = %bin_dir.display(),
        "{} is not on your PATH, so globally installed commands won't be found. Add it to PATH \
         (e.g. `export PATH=\"{}:$PATH\"`), or set globalBinDir to a directory that already is.",
        bin_dir.display(),
        bin_dir.display(),
    );
}

/// Create a fresh install directory under `pkg_dir`. Matches pnpm's naming
/// convention (`<pid-hex>-<time-hex>`) so the dirs sort intuitively and
/// the orphan-cleanup logic can't confuse them with hash pointer symlinks.
pub fn create_install_dir(pkg_dir: &Path) -> miette::Result<PathBuf> {
    std::fs::create_dir_all(pkg_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create global dir {}", pkg_dir.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{:x}-{:x}", std::process::id(), now);
    let dir = pkg_dir.join(name);
    std::fs::create_dir_all(&dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create install dir {}", dir.display()))?;
    Ok(dir)
}

/// Compute a stable hash for a set of aliases plus the registry map. Two
/// `aube add -g` invocations with the same aliases (and registry config)
/// land on the same pointer, so the second overwrites the first.
pub fn cache_key(aliases: &[String], registries: &BTreeMap<String, String>) -> String {
    let mut sorted = aliases.to_vec();
    sorted.sort();
    let registries_vec: Vec<(&String, &String)> = registries.iter().collect();
    let payload = serde_json::json!([sorted, registries_vec]).to_string();
    let digest = Sha256::digest(payload.as_bytes());
    hex::encode(digest)
}

/// Path to the hash pointer (symlink) for a given cache key.
pub fn hash_link(pkg_dir: &Path, hash: &str) -> PathBuf {
    pkg_dir.join(hash)
}

#[derive(Debug, Clone)]
pub struct GlobalPackageInfo {
    pub hash: String,
    pub install_dir: PathBuf,
    /// Aliases from the install dir's `package.json` `dependencies`.
    pub aliases: Vec<String>,
}

/// Walk `pkg_dir`, resolve every symlink entry to its physical install
/// directory, and read the aliases out of that directory's `package.json`.
/// Non-symlinks (raw install dirs) and dangling/broken symlinks are skipped.
pub fn scan_packages(pkg_dir: &Path) -> Vec<GlobalPackageInfo> {
    let Ok(entries) = std::fs::read_dir(pkg_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_symlink() {
            continue;
        }
        let link_path = entry.path();
        // `crate::dirs::canonicalize` strips the Windows `\\?\` verbatim
        // prefix so the `install_dir` we hand back can be compared with
        // `==` / `starts_with` against paths produced by `run_global` (also
        // routed through the same helper). Without this, the prior-cleanup
        // branch in `run_global_inner` never matches on Windows and stale
        // hash pointers / install dirs accumulate.
        let Ok(install_dir) = crate::dirs::canonicalize(&link_path) else {
            continue;
        };
        let manifest_path = install_dir.join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(deps) = json.get("dependencies").and_then(|d| d.as_object()) else {
            continue;
        };
        if deps.is_empty() {
            continue;
        }
        let aliases: Vec<String> = deps.keys().cloned().collect();
        out.push(GlobalPackageInfo {
            hash: entry.file_name().to_string_lossy().into_owned(),
            install_dir,
            aliases,
        });
    }
    out
}

/// Find the global install that owns `alias` (if any). pnpm parity:
/// returns the first match; there should only ever be one because each
/// install is keyed on its alias set.
pub fn find_package(pkg_dir: &Path, alias: &str) -> Option<GlobalPackageInfo> {
    scan_packages(pkg_dir)
        .into_iter()
        .find(|info| info.aliases.iter().any(|a| a == alias))
}

/// Create a symlink (replacing any existing entry). Used both for hash
/// pointers and for global bin entries. Delegates removal to
/// `super::remove_existing` so an entry that happens to be a regular
/// directory or a non-symlink file gets cleaned up correctly instead of
/// silently failing the subsequent create with `EEXIST`.
pub fn symlink_force(target: &Path, link: &Path) -> miette::Result<()> {
    super::remove_existing(link)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
    }
    #[cfg(windows)]
    {
        // Hash pointers target install dirs, so the common path uses
        // `create_dir_link` (an NTFS junction — no Developer Mode
        // required). The non-directory fallback is rare but still
        // goes through the file-symlink syscall, which *does* need
        // Developer Mode until cmd-shim generation lands.
        if target.is_dir() {
            aube_linker::create_dir_link(target, link)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
        } else {
            std::os::windows::fs::symlink_file(target, link)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
        }
    }
    Ok(())
}

/// After a global install lands, link each resolved dependency's bins
/// into `<bin_dir>`. Bins are extracted from each package's `package.json`
/// inside `<install_dir>/node_modules/<alias>/`. Returns the list of bin
/// names that were linked — callers use this list to undo the links on
/// `aube remove -g`.
pub fn link_bins(
    install_dir: &Path,
    bin_dir: &Path,
    aliases: &[String],
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<Vec<String>> {
    std::fs::create_dir_all(bin_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create bin dir {}", bin_dir.display()))?;
    let modules = super::project_modules_dir(install_dir);
    let mut linked = Vec::new();
    for alias in aliases {
        let pkg_dir = modules.join(alias);
        let manifest_path = pkg_dir.join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(bin_field) = json.get("bin") else {
            continue;
        };
        let bins: Vec<(String, String)> = match bin_field {
            serde_json::Value::String(path) => {
                let name = alias.rsplit('/').next().unwrap_or(alias).to_string();
                vec![(name, path.clone())]
            }
            serde_json::Value::Object(map) => map
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => continue,
        };
        for (name, rel) in bins {
            if aube_linker::validate_bin_name(&name).is_err()
                || aube_linker::validate_bin_target(&rel).is_err()
            {
                continue;
            }
            let target = pkg_dir.join(&rel);
            aube_linker::create_bin_shim(bin_dir, &name, &target, shim_opts)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin shim for {name}"))?;
            linked.push(name);
        }
    }
    Ok(linked)
}

/// Remove bin symlinks we own. Only unlinks entries whose symlink target
/// points inside `install_dir` — any bin that was overwritten by a later
/// `aube add -g` is owned by that later install, so we leave it alone.
///
/// Both the target and `install_dir` are canonicalized before the
/// `starts_with` check. On macOS, temp dirs like `/var/folders/...` are
/// actually symlinks to `/private/var/folders/...`; without canonicalizing
/// both sides the comparison always returns false and the bins leak.
pub fn unlink_bins(install_dir: &Path, bin_dir: &Path, bin_names: &[String]) {
    #[cfg(unix)]
    {
        let install_canon = std::fs::canonicalize(install_dir).ok();
        // Lex-normalized `install_dir` is the fallback ownership anchor
        // for regular-file shims (`preferSymlinkedExecutables=false`),
        // where we can't canonicalize the shim's `$basedir/<rel>` target
        // without following the project's symlinks into the shared
        // virtual store.
        let install_lex = aube_linker::normalize_path(install_dir);
        for name in bin_names {
            let link = bin_dir.join(name);
            match std::fs::read_link(&link) {
                Ok(target) => {
                    // Symlink bin: `link_bins` wrote the target as
                    // `<install_dir>/node_modules/<alias>/<rel>`, so the
                    // ownership check is textual for the same reason the
                    // shim branch below is. Canonicalizing first resolves
                    // through `node_modules/<alias>` and `.aube/<dep_path>`
                    // into `<cacheDir>/virtual-store/...` whenever the
                    // global virtual store is on (the default outside CI) —
                    // that lands outside `install_dir`, the ownership check
                    // reads the bin as belonging to another install, and
                    // every global bin leaks as a dangling symlink after
                    // `remove -g` deletes the install dir.
                    let absolute = if target.is_absolute() {
                        target
                    } else {
                        bin_dir.join(target)
                    };
                    let resolved = aube_linker::normalize_path(&absolute);
                    // Full canonicalization stays as a fallback: a bin
                    // linked by an older aube (or a target reached through
                    // a symlinked `install_dir` ancestor) only matches
                    // once both sides are resolved.
                    if resolved.starts_with(&install_lex)
                        || install_canon
                            .as_ref()
                            .is_some_and(|canon| resolved.starts_with(canon))
                        || std::fs::canonicalize(&absolute).is_ok_and(|resolved| {
                            install_canon
                                .as_ref()
                                .is_some_and(|canon| resolved.starts_with(canon))
                        })
                    {
                        let _ = std::fs::remove_file(&link);
                    }
                }
                Err(_) => {
                    // Regular-file shim (`preferSymlinkedExecutables=false`):
                    // read the `# aube-bin-shim` marker line generated
                    // alongside the script body to recover the
                    // `$basedir`-relative target, then lex-normalize from
                    // `bin_dir` to match the shim's string-level
                    // resolution semantics. Canonicalizing here would
                    // follow the install's symlinks into the shared
                    // virtual store, so the ownership check has to
                    // stay textual.
                    let Some(content) = std::fs::read_to_string(&link).ok() else {
                        continue;
                    };
                    let Some(rel) = aube_linker::parse_posix_shim_target(&content) else {
                        continue;
                    };
                    let resolved = aube_linker::normalize_path(&bin_dir.join(rel));
                    if resolved.starts_with(&install_lex)
                        || install_canon
                            .as_ref()
                            .is_some_and(|canon| resolved.starts_with(canon))
                    {
                        let _ = std::fs::remove_file(&link);
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // On Windows, bins are cmd-shim wrapper scripts. Parse the .cmd
        // shim to extract the embedded relative target path and verify
        // it resolves into install_dir before removing — same ownership
        // semantics as the Unix read_link check.
        let Ok(install_canon) = std::fs::canonicalize(install_dir) else {
            return;
        };
        for name in bin_names {
            let cmd_path = bin_dir.join(format!("{name}.cmd"));
            let Ok(content) = std::fs::read_to_string(&cmd_path) else {
                continue;
            };
            // The .cmd shim embeds the target as `"%~dp0\<rel_path>"`.
            // Extract the relative path from the ELSE branch (the one
            // without `.exe`), which looks like:
            //   prog "%~dp0\<rel_target>" %*
            let owned = content
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    // Match the fallback line: `prog "%~dp0\<path>" %*`
                    // Skip lines containing `.exe"` (those are the IF branch).
                    if line.contains("%~dp0\\") && !line.contains(".exe\"") {
                        let start = line.find("%~dp0\\")?;
                        let after = &line[start + 6..]; // skip `%~dp0\`
                        let end = after.find('"')?;
                        Some(after[..end].to_string())
                    } else {
                        None
                    }
                })
                .next();
            if let Some(rel) = owned {
                let resolved = bin_dir.join(&rel);
                if let Ok(resolved) = std::fs::canonicalize(&resolved)
                    && !resolved.starts_with(&install_canon)
                {
                    continue; // owned by a different global install
                }
                // Remove if owned or target no longer exists (stale shim)
            }
            aube_linker::remove_bin_shim(bin_dir, name);
        }
    }
}

/// Enumerate bin names for every alias in an install dir. Used by the
/// remove path to know which symlinks to clean up.
pub fn bin_names_for(install_dir: &Path, aliases: &[String]) -> Vec<String> {
    let modules = super::project_modules_dir(install_dir);
    let mut out = Vec::new();
    for alias in aliases {
        let manifest_path = modules.join(alias).join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(bin_field) = json.get("bin") else {
            continue;
        };
        match bin_field {
            serde_json::Value::String(_) => {
                out.push(alias.rsplit('/').next().unwrap_or(alias).to_string());
            }
            serde_json::Value::Object(map) => {
                for name in map.keys() {
                    out.push(name.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// Delete a global package: remove its bins, its hash pointer, and the
/// physical install directory.
///
/// Both sides of the containment check are canonicalized. `info.install_dir`
/// already comes out of `scan_packages` in canonical form, but `layout.pkg_dir`
/// may still be in whatever shape `GlobalLayout::resolve()` produced (on
/// macOS that's typically an un-canonicalized `/var/folders/...` path
/// that's actually a symlink to `/private/var/folders/...`). Without
/// normalizing here, `starts_with` silently returns false and the
/// physical install dir leaks.
pub fn remove_package(info: &GlobalPackageInfo, layout: &GlobalLayout) -> miette::Result<()> {
    let bins = bin_names_for(&info.install_dir, &info.aliases);
    unlink_bins(&info.install_dir, &layout.bin_dir, &bins);

    // Remove the hash pointer first. A missing pointer is fine (the
    // caller may have already cleaned it up), but permission denied or
    // similar means the package is still findable and we must not
    // report success. `super::remove_existing` handles the Windows
    // directory-junction case where `remove_file` fails with
    // `Access is denied`; we created the pointer via `create_dir_link`
    // (NTFS junction), so a plain `remove_file` here would leak it.
    let hash_ptr = hash_link(&layout.pkg_dir, &info.hash);
    super::remove_existing(&hash_ptr)?;

    // `crate::dirs::canonicalize` so `pkg_canon` is comparable with the
    // `info.install_dir` `scan_packages` produced — both must be in the
    // same Windows form (no `\\?\` prefix) or the `starts_with` check
    // fails and the install dir leaks.
    let pkg_canon =
        crate::dirs::canonicalize(&layout.pkg_dir).unwrap_or_else(|_| layout.pkg_dir.clone());
    if info.install_dir.starts_with(&pkg_canon) {
        match std::fs::remove_dir_all(&info.install_dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).into_diagnostic().wrap_err_with(|| {
                    format!(
                        "failed to remove install dir {}",
                        info.install_dir.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_across_alias_order() {
        let regs: BTreeMap<String, String> = [(
            "default".to_string(),
            "https://registry.npmjs.org/".to_string(),
        )]
        .into_iter()
        .collect();
        let a = cache_key(&["lodash".into(), "chalk".into()], &regs);
        let b = cache_key(&["chalk".into(), "lodash".into()], &regs);
        assert_eq!(a, b);
    }

    #[test]
    fn bin_dir_on_path_matches_a_listed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths(["/usr/bin".as_ref(), bin.as_os_str()]).unwrap();
        assert!(bin_dir_on_path(&bin, Some(&path)));
    }

    #[test]
    fn bin_dir_on_path_rejects_an_absent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths(["/usr/bin"]).unwrap();
        assert!(!bin_dir_on_path(&bin, Some(&path)));
    }

    /// An unset `PATH` means the bin is unreachable, so `add -g` must still
    /// warn — the check can't quietly pass because it has nothing to search.
    #[test]
    fn bin_dir_on_path_is_false_when_path_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!bin_dir_on_path(dir.path(), None));
        assert!(!bin_dir_on_path(dir.path(), Some(std::ffi::OsStr::new(""))));
    }

    #[test]
    fn cache_key_changes_with_aliases() {
        let regs = BTreeMap::new();
        let a = cache_key(&["lodash".into()], &regs);
        let b = cache_key(&["chalk".into()], &regs);
        assert_ne!(a, b);
    }
}
