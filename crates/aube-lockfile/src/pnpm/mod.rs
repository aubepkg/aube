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

/// Whether an `overrides` entry for package `name` has the value
/// `specifier`. Only the selector's last segment has to name the package;
/// its version range and parents aren't checked. An override for another
/// package with the same value doesn't count.
pub(super) fn override_sets(
    overrides: &std::collections::BTreeMap<String, String>,
    name: &str,
    specifier: &str,
) -> bool {
    overrides
        .iter()
        .any(|(key, value)| value == specifier && override_target_name(key) == name)
}

/// The package named by an override selector's last segment: `bar` for
/// `bar`, `bar@^1`, or `foo>bar@<2`. As in pnpm, a `>` after a space, `|`,
/// or `@` belongs to a version range, not a parent boundary.
fn override_target_name(key: &str) -> &str {
    let bytes = key.as_bytes();
    let start = (1..bytes.len())
        .rev()
        .find(|&i| bytes[i] == b'>' && !matches!(bytes[i - 1], b' ' | b'|' | b'@'))
        .map_or(0, |i| i + 1);
    let segment = &key[start..];
    // Skip a leading `@` so a scope isn't read as a version separator.
    match segment.get(1..).and_then(|rest| rest.find('@')) {
        Some(at) => &segment[..=at],
        None => segment,
    }
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
