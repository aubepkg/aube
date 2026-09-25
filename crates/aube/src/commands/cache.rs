//! `aube cache` — inspect and manage the on-disk packument metadata cache.
//!
//! Mirrors `pnpm cache`'s subcommand surface (`list`, `delete`, `view`,
//! `list-registries`) over aube's two packument cache directories:
//!
//! - `~/.cache/aube/packuments-v1/`      — abbreviated (corgi) packuments
//!   used by the resolver
//! - `~/.cache/aube/packuments-full-v1/` — full packuments used by
//!   `aube view`
//!
//! Registry partitions use `origin-<hash>/` directories. Compact exact entries
//! live under `exact-v1/origin-<hash>/<safe_name>/<version_hash>.json`.
//!
//! Full and abbreviated files are named `<safe_name>.json` where `/` in scoped names is
//! replaced by `__`. The on-disk shape is `{ etag, last_modified,
//! fetched_at, packument }` where `packument` is either a parsed
//! `Packument` (corgi cache) or raw JSON (full cache).
//!
//! This is a read-only / file-deletion command — no project lock,
//! no lockfile, no node_modules.

use glob::Pattern;
use miette::{IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, usage_rs::Args)]
pub struct CacheArgs {
    #[usage(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, usage_rs::Subcommands)]
pub enum CacheCommand {
    /// Delete metadata cache for the specified package(s).
    ///
    /// Supports glob patterns; matches against the package name (e.g.
    /// `lodash`, `@babel/*`).
    Delete(DeleteArgs),
    /// List the available packages in the metadata cache.
    ///
    /// Optional glob filters narrow the result; with no filter every
    /// cached package is listed.
    List(ListArgs),
    /// List configured registries from the project + user `.npmrc`.
    ///
    /// Prints the registries currently configured for this project,
    /// rather than the registry partitions present in the cache.
    ListRegistries,
    /// Print the directory used for metadata and policy caches.
    Path,
    /// Remove stale extracted primer files from the metadata cache.
    Prune(PruneArgs),
    /// View the cached metadata for a single package.
    ///
    /// Prints a summary (versions, dist-tags, ETag, fetched-at) by
    /// default; `--json` dumps the raw cache file.
    View(ViewArgs),
}

#[derive(Debug, usage_rs::Args)]
pub struct DeleteArgs {
    /// One or more package name patterns.
    ///
    /// Glob metacharacters (`*`, `?`, `[...]`) are supported.
    #[usage(arg, required)]
    pub patterns: Vec<String>,
}

#[derive(Debug, usage_rs::Args)]
pub struct ListArgs {
    /// Optional glob patterns to filter the listing.
    ///
    /// With no patterns, every cached package is printed.
    pub patterns: Vec<String>,
}

#[derive(Debug, usage_rs::Args)]
pub struct ViewArgs {
    /// Package name (scoped names like `@babel/core` are accepted).
    pub name: String,
    /// Dump the raw on-disk cache JSON instead of a summary.
    #[usage(long)]
    pub json: bool,
}

#[derive(Debug, usage_rs::Args)]
pub struct PruneArgs {
    /// Minimum age in days before an old primer file is removed.
    #[usage(long, default_value_t = 30, default = "30")]
    pub age_days: u64,
    /// Do not actually delete anything.
    #[usage(long)]
    pub dry_run: bool,
}

pub async fn run(args: CacheArgs) -> miette::Result<()> {
    match args.command {
        CacheCommand::List(a) => list(a),
        CacheCommand::Delete(a) => delete(a),
        CacheCommand::Prune(a) => prune(a),
        CacheCommand::View(a) => view(a),
        CacheCommand::ListRegistries => list_registries(),
        CacheCommand::Path => path(),
    }
}

fn path() -> miette::Result<()> {
    let cwd = super::metadata_cache_anchor()?;
    println!("{}", super::resolved_cache_dir(&cwd).display());
    Ok(())
}

/// Both packument cache directories. Returned in a fixed order so
/// list/delete output is deterministic. Either or both may be missing
/// if the user has never run a command that populates them.
fn cache_dirs() -> Vec<(&'static str, PathBuf)> {
    vec![
        ("corgi", super::packument_cache_dir()),
        ("full", super::packument_full_cache_dir()),
    ]
}

/// Reverse the `safe_name` encoding used when writing cache files:
/// `/` is mapped to `__` in non-scoped names, but only the first `__`
/// of a scoped name is the scope/pkg separator (see `find_hash::split_stem`
/// for the same trick on a different filename shape).
fn decode_safe_name(stem: &str) -> String {
    if let Some(rest) = stem.strip_prefix('@')
        && let Some(sep) = rest.find("__")
    {
        return format!("@{}/{}", &rest[..sep], &rest[sep + 2..]);
    }
    stem.to_string()
}

/// Collect legacy, registry-partitioned, and compact exact-version entries.
/// Keep the actual paths: a package can have entries in several registries.
/// Only known directory layouts are traversed, and symlinks are never followed.
fn collect_entries(dir: &Path) -> miette::Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut entries = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut pending = vec![(dir.to_path_buf(), Vec::<String>::new())];
    while let Some((directory, components)) = pending.pop() {
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).into_diagnostic(),
        };
        for child in children {
            let child = child.into_diagnostic()?;
            let file_type = child.file_type().into_diagnostic()?;
            let Some(filename) = child.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if file_type.is_dir() {
                let descend = match components.as_slice() {
                    [] => filename.starts_with("origin-") || filename == "exact-v1",
                    [exact] if exact == "exact-v1" => filename.starts_with("origin-"),
                    [exact, origin] if exact == "exact-v1" && origin.starts_with("origin-") => {
                        aube_store::validate_and_encode_name(&filename.replace("%2F", "/"))
                            .is_some()
                            || aube_store::validate_and_encode_name(&decode_safe_name(&filename))
                                .is_some_and(|encoded| encoded == filename)
                    }
                    _ => false,
                };
                if descend {
                    let mut next = components.clone();
                    next.push(filename);
                    pending.push((child.path(), next));
                }
            } else if file_type.is_file() && filename.ends_with(".json") {
                let encoded = match components.as_slice() {
                    [] => filename.strip_suffix(".json").unwrap_or(&filename),
                    [origin] if origin.starts_with("origin-") => {
                        filename.strip_suffix(".json").unwrap_or(&filename)
                    }
                    [exact, origin, package]
                        if exact == "exact-v1" && origin.starts_with("origin-") =>
                    {
                        package
                    }
                    _ => continue,
                };
                let name = if components.len() == 3 && encoded.contains("%2F") {
                    encoded.replace("%2F", "/")
                } else if encoded.starts_with('@') && encoded.matches("__").count() > 1 {
                    // Old slash encoding is ambiguous when scopes contain __.
                    // Read the stored identity instead of guessing which slash
                    // belongs to the scope. Unreadable ambiguous entries have
                    // no name: wildcard deletion can still clean them up.
                    std::fs::read(child.path())
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .and_then(|value| {
                            value
                                .pointer("/packument/name")
                                .or_else(|| value.pointer("/exact/metadata/name"))
                                .and_then(|name| name.as_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_default()
                } else {
                    decode_safe_name(encoded)
                };
                if name.is_empty()
                    || aube_store::validate_and_encode_name(&name)
                        .is_some_and(|safe| safe == encoded || name.replace('/', "%2F") == encoded)
                {
                    entries.entry(name).or_default().push(child.path());
                }
            }
        }
    }
    for paths in entries.values_mut() {
        paths.sort();
    }
    Ok(entries)
}

fn compile_patterns(raw: &[String]) -> miette::Result<Vec<Pattern>> {
    raw.iter()
        .map(|p| {
            Pattern::new(p)
                .into_diagnostic()
                .map_err(|e| miette!("invalid pattern `{p}`: {e}"))
        })
        .collect()
}

fn matches_any(name: &str, patterns: &[Pattern]) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| p.matches(name))
}

