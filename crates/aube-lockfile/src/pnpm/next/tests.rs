use super::{blank_trailing_commas, declares_next_format, parse_document};
use crate::pnpm::parse;
use proptest::prelude::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pnpm-v13")
        .join(name)
}

/// The graph a lockfile parses into, compared through its `Debug`
/// rendering since `LockfileGraph` has no `PartialEq`.
fn graph_of(path: &Path) -> String {
    let graph = parse(path).unwrap_or_else(|err| panic!("{}: {err:?}", path.display()));
    format!("{graph:#?}")
}

fn graph_of_content(content: &str) -> Result<String, crate::Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, content).unwrap();
    parse(&path).map(|graph| format!("{graph:#?}"))
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

/// aube writes only v9, so it refuses to write over a v13 lockfile and
/// leaves the file as pnpm wrote it.
#[test]
fn writing_over_v13_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    let v13 = std::fs::read_to_string(fixture("peers.v13.yaml")).unwrap();
    std::fs::write(&path, &v13).unwrap();
    let graph = parse(&path).unwrap();
    let err = crate::pnpm::write(&path, &graph, &aube_manifest::PackageJson::default())
        .expect_err("v13 must not be rewritten as v9");
    assert_eq!(
        miette::Diagnostic::code(&err)
            .map(|c| c.to_string())
            .as_deref(),
        Some(aube_codes::errors::ERR_AUBE_PNPM_LOCKFILE_V13_READ_ONLY)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), v13);
}

/// A lockfile that exists but cannot be read might be v13, so the write
/// fails instead of replacing it.
#[test]
fn writing_over_unreadable_lockfile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::create_dir(&path).unwrap();
    let graph = parse(&fixture("peers.v9.yaml")).unwrap();
    let err = crate::pnpm::write(&path, &graph, &aube_manifest::PackageJson::default())
        .expect_err("an unreadable lockfile is not overwritten");
    assert!(matches!(&err, crate::Error::Io(..)), "{err:?}");
    assert!(path.is_dir());
}
