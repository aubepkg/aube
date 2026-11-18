use super::{blank_trailing_commas, declares_next_format, parse_document};
use crate::pnpm::parse;
use proptest::prelude::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pnpm-v13")
        .join(name)
}

/// The graph a lockfile parses into, compared through its `Debug`
/// rendering since `LockfileGraph` has no `PartialEq`. The format it was
/// read from is left out.
fn graph_of(path: &Path) -> String {
    let graph = parse(path).unwrap_or_else(|err| panic!("{}: {err:?}", path.display()));
    debug_without_format(graph)
}

fn graph_of_content(content: &str) -> Result<String, crate::Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, content).unwrap();
    parse(&path).map(debug_without_format)
}

fn debug_without_format(mut graph: crate::LockfileGraph) -> String {
    graph.settings.pnpm_lockfile_format = None;
    format!("{graph:#?}")
}

/// Each fixture pair is what pnpm 12.21 wrote for one project, once with
/// `lockfile.format: v13` in `pnpm-workspace.yaml` and once without.
fn assert_same_graph(project: &str) {
    let v9 = graph_of(&fixture(&format!("{project}.v9.yaml")));
    let v13 = graph_of(&fixture(&format!("{project}.v13.yaml")));
    assert!(
        v9.lines()
            .any(|line| line.contains('@') && line.contains('(')),
        "{project}: the fixture has no peer suffixes"
    );
    if v9 != v13 {
        let diff: Vec<String> = v9
            .lines()
            .zip(v13.lines())
            .enumerate()
            .filter(|(_, (left, right))| left != right)
            .take(20)
            .map(|(line, (left, right))| format!("{line}:\n  v9:  {left}\n  v13: {right}"))
            .collect();
        panic!(
            "{project}: the v13 lockfile reads into another graph ({} vs {} lines)\n{}",
            v9.lines().count(),
            v13.lines().count(),
            diff.join("\n")
        );
    }
}

#[test]
fn v13_with_peers_reads_like_v9() {
    assert_same_graph("peers");
}

#[test]
fn v13_with_catalogs_patches_and_nested_peers_reads_like_v9() {
    assert_same_graph("nested");
}

#[test]
fn v13_with_dedupe_peers_reads_like_v9() {
    assert_same_graph("dedupe");
}

/// The YAML form of the format reads like the JSON form pnpm writes.
#[test]
fn v13_yaml_form_reads_like_json_form() {
    let json = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    let document = parse_document(&json).unwrap();
    let yaml = yaml_serde::to_string(&document).unwrap();
    assert!(declares_next_format(&yaml), "{yaml}");
    assert_eq!(
        graph_of_content(&yaml).unwrap(),
        graph_of(&fixture("peers.v13.yaml"))
    );
}

/// pnpm writes the env lockfile as a leading document of the same file.
#[test]
fn v13_after_env_document_reads_like_v9() {
    let json = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    let content = format!("---\nlockfileVersion: '9.0'\nimporters: {{}}\n---\n{json}");
    assert_eq!(
        graph_of_content(&content).unwrap(),
        graph_of(&fixture("peers.v9.yaml"))
    );
}

/// A lone document that opens with `---` is the lockfile itself, not an
/// env document with nothing after it.
#[test]
fn v13_yaml_after_document_start_reads_like_json_form() {
    let json = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    let yaml = yaml_serde::to_string(&parse_document(&json).unwrap()).unwrap();
    assert_eq!(
        graph_of_content(&format!("---\n{yaml}")).unwrap(),
        graph_of(&fixture("peers.v13.yaml"))
    );
}

/// Blank and comment lines before the env document don't hide it.
#[test]
fn v13_after_commented_env_document_reads_like_v9() {
    let json = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    let content =
        format!("# pinned by pnpm\n\n---\nlockfileVersion: '9.0'\nimporters: {{}}\n---\n{json}");
    assert_eq!(
        graph_of_content(&content).unwrap(),
        graph_of(&fixture("peers.v9.yaml"))
    );
}