fn list(args: ListArgs) -> miette::Result<()> {
    let patterns = compile_patterns(&args.patterns)?;
    let mut all = BTreeSet::new();
    for (_, dir) in cache_dirs() {
        all.extend(
            collect_entries(&dir)?
                .into_keys()
                .filter(|name| !name.is_empty()),
        );
    }
    for name in all.iter().filter(|n| matches_any(n, &patterns)) {
        println!("{name}");
    }
    Ok(())
}

fn delete(args: DeleteArgs) -> miette::Result<()> {
    let patterns = compile_patterns(&args.patterns)?;
    let mut deleted = 0usize;
    for (_, dir) in cache_dirs() {
        if !dir.exists() {
            continue;
        }
        for (name, paths) in collect_entries(&dir)? {
            if !matches_any(&name, &patterns) {
                continue;
            }
            for path in paths {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        println!("removed {}", path.display());
                        deleted += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(miette!("failed to remove {}: {e}", path.display()));
                    }
                }
            }
        }
    }
    if deleted == 0 {
        return Err(miette!("no cached packages matched the given pattern(s)"));
    }
    Ok(())
}

fn prune(args: PruneArgs) -> miette::Result<()> {
    let age = std::time::Duration::from_secs(args.age_days.saturating_mul(24 * 60 * 60));
    let stats = aube_resolver::prune_primer_cache(args.dry_run, age)
        .into_diagnostic()
        .map_err(|e| miette!("failed to prune primer cache: {e}"))?;
    let label = if args.dry_run {
        "would prune"
    } else {
        "pruned"
    };
    println!(
        "primer cache {label} {} file(s), {} byte(s)",
        stats.files, stats.bytes
    );
    Ok(())
}

