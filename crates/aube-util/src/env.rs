use std::path::PathBuf;

use crate::identity::embedder;

/// Whether a *branded* settings env-var alias (the tool-prefixed form like
/// `AUBE_NODE_LINKER`) should be read, given the active embedder's
/// [`env_prefix`](crate::identity::Embedder::env_prefix).
///
/// aube's settings table declares each branded env alias as `{PREFIX}_<NAME>`
/// alongside the neutral `npm_config_*` / `NPM_CONFIG_*` forms and a handful of
/// bare external vars (`CI`, `HTTP_PROXY`, `NODE_OPTIONS`, …). `env_prefix` is
/// the single on/off switch for the *branded* surface only:
///
/// - `Some(prefix)` — a branded alias is read only when it is `{prefix}_…`.
///   Standalone aube (`Some("AUBE")`) thus reads every `AUBE_*` settings var
///   exactly as before, and nothing else changes.
/// - `None` — the embedder reads *no* branded settings env vars; every
///   tool-branded alias is skipped.
///
/// The neutral `npm_config_*` / `NPM_CONFIG_*` aliases and the bare external
/// vars are never the tool's brand and are always honored. Standalone aube's
/// settings table only ever emits its own `env_prefix` as the branded prefix,
/// so the brand family is exactly the `{prefix}_*` set.
pub fn branded_env_alias_enabled(alias: &str) -> bool {
    // npm-compat family — never the tool's brand, always honored.
    if alias.starts_with("npm_config_") || alias.starts_with("NPM_CONFIG_") {
        return true;
    }
    // Bare external/neutral vars — not part of any tool's brand family.
    if !looks_branded(alias) {
        return true;
    }
    // A branded-shaped alias: read it only when it matches the active prefix.
    match embedder().env_prefix {
        Some(prefix) => alias
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('_')),
        None => false,
    }
}

/// Does `alias` have the `<UPPER_PREFIX>_<NAME>` shape of a tool-branded env
/// var, as opposed to a bare external var (`CI`) or neutral proxy/Node var
/// (`HTTP_PROXY`, `NODE_OPTIONS`)? aube's settings table only ever emits its
/// own `env_prefix` as the branded prefix, so this just has to separate the
/// branded family from the recognized neutral vars.
fn looks_branded(alias: &str) -> bool {
    const NEUTRAL: &[&str] = &[
        "CI",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "PROXY",
        "NODE_OPTIONS",
    ];
    if NEUTRAL.contains(&alias) {
        return false;
    }
    match alias.split_once('_') {
        Some((head, _)) if !head.is_empty() => head.chars().all(|c| c.is_ascii_uppercase()),
        _ => false,
    }
}

/// Read a tool-prefixed *non-settings, non-user-facing* env toggle through the
/// active embedder's [`env_prefix`](crate::identity::Embedder::env_prefix). For
/// standalone aube (`Some("AUBE")`) `embedder_env("DISABLE_CLONEDIR")` reads
/// `AUBE_DISABLE_CLONEDIR`; for an embedder with `env_prefix = None` (a host
/// that exposes no branded debug surface) it reads nothing and returns `None`,
/// so no branded debug/perf/diag toggle leaks under the embedding host's brand.
///
/// This is for the dev/debug/perf-bisect/diagnostic toggles that are NOT
/// user-facing config — `AUBE_DISABLE_*`, `AUBE_DIAG_*`, `AUBE_CAS_*`,
/// `AUBE_INTERNAL_*`, `AUBE_BENCH_*`, the self-update endpoints, … User-facing
/// config knobs go through [`config_env`] instead, and settings-table branded
/// aliases through [`branded_env_alias_enabled`]. Additive and no-op for
/// standalone aube: an embedder that registers nothing reads exactly the
/// `AUBE_*` forms it read before.
pub fn embedder_env(suffix: &str) -> Option<std::ffi::OsString> {
    let prefix = embedder().env_prefix?;
    std::env::var_os(format!("{prefix}_{suffix}"))
}

/// Read one of the tool's *first-class config* env knobs through the active
/// embedder's [`config_env_prefix`](crate::identity::Embedder::config_env_prefix).
/// For standalone aube (`Some("AUBE")`) `config_env("CACHE_DIR")` reads
/// `AUBE_CACHE_DIR`; for an embedder with `config_env_prefix = Some("NUB")` it
/// reads `NUB_CACHE_DIR`. `None` reads nothing.
///
/// This is the deliberate, minimal exception to the debug-toggle gate: the
/// handful of knobs a host legitimately wants under its OWN brand — the cache
/// dir, the fetch concurrency — rather than hidden. Distinct from
/// [`embedder_env`]: that family vanishes under an embedder with no
/// `env_prefix`; this family follows the host's `config_env_prefix`, so a host
/// reads its own brand for exactly these knobs and the branded `AUBE_*` form is
/// never read under it.
pub fn config_env(suffix: &str) -> Option<std::ffi::OsString> {
    let prefix = embedder().config_env_prefix?;
    std::env::var_os(format!("{prefix}_{suffix}"))
}

pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(h.into());
    }
    #[cfg(windows)]
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Some(h.into());
    }
    None
}

fn non_empty_path_var(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn xdg_config_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CONFIG_HOME")
}

pub fn xdg_data_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_DATA_HOME")
}

pub fn xdg_cache_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CACHE_HOME")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under the default (AUBE) profile — `env_prefix = Some("AUBE")` — every
    /// settings env alias aube's table declares is honored: the branded
    /// `AUBE_*` form, the neutral `npm_config_*` / `NPM_CONFIG_*` forms, and
    /// the bare external vars. This is the standalone-neutrality contract for
    /// the env-prefix gate: a binary that registers no profile reads exactly
    /// what aube read before the gate existed.
    #[test]
    fn aube_profile_honors_every_settings_env_family() {
        // Branded family (the tool's own prefix).
        assert!(branded_env_alias_enabled("AUBE_NODE_LINKER"));
        assert!(branded_env_alias_enabled("AUBE_NO_LOCK"));
        assert!(branded_env_alias_enabled("AUBE_LINK_CONCURRENCY"));
        // npm-compat family — never gated.
        assert!(branded_env_alias_enabled("npm_config_node_linker"));
        assert!(branded_env_alias_enabled("NPM_CONFIG_NODE_LINKER"));
        // Bare external / neutral vars — never gated.
        assert!(branded_env_alias_enabled("CI"));
        assert!(branded_env_alias_enabled("HTTP_PROXY"));
        assert!(branded_env_alias_enabled("NODE_OPTIONS"));
    }

    /// Under the default (AUBE) profile — `env_prefix = Some("AUBE")`,
    /// `config_env_prefix = Some("AUBE")` — both helpers compose the prefix onto
    /// the suffix and read the resulting `AUBE_*` var. This is the
    /// standalone-neutrality contract: a binary that registers no profile reads
    /// exactly the `AUBE_*` forms it read before the helpers existed. Tests run
    /// serially (`RUST_TEST_THREADS=1`) and restore the prior value so they
    /// don't bleed into the next test.
    ///
    /// The `None`-prefix branch (an embedder that hides a family → the helper
    /// returns `None`) can't be exercised here without `set_embedder`, which
    /// would flip the process-global fallback the default-profile tests rely on;
    /// it's covered by the `embedder_env_brand_gate` integration test, which
    /// registers a real non-aube profile in its own process.
    #[test]
    fn embedder_and_config_env_read_aube_prefixed_under_default_profile() {
        // RAII guard so a panic in `f()` still restores the prior value —
        // a bare restore-after-`f()` would leak the var on panic and flake
        // the next serial test.
        struct EnvGuard {
            key: String,
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: tests run serially via RUST_TEST_THREADS=1.
                unsafe {
                    match &self.prev {
                        Some(v) => std::env::set_var(&self.key, v),
                        None => std::env::remove_var(&self.key),
                    }
                }
            }
        }
        fn with_var<F: FnOnce()>(key: &str, value: &str, f: F) {
            let _guard = EnvGuard {
                key: key.to_string(),
                prev: std::env::var_os(key),
            };
            // SAFETY: tests run serially via RUST_TEST_THREADS=1.
            unsafe { std::env::set_var(key, value) };
            f();
        }

        with_var("AUBE_DISABLE_CLONEDIR", "1", || {
            assert_eq!(
                embedder_env("DISABLE_CLONEDIR").as_deref(),
                Some(std::ffi::OsStr::new("1")),
            );
        });
        with_var("AUBE_CACHE_DIR", "/tmp/x", || {
            assert_eq!(
                config_env("CACHE_DIR").as_deref(),
                Some(std::ffi::OsStr::new("/tmp/x")),
            );
        });
    }

    /// `looks_branded` separates the tool-branded `<UPPER>_<NAME>` shape from
    /// the recognized neutral/external vars, so the `None`-prefix embedder
    /// skips exactly the branded family and nothing else.
    #[test]
    fn looks_branded_distinguishes_brand_from_neutral() {
        assert!(looks_branded("AUBE_NODE_LINKER"));
        assert!(looks_branded("FOO_BAR")); // any UPPER-prefixed var reads as branded
        assert!(!looks_branded("CI"));
        assert!(!looks_branded("HTTP_PROXY"));
        assert!(!looks_branded("NODE_OPTIONS"));
        assert!(!looks_branded("npm_config_node_linker")); // lowercase head
    }
}