/// A root-level dependency block is the pre-v9 layout under any version,
/// so it is refused rather than dropped.
#[test]
fn v13_with_root_dependencies_is_refused() {
    let err =
        graph_of_content(r#"{"lockfileVersion": "13.0", "dependencies": {"is-odd": "3.0.1"}}"#)
            .expect_err("root dependencies");
    assert!(
        matches!(&err, crate::Error::PnpmLockfileLegacyLayout { marker, .. }
            if marker == "root-level `dependencies:` block"),
        "{err:?}"
    );
}

#[test]
fn detects_the_version_of_each_form() {
    assert!(declares_next_format("lockfileVersion: '13.0'\n"));
    assert!(declares_next_format("lockfileVersion: \"13.0\" # next\n"));
    assert!(declares_next_format("\u{feff}lockfileVersion: 13.0\r\n"));
    assert!(declares_next_format(
        "{\n\t\"lockfileVersion\": \"13.0\",\n}"
    ));
    assert!(declares_next_format(
        "---\nlockfileVersion: '9.0'\n---\nlockfileVersion: '13.0'\n"
    ));
    assert!(!declares_next_format("lockfileVersion: '9.0'\n"));
    assert!(!declares_next_format("lockfileVersion: '13'\n"));
    assert!(!declares_next_format("lockfileVersion: '13.x'\n"));
    assert!(declares_next_format("---\nlockfileVersion: '13.0'\n"));
    assert!(declares_next_format(
        "# env\n\n---\nlockfileVersion: '9.0'\n---\nlockfileVersion: '13.0'\n"
    ));
    assert!(!declares_next_format(
        "---\nlockfileVersion: '9.0'\n---\nlockfileVersion: '9.0'\n"
    ));
    // Only the top-level key counts, not one nested or inside a string.
    assert!(!declares_next_format(
        "{\"settings\": {\"lockfileVersion\": \"13.0\"}, \"lockfileVersion\": \"9.0\"}"
    ));
    assert!(!declares_next_format(
        "{\"time\": {\"a\": \"\\\"lockfileVersion\\\": \\\"13.0\\\"\"}}"
    ));
}

#[test]
fn blanks_trailing_commas_outside_strings() {
    let mut bytes = br#"{"a": [1, 2,], "b": ",}", "c": {"d": "\",]",	},}"#.to_vec();
    blank_trailing_commas(&mut bytes);
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        r#"{"a": [1, 2 ], "b": ",}", "c": {"d": "\",]" 	} }"#
    );
}

/// A v13 file that leaves out a version nothing implies is refused with
/// a message that names the dependency.
#[test]
fn missing_implied_version_names_the_dependency() {
    let content = r#"{
        "lockfileVersion": "13.0",
        "projects": {".": {"dependencies": {"a": "workspace:*"}}},
    }"#;
    let err = graph_of_content(content).expect_err("no version follows from workspace:*");
    assert_eq!(
        miette::Diagnostic::code(&err)
            .map(|c| c.to_string())
            .as_deref(),
        Some(aube_codes::errors::ERR_AUBE_LOCKFILE_PARSE)
    );
    assert!(
        err.to_string().contains("which version a resolves to in .")
            || format!("{err:?}").contains("which version a resolves to in ."),
        "{err:?}"
    );
}

/// A lockfile version after 9 that is not 13 has a shape aube does not
/// know, so it is refused instead of read as v9.
#[test]
fn unknown_newer_version_is_refused() {
    let err = graph_of_content("lockfileVersion: '10.0'\nimporters: {}\n").expect_err("10.0");
    assert!(
        matches!(&err, crate::Error::UnsupportedPnpmLockfileVersion { version, .. } if version == "10.0"),
        "{err:?}"
    );
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        "[ -~\\t\\n,\\]\\}\"\\\\é]{0,12}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map("[a-z,\\]\\}\"]{0,6}", inner, 0..6)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