fn view(args: ViewArgs) -> miette::Result<()> {
    // Validate the user-supplied name against the npm grammar before
    // it is used to select entries. Slash replacement alone would let
    // `aube cache view ../../evil` escape the cache directory.
    aube_store::validate_and_encode_name(&args.name)
        .ok_or_else(|| miette!("invalid package name: {:?}", args.name))?;

    // Probe both directories. The corgi cache has a richer schema we can
    // pretty-print; the full cache is opaque JSON we just dump.
    let mut found = false;
    for (kind, dir) in cache_dirs() {
        for path in collect_entries(&dir)?
            .remove(&args.name)
            .unwrap_or_default()
        {
            let bytes = std::fs::read(&path)
                .into_diagnostic()
                .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;

            if args.json {
                found = true;
                // Dump verbatim. We've already read the bytes; printing them
                // as a string keeps formatting whatever the cache writer chose.
                let s = String::from_utf8_lossy(&bytes);
                println!("# {} ({kind})", path.display());
                println!("{s}");
                continue;
            }

            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "skipping corrupt metadata cache entry");
                    continue;
                }
            };
            found = true;
            print_summary(&args.name, kind, &path, &value);
        }
    }
    if !found {
        return Err(miette!(
            "no cached metadata for `{}`\nhelp: run `{} {}` or `{}` first to populate the cache",
            args.name,
            aube_util::cmd("view"),
            args.name,
            aube_util::cmd("install"),
        ));
    }
    Ok(())
}

/// Print a small human-readable digest of a cache entry. Both cache
/// shapes share the outer `{ etag, last_modified, fetched_at, packument }`
/// envelope, so the wrapping fields render the same way; only the
/// inner `packument` shape differs.
fn print_summary(name: &str, kind: &str, path: &Path, value: &serde_json::Value) {
    println!("{name} ({kind})");
    println!("  path:          {}", path.display());
    if let Some(etag) = value.get("etag").and_then(|v| v.as_str()) {
        println!("  etag:          {etag}");
    }
    if let Some(lm) = value.get("last_modified").and_then(|v| v.as_str()) {
        println!("  last-modified: {lm}");
    }
    if let Some(ts) = value.get("fetched_at").and_then(|v| v.as_u64()) {
        println!("  fetched-at:    {ts} (unix seconds)");
    }
    if let Some(version) = value
        .pointer("/exact/metadata/version")
        .and_then(|v| v.as_str())
    {
        println!("  version:       {version}");
    }
    let pack = value.get("packument");
    if let Some(versions) = pack
        .and_then(|p| p.get("versions"))
        .and_then(|v| v.as_object())
    {
        println!("  versions:      {}", versions.len());
        if let Some(highest) = highest_semver(versions.keys().map(String::as_str)) {
            println!("  highest:       {highest}");
        }
    }
    if let Some(tags) = pack
        .and_then(|p| p.get("dist-tags").or_else(|| p.get("dist_tags")))
        .and_then(|v| v.as_object())
    {
        println!("  dist-tags:");
        for (tag, ver) in tags {
            if let Some(s) = ver.as_str() {
                println!("    {tag}: {s}");
            }
        }
    }
}

