mod checksum;
mod dep_path;
mod format;
mod raw;
mod read;
mod subset;
mod write;

#[cfg(test)]
mod tests;

pub use checksum::{package_extensions_checksum, pnpmfile_checksum};
pub use read::{parse, parse_with_options};
pub use write::{registry_tarball_url_is_not_derivable, write};

/// Benchmark-only shims comparing the byte-cursor subset parser against
/// the general `yaml_serde` parser on raw `pnpm-lock.yaml` content.
/// Returns `(packages, snapshots, importers)` counts so the bench's
/// black-box can't be optimized away. Gated behind the `bench` feature
/// so they are not part of the crate's default public surface.
#[cfg(feature = "bench")]
#[doc(hidden)]
pub fn __bench_parse_subset(content: &str) -> Option<(usize, usize, usize)> {
    subset::try_parse(content).map(|r| raw::__bench_counts(&r))
}

/// Benchmark-only: parse via the original serde path.
#[cfg(feature = "bench")]
#[doc(hidden)]
pub fn __bench_parse_serde(content: &str) -> Option<(usize, usize, usize)> {
    raw::parse_raw_lockfile_serde(content)
        .ok()
        .map(|r| raw::__bench_counts(&r))
}

/// Benchmark-only: the full serialize + reformat + atomic-write path —
/// exactly the work the lockfile-write-overlap optimization moves onto a
/// background thread. Measured against `LockfileGraph::clone()` (the
/// offsetting cost the spawned task pays) to decide whether overlapping
/// the write with the link tail is a net win.
#[cfg(feature = "bench")]
#[doc(hidden)]
pub fn __bench_write_to(
    path: &std::path::Path,
    graph: &crate::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) {
    write::write(path, graph, manifest).expect("bench write");
}

/// Whether an `overrides` entry sets importer direct dependency `name` to
/// `specifier`. The selector has to name the package, with or without a
/// version range, and have no parent: an importer's direct dependencies
/// have no parent package, so `parent>name` never applies to them. An
/// override for another package with the same value doesn't count.
pub(super) fn override_sets_direct_dep(
    overrides: &std::collections::BTreeMap<String, String>,
    name: &str,
    specifier: &str,
) -> bool {
    overrides
        .iter()
        .any(|(key, value)| value == specifier && parentless_override_target(key) == Some(name))
}

/// The package a parentless override selector names: `bar` for `bar` or
/// `bar@^1`, and `None` for `foo>bar`. As in pnpm, a `>` after a space,
/// `|`, or `@` belongs to a version range, not a parent boundary.
fn parentless_override_target(key: &str) -> Option<&str> {
    let bytes = key.as_bytes();
    if (1..bytes.len()).any(|i| bytes[i] == b'>' && !matches!(bytes[i - 1], b' ' | b'|' | b'@')) {
        return None;
    }
    // Skip a leading `@` so a scope isn't read as a version separator.
    Some(match key.get(1..).and_then(|rest| rest.find('@')) {
        Some(at) => &key[..=at],
        None => key,
    })
}

pub(super) fn tarball_url_is_hosted_git(url: &str) -> bool {
    let Some((host, path)) = http_url_host_and_path(url) else {
        return false;
    };
    match host.as_str() {
        "codeload.github.com" | "npm.pkg.github.com" => true,
        "gitlab.com" => path.contains("/-/archive/"),
        "bitbucket.org" => path.contains("/get/"),
        _ => false,
    }
}

fn http_url_host_and_path(url: &str) -> Option<(String, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let before_query = rest.split_once('?').map_or(rest, |(before, _)| before);
    let before_fragment = before_query
        .split_once('#')
        .map_or(before_query, |(before, _)| before);
    let (authority, path) = before_fragment
        .split_once('/')
        .unwrap_or((before_fragment, ""));
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_port
        .split_once(':')
        .map_or(host_port, |(host, _)| host)
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some((host, path))
    }
}