/// `value` as JSON with a comma after every element, the way pnpm writes
/// the format.
fn with_trailing_commas(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for item in items {
                with_trailing_commas(item, out);
                out.push_str(",\n");
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (key, item) in map {
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                with_trailing_commas(item, out);
                out.push_str(",\t");
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

proptest! {
    /// Valid JSON has no trailing comma, so blanking leaves it as it is.
    #[test]
    fn blanking_leaves_valid_json_unchanged(value in json_value(), pretty in any::<bool>()) {
        let text = if pretty {
            serde_json::to_string_pretty(&value).unwrap()
        } else {
            serde_json::to_string(&value).unwrap()
        };
        let mut bytes = text.clone().into_bytes();
        blank_trailing_commas(&mut bytes);
        prop_assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    /// JSON with trailing commas reads back to the value it was written
    /// from.
    #[test]
    fn trailing_commas_read_back(value in json_value()) {
        let mut text = String::new();
        with_trailing_commas(&value, &mut text);
        let mut bytes = text.into_bytes();
        blank_trailing_commas(&mut bytes);
        let read: Value = serde_json::from_slice(&bytes).unwrap();
        prop_assert_eq!(read, value);
    }
}

/// `graph` written over `existing` in a fresh `pnpm-lock.yaml`.
fn written_over(existing: Option<&str>, graph: &crate::LockfileGraph) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    if let Some(existing) = existing {
        std::fs::write(&path, existing).unwrap();
    }
    crate::pnpm::write(&path, graph, &aube_manifest::PackageJson::default())
        .unwrap_or_else(|err| panic!("{err:?}"));
    std::fs::read_to_string(&path).unwrap()
}

/// A line-by-line diff of the first lines that differ, for a readable
/// failure on a byte comparison.
fn first_differences(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let start = expected
        .iter()
        .zip(&actual)
        .position(|(left, right)| left != right)
        .unwrap_or(expected.len().min(actual.len()));
    let window = |lines: &[&str]| {
        lines
            .iter()
            .skip(start)
            .take(8)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
    };
    format!(
        "first difference at line {}\n  pnpm: {:#?}\n  aube: {:#?}",
        start + 1,
        window(&expected),
        window(&actual)
    )
}

/// Rewriting a v13 lockfile pnpm 12.21 wrote gives back the same bytes.
fn assert_v13_round_trip(project: &str) {
    let v13 = std::fs::read_to_string(fixture(&format!("{project}.v13.yaml"))).unwrap();
    let graph = graph_from(&v13);
    let written = written_over(Some(&v13), &graph);
    assert!(
        written == v13,
        "{project}: {}",
        first_differences(&v13, &written)
    );
}

fn graph_from(content: &str) -> crate::LockfileGraph {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, content).unwrap();
    parse(&path).unwrap_or_else(|err| panic!("{err:?}"))
}

/// pnpm's v9 document of each project converts into the v13 lockfile
/// pnpm writes for it.
#[test]
fn v9_documents_convert_as_pnpm_converts_them() {
    for project in ["peers", "nested", "dedupe"] {
        let v9 = std::fs::read_to_string(fixture(&format!("{project}.v9.yaml"))).unwrap();
        let v13 = std::fs::read_to_string(fixture(&format!("{project}.v13.yaml"))).unwrap();
        let document: Value = yaml_serde::from_str(&v9).unwrap();
        let written = super::to_next(&document, &super::overrides_order(&v9))
            .unwrap_or_else(|err| panic!("{project}: {err}"));
        assert!(
            written == v13,
            "{project}: {}",
            first_differences(&v13, &written)
        );
    }
}

// `nested` is left out of the graph round trips below: the graph does
// not keep its `tpos` alias of a remote tarball, or the version and
// engines of its `is-negative` tarball, so aube rewrites those at v9 too.

#[test]
fn v13_with_peers_rewrites_byte_for_byte() {
    assert_v13_round_trip("peers");
}

#[test]
fn v13_with_dedupe_peers_rewrites_byte_for_byte() {
    assert_v13_round_trip("dedupe");
}

/// With `lockfile.format: v13`, rewriting the v9 lockfile of a project
/// gives the v13 lockfile pnpm writes for it.
#[test]
fn v9_lockfile_with_v13_requested_is_written_as_pnpm_writes_v13() {
    for project in ["peers", "dedupe"] {
        let v9 = std::fs::read_to_string(fixture(&format!("{project}.v9.yaml"))).unwrap();
        let v13 = std::fs::read_to_string(fixture(&format!("{project}.v13.yaml"))).unwrap();
        let mut graph = graph_from(&v9);
        graph.settings.pnpm_lockfile_format = Some(crate::PnpmLockfileFormat::V13);
        let written = written_over(Some(&v9), &graph);
        assert!(
            written == v13,
            "{project}: {}",
            first_differences(&v13, &written)
        );
    }
}

/// A new lockfile is written at v13 when the graph asks for it, and at v9
/// otherwise.
#[test]
fn new_lockfile_takes_the_requested_format() {
    let v9 = std::fs::read_to_string(fixture("peers.v9.yaml")).unwrap();
    let mut graph = graph_from(&v9);
    assert!(!declares_next_format(&written_over(None, &graph)));
    graph.settings.pnpm_lockfile_format = Some(crate::PnpmLockfileFormat::V13);
    let written = written_over(None, &graph);
    assert!(declares_next_format(&written), "{written}");
    assert_eq!(
        graph_of_content(&written).unwrap(),
        graph_of_content(&v9).unwrap()
    );
}

/// `lockfile.format: v9` switches a v13 lockfile back to v9.
#[test]
fn v13_lockfile_with_v9_requested_is_written_as_v9() {
    let v13 = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    let mut graph = graph_from(&v13);
    graph.settings.pnpm_lockfile_format = Some(crate::PnpmLockfileFormat::V9);
    let written = written_over(Some(&v13), &graph);
    assert!(written.starts_with("lockfileVersion: '9.0'"), "{written}");
    assert_eq!(
        graph_of_content(&written).unwrap(),
        graph_of(&fixture("peers.v9.yaml"))
    );
}

/// The v13 lockfile reads back as the graph that was written.
#[test]
fn v13_is_kept_in_a_lockfile_without_a_requested_format() {
    let v13 = std::fs::read_to_string(fixture("dedupe.v13.yaml")).unwrap();
    let mut graph = graph_from(&v13);
    graph.settings.pnpm_lockfile_format = None;
    assert_eq!(written_over(Some(&v13), &graph), v13);
}

/// Per-project and deploy lockfiles are new files written from a fresh
/// resolve, so the format of the lockfile it replaces has to come along.
#[test]
fn re_resolved_graph_keeps_the_prior_format() {
    let prior = parse(&fixture("dedupe.v13.yaml")).unwrap();
    let mut resolved = parse(&fixture("dedupe.v9.yaml")).unwrap();
    resolved.overlay_metadata_from(&prior);
    assert_eq!(
        resolved.settings.pnpm_lockfile_format,
        Some(crate::PnpmLockfileFormat::V13)
    );
    resolved.settings.pnpm_lockfile_format = Some(crate::PnpmLockfileFormat::V9);
    resolved.overlay_metadata_from(&prior);
    assert_eq!(
        resolved.settings.pnpm_lockfile_format,
        Some(crate::PnpmLockfileFormat::V9)
    );
}

/// A lockfile that exists but cannot be read is not replaced, whatever
/// format the graph asks for: its format and environment are unknown.
#[test]
fn writing_over_unreadable_lockfile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::create_dir(&path).unwrap();
    let mut graph = parse(&fixture("peers.v9.yaml")).unwrap();
    for format in [
        None,
        Some(crate::PnpmLockfileFormat::V9),
        Some(crate::PnpmLockfileFormat::V13),
    ] {
        graph.settings.pnpm_lockfile_format = format;
        let err = crate::pnpm::write(&path, &graph, &aube_manifest::PackageJson::default())
            .expect_err("an unreadable lockfile is not overwritten");
        assert!(matches!(&err, crate::Error::Io(..)), "{format:?}: {err:?}");
        assert!(path.is_dir());
    }
}

const INTEGRITY: &str = "sha512-ImoN9vdC+9CSDxbHJIcYwImhox3/lvLNqKpX5dNXT9O5Vawb+c4wddQ6KXq8FsFMm4WuSc3nxaWG2Q3iBRcFdA==";

/// A generated package `p<index>`: at `1.0.0`, and also at `2.0.0` when
/// `two_versions`. With `peer`, it has a peer dependency on `p0`. `deps`
/// pick packages before it, each at its first or second version.
#[derive(Debug, Clone)]
struct PackageChoice {
    two_versions: bool,
    peer: bool,
    deps: Vec<(usize, bool)>,
}

/// A generated project: the version of `p0` its peers get, and its
/// dependencies, each a package, its first or second version, and
/// whether it is a dev dependency.
#[derive(Debug, Clone)]
struct ImporterChoice {
    second_peer_version: bool,
    deps: Vec<(usize, bool, bool)>,
}

fn package_choice() -> impl Strategy<Value = PackageChoice> {
    (
        any::<bool>(),
        any::<bool>(),
        prop::collection::vec((any::<usize>(), any::<bool>()), 0..4),
    )
        .prop_map(|(two_versions, peer, deps)| PackageChoice {
            two_versions,
            peer,
            deps,
        })
}

fn importer_choice() -> impl Strategy<Value = ImporterChoice> {
    (
        any::<bool>(),
        prop::collection::vec((any::<usize>(), any::<bool>(), any::<bool>()), 0..5),
    )
        .prop_map(|(second_peer_version, deps)| ImporterChoice {
            second_peer_version,
            deps,
        })
}

/// The v9 document of a generated workspace. Every peer of a snapshot is
/// a dependency of its parent, the way pnpm resolves it, so no snapshot
/// has transitive peers.
fn generated_document(packages: &[PackageChoice], importers: &[ImporterChoice]) -> Value {
    let version = |index: usize, second: bool| {
        if second && packages[index].two_versions {
            "2.0.0"
        } else {
            "1.0.0"
        }
    };
    let has_peer = |index: usize| index > 0 && packages[index].peer;
    let mut package_entries = serde_json::Map::new();
    let mut snapshots = serde_json::Map::new();
    for (index, package) in packages.iter().enumerate() {
        let versions: &[bool] = if package.two_versions {
            &[false, true]
        } else {
            &[false]
        };
        let variants: Vec<Option<&str>> = if has_peer(index) {
            [false, true]
                .iter()
                .map(|second| Some(version(0, *second)))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            vec![None]
        };
        for second in versions {
            let own = version(index, *second);
            let mut entry = serde_json::json!({ "resolution": { "integrity": INTEGRITY } });
            if has_peer(index) {
                entry["peerDependencies"] = serde_json::json!({ "p0": "*" });
            }
            package_entries.insert(format!("p{index}@{own}"), entry);
            for peer in &variants {
                let mut deps = BTreeMap::new();
                if let Some(peer) = peer {
                    deps.insert("p0".to_string(), peer.to_string());
                }
                for (pick, second) in &package.deps {
                    if index == 0 {
                        break;
                    }
                    let child = pick % index;
                    let child_version = version(child, *second);
                    if child == 0 {
                        deps.entry("p0".to_string())
                            .or_insert_with(|| child_version.to_string());
                    } else if has_peer(child) {
                        let provided = deps
                            .entry("p0".to_string())
                            .or_insert_with(|| version(0, *second).to_string())
                            .clone();
                        deps.insert(
                            format!("p{child}"),
                            format!("{child_version}(p0@{provided})"),
                        );
                    } else {
                        deps.insert(format!("p{child}"), child_version.to_string());
                    }
                }
                let key = match peer {
                    Some(peer) => format!("p{index}@{own}(p0@{peer})"),
                    None => format!("p{index}@{own}"),
                };
                let snapshot = if deps.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::json!({ "dependencies": deps })
                };
                snapshots.insert(key, snapshot);
            }
        }
    }
    let mut importer_entries = serde_json::Map::new();
    for (importer, id) in importers.iter().zip([".", "packages/a", "packages/b"]) {
        let peer = version(0, importer.second_peer_version);
        let mut groups: BTreeMap<&str, BTreeMap<String, Value>> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut add = |groups: &mut BTreeMap<&str, BTreeMap<String, Value>>,
                       group: &'static str,
                       alias: String,
                       specifier: &str,
                       reference: String| {
            if seen.insert(alias.clone()) {
                groups.entry(group).or_default().insert(
                    alias,
                    serde_json::json!({ "specifier": specifier, "version": reference }),
                );
            }
        };
        for (pick, second, dev) in &importer.deps {
            let index = pick % packages.len();
            let group = if *dev {
                "devDependencies"
            } else {
                "dependencies"
            };
            if index == 0 {
                add(&mut groups, group, "p0".into(), peer, peer.into());
                continue;
            }
            let own = version(index, *second);
            if has_peer(index) {
                add(&mut groups, "dependencies", "p0".into(), peer, peer.into());
                add(
                    &mut groups,
                    group,
                    format!("p{index}"),
                    own,
                    format!("{own}(p0@{peer})"),
                );
            } else {
                add(&mut groups, group, format!("p{index}"), own, own.into());
            }
        }
        importer_entries.insert(id.to_string(), serde_json::json!(groups));
    }
    serde_json::json!({
        "lockfileVersion": "9.0",
        "settings": { "autoInstallPeers": true, "excludeLinksFromLockfile": false },
        "importers": importer_entries,
        "packages": package_entries,
        "snapshots": snapshots,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A generated graph written at v13 reads back as the same graph.
    #[test]
    fn written_v13_reads_back_as_the_graph(
        packages in prop::collection::vec(package_choice(), 1..6),
        importers in prop::collection::vec(importer_choice(), 1..4),
    ) {
        let document = generated_document(&packages, &importers);
        let v9 = yaml_serde::to_string(&document).unwrap();
        let expected = graph_of_content(&v9).unwrap();
        let v13 = super::to_next(&document, &[])
            .map_err(|err| TestCaseError::fail(format!("{err}\n{v9}")))?;
        prop_assert!(declares_next_format(&v13), "{}", v13);
        let read = graph_of_content(&v13).map_err(|err| TestCaseError::fail(format!("{err:?}\n{v13}")))?;
        prop_assert!(read == expected, "{}\n{}", v9, v13);
    }
}