/// Pick the highest version from an iterator of version strings using
/// semver ordering — *not* the lexicographic order that BTreeMap key
/// iteration would give us, which incorrectly ranks `"9.0.0"` above
/// `"10.0.0"`. Versions that fail to parse are skipped; if none parse,
/// we fall back to the lexicographic max so we still print *something*
/// useful for unusual registries (e.g. date-stamped pre-releases).
fn highest_semver<'a, I: IntoIterator<Item = &'a str>>(versions: I) -> Option<String> {
    let all: Vec<&str> = versions.into_iter().collect();
    let parsed_max = all
        .iter()
        .filter_map(|v| node_semver::Version::parse(v).ok().map(|p| (p, *v)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, s)| s.to_string());
    parsed_max.or_else(|| all.iter().max().map(|s| s.to_string()))
}

fn list_registries() -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd()?;
    let config = aube_registry::config::NpmConfig::load(&cwd);
    println!("default: {}", config.registry);
    for (scope, url) in &config.scoped_registries {
        println!("{scope}: {url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_round_trip_unscoped() {
        assert_eq!(decode_safe_name("lodash"), "lodash");
        assert_eq!(
            aube_store::validate_and_encode_name("lodash").as_deref(),
            Some("lodash")
        );
    }

    #[test]
    fn safe_name_round_trip_scoped() {
        assert_eq!(decode_safe_name("@babel__core"), "@babel/core");
        assert_eq!(
            aube_store::validate_and_encode_name("@babel/core").as_deref(),
            Some("@babel__core")
        );
    }

    #[test]
    fn safe_name_preserves_double_underscore_in_unscoped() {
        // Non-scoped names can legitimately contain `__`; the encoding
        // only collides for scoped names, where the first `__` is the
        // scope/pkg separator.
        assert_eq!(decode_safe_name("foo__bar"), "foo__bar");
    }

    #[test]
    fn collect_entries_handles_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let names = collect_entries(&tmp.path().join("does-not-exist")).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn collect_entries_decodes_filenames() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lodash.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("@babel__core.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("README"), "ignored").unwrap();
        let names = collect_entries(tmp.path()).unwrap();
        assert!(names.contains_key("lodash"));
        assert!(names.contains_key("@babel/core"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn matches_any_no_patterns_matches_everything() {
        assert!(matches_any("anything", &[]));
    }

    #[test]
    fn highest_semver_beats_lexicographic_order() {
        // Regression: BTreeMap key iteration sorts lexicographically,
        // which puts "9.0.0" *after* "10.0.0". The summary needs to
        // report the actual semver max.
        let v = ["1.0.0", "9.0.0", "10.0.0", "2.5.3"];
        assert_eq!(highest_semver(v.iter().copied()), Some("10.0.0".into()));
    }

    #[test]
    fn highest_semver_falls_back_when_nothing_parses() {
        let v = ["not-semver-a", "not-semver-b"];
        assert_eq!(
            highest_semver(v.iter().copied()),
            Some("not-semver-b".into())
        );
    }

    #[test]
    fn matches_any_glob() {
        let pats = compile_patterns(&["@babel/*".into()]).unwrap();
        assert!(matches_any("@babel/core", &pats));
        assert!(!matches_any("lodash", &pats));
    }
}
