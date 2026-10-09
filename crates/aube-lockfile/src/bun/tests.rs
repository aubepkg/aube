use super::{jsonc::strip_jsonc, parse, raw::is_integrity_hash, source::split_ident, write};
use crate::{DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[test]
fn test_split_ident() {
    assert_eq!(
        split_ident("foo@1.2.3"),
        Some(("foo".to_string(), "1.2.3".to_string()))
    );
    assert_eq!(
        split_ident("@scope/pkg@1.0.0"),
        Some(("@scope/pkg".to_string(), "1.0.0".to_string()))
    );
}

#[test]
fn test_is_integrity_hash() {
    // Real SRI hashes at their exact base64 lengths.
    assert!(is_integrity_hash(&format!("sha512-{}", "A".repeat(88))));
    assert!(is_integrity_hash(&format!("sha256-{}", "A".repeat(44))));
    assert!(is_integrity_hash(&format!("sha1-{}", "A".repeat(28))));
    // base64 body with +, /, and = padding is still valid.
    let mixed = format!("{}+/==", "A".repeat(84));
    assert_eq!(mixed.len(), 88);
    assert!(is_integrity_hash(&format!("sha512-{mixed}")));

    // Github dir-id whose owner is literally a hash algo name —
    // the extra `-` and the wrong length must disqualify it.
    assert!(!is_integrity_hash("sha1-myrepo-abc123"));
    assert!(!is_integrity_hash("sha256-owner-repo-deadbee"));
    // Unknown algo prefix.
    assert!(!is_integrity_hash("foo-bar"));
    // Correct algo prefix but the wrong body length.
    assert!(!is_integrity_hash("sha512-tooshort"));
    // Right length but contains a forbidden `-` (base64 has no `-`).
    let with_dash = format!("sha512-{}-{}", "A".repeat(43), "A".repeat(44));
    assert_eq!(with_dash.len(), "sha512-".len() + 88);
    assert!(!is_integrity_hash(&with_dash));
    // No dash at all.
    assert!(!is_integrity_hash("opaquestring"));
}

#[test]
fn test_strip_jsonc_trailing_comma() {
    let input = r#"{ "a": 1, "b": 2, }"#;
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["a"], 1);
    assert_eq!(v["b"], 2);
}

#[test]
fn test_strip_jsonc_line_comment() {
    let input = "{ // comment\n  \"a\": 1 }";
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["a"], 1);
}

#[test]
fn test_strip_jsonc_respects_strings() {
    // Make sure we don't strip things that look like comments inside strings
    let input = r#"{ "url": "http://example.com/path" }"#;
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["url"], "http://example.com/path");
}

#[test]
fn strip_jsonc_preserves_utf8_string_value() {
    let input = "{ \"name\": \"café\" }";
    let out = strip_jsonc(input);
    assert_eq!(out.len(), input.len());
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["name"], "café");
}

#[test]
fn strip_jsonc_preserves_offsets_for_nonascii_in_comments() {
    let input = "{ // café\n  \"a\": 1 }";
    let out = strip_jsonc(input);
    assert_eq!(out.len(), input.len());
}

/// `strip_jsonc` must preserve byte offsets so a `serde_json` error
/// on the stripped buffer maps 1:1 onto the original file — that's
/// the only reason `parse()` can hand `raw_content` to miette's
/// `NamedSource` and trust the span.
#[test]
fn test_strip_jsonc_preserves_byte_offsets() {
    let cases = [
        "{ \"a\": 1 }",                    // no-op
        "{ // line\n  \"a\": 1 }",         // line comment
        "{ /* block */ \"a\": 1 }",        // block comment
        "{ /* multi\nline */ \"a\": 1 }",  // block spans newline
        "{ \"a\": 1, \"b\": 2, }",         // trailing comma
        "{ \"a\": \"// not a comment\" }", // comment inside string
        "{ \"a\": 1 /* trailing",          // unterminated block
    ];
    for input in cases {
        let out = strip_jsonc(input);
        assert_eq!(
            out.len(),
            input.len(),
            "length mismatch stripping {input:?} -> {out:?}"
        );
        // Every `\n` must land at the same byte offset so line
        // numbers stay stable between the raw and cleaned buffers.
        let raw_nls: Vec<usize> = input.match_indices('\n').map(|(i, _)| i).collect();
        let out_nls: Vec<usize> = out.match_indices('\n').map(|(i, _)| i).collect();
        assert_eq!(raw_nls, out_nls, "newline drift stripping {input:?}");
    }
}

/// Build a placeholder SRI hash of the right shape (88-char base64
/// body for sha512). Tests need real SRI lengths now that
/// `is_integrity_hash` validates them — bogus stand-ins like
/// `sha512-aaa` would be rejected and integrity dropped.
fn fake_sri(tag: char) -> String {
    format!("sha512-{}", tag.to_string().repeat(88))
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_foo = fake_sri('a');
    let sri_nested = fake_sri('b');
    let sri_bar = fake_sri('c');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "test",
      "dependencies": {
        "foo": "^1.0.0",
      },
      "devDependencies": {
        "bar": "^2.0.0",
      },
    },
  },
  "packages": {
    "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "SRI_FOO"],
    "nested": ["nested@3.1.0", "", {}, "SRI_NESTED"],
    "bar": ["bar@2.5.0", "", {}, "SRI_BAR"],
  }
}"#
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED", &sri_nested)
    .replace("SRI_BAR", &sri_bar);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert_eq!(graph.packages.len(), 3);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("nested@3.1.0"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some(sri_foo.as_str()));
    assert_eq!(
        foo.dependencies.get("nested").map(String::as_str),
        Some("3.1.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    assert!(
        root.iter()
            .any(|d| d.name == "foo" && d.dep_type == DepType::Production)
    );
    assert!(
        root.iter()
            .any(|d| d.name == "bar" && d.dep_type == DepType::Dev)
    );
}

#[test]
fn test_parse_bun_lifecycle_deps_as_dep_path_tails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_bufferutil = fake_sri('a');
    let sri_node_gyp_build = fake_sri('b');
    let sri_electron = fake_sri('c');
    let sri_electron_get = fake_sri('d');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": {
        "bufferutil": "4.0.9",
        "electron": "39.2.7"
      }
    }
  },
  "packages": {
    "bufferutil": ["bufferutil@4.0.9", "", { "dependencies": { "node-gyp-build": "^4.3.0" } }, "SRI_BUFFERUTIL"],
    "node-gyp-build": ["node-gyp-build@4.8.4", "", { "bin": { "node-gyp-build": "bin.js" } }, "SRI_NODE_GYP_BUILD"],
    "electron": ["electron@39.2.7", "", { "dependencies": { "@electron/get": "^2.0.0" } }, "SRI_ELECTRON"],
    "@electron/get": ["@electron/get@2.0.3", "", {}, "SRI_ELECTRON_GET"]
  }
}"#
        .replace("SRI_BUFFERUTIL", &sri_bufferutil)
        .replace("SRI_NODE_GYP_BUILD", &sri_node_gyp_build)
        .replace("SRI_ELECTRON", &sri_electron)
        .replace("SRI_ELECTRON_GET", &sri_electron_get);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let bufferutil = &graph.packages["bufferutil@4.0.9"];
    assert_eq!(
        bufferutil
            .dependencies
            .get("node-gyp-build")
            .map(String::as_str),
        Some("4.8.4")
    );

    let electron = &graph.packages["electron@39.2.7"];
    assert_eq!(
        electron
            .dependencies
            .get("@electron/get")
            .map(String::as_str),
        Some("2.0.3")
    );

    let root = graph.importers.get(".").unwrap();
    assert!(
        root.iter()
            .any(|d| d.name == "bufferutil" && d.dep_path == "bufferutil@4.0.9")
    );
    assert!(
        root.iter()
            .any(|d| d.name == "electron" && d.dep_path == "electron@39.2.7")
    );
}

#[test]
fn test_parse_multi_version_nested() {
    // bun keys nested packages using "parent/child" paths.
    // Here `bar` exists hoisted at 2.0.0 and nested under `foo` at 1.0.0.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_top_bar = fake_sri('a');
    let sri_foo = fake_sri('b');
    let sri_nested_bar = fake_sri('c');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "SRI_TOP_BAR"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "SRI_FOO"],
    "foo/bar": ["bar@1.0.0", "", {}, "SRI_NESTED_BAR"]
  }
}"#
    .replace("SRI_TOP_BAR", &sri_top_bar)
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED_BAR", &sri_nested_bar);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert!(graph.packages.contains_key("bar@2.0.0"));
    assert!(graph.packages.contains_key("bar@1.0.0"));
    assert!(graph.packages.contains_key("foo@1.0.0"));

    // foo's transitive must be the nested bar@1.0.0
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("1.0.0")
    );

    // Root direct bar is the hoisted 2.0.0
    let root = graph.importers.get(".").unwrap();
    let bar = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar.dep_path, "bar@2.0.0");
}

#[test]
fn test_parse_scoped() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('s');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "@scope/pkg": "^1.0.0" }
    }
  },
  "packages": {
    "@scope/pkg": ["@scope/pkg@1.0.0", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
}

/// bun.lock uses a 3-tuple `[ident, { meta }, "owner-repo-commit"]`
/// for GitHub / git deps (no `resolved` slot and no integrity). A
/// naive positional parse would mistake the trailing commit-id
/// string for the metadata object — make sure we recognize the
/// object by type rather than position.
#[test]
fn test_parse_github_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_dep = fake_sri('d');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "vfs": "github:collinstevens/vfs#0b6ea53" }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", { "dependencies": { "dep": "^1.0.0" } }, "collinstevens-vfs-0b6ea53"],
    "dep": ["dep@1.0.0", "", {}, "SRI_DEP"]
  }
}"#
        .replace("SRI_DEP", &sri_dep);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    // The vfs package parsed with its github: version and picked up
    // the transitive dep declared in the metadata slot.
    let vfs_key = "vfs@github:collinstevens/vfs#0b6ea53abcdef";
    assert!(graph.packages.contains_key(vfs_key));
    let vfs = &graph.packages[vfs_key];
    assert_eq!(
        vfs.dependencies.get("dep").map(String::as_str),
        Some("1.0.0")
    );
    // No SRI-shaped hash on the github entry → integrity stays None.
    assert!(vfs.integrity.is_none());

    // The adjacent registry dep's integrity must still round-trip —
    // proves the type-based introspection doesn't break the normal
    // 4-tuple path when mixed with a 3-tuple github entry.
    let dep = &graph.packages["dep@1.0.0"];
    assert_eq!(dep.integrity.as_deref(), Some(sri_dep.as_str()));

    let root = graph.importers.get(".").unwrap();
    assert!(root.iter().any(|d| d.name == "vfs"));
}

#[test]
fn test_parse_prefixless_local_tarball() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('t');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "local-helper": "file:tarballs/local-helper-1.0.0.tgz" }
    }
  },
  "packages": {
    "local-helper": ["local-helper@tarballs/local-helper-1.0.0.tgz", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["local-helper@tarballs/local-helper-1.0.0.tgz"];
    assert!(
        matches!(pkg.local_source, Some(LocalSource::Tarball(_))),
        "prefixless bun tarball ident must be LocalSource::Tarball, got {:?}",
        pkg.local_source
    );
}

/// Round-trip the same multi-version shape the npm writer test
/// uses: two versions of `bar`, one hoisted, one nested under
/// `foo`. The writer's bun-key form (`foo/bar` instead of
/// `node_modules/foo/node_modules/bar`) must round-trip through
/// the bun parser without losing the nested version.
#[test]
fn test_write_roundtrip_multi_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_top = fake_sri('t');
    let sri_foo = fake_sri('f');
    let sri_nested = fake_sri('n');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "SRI_TOP"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "SRI_FOO"],
    "foo/bar": ["bar@1.0.0", "", {}, "SRI_NESTED"]
  }
}"#
    .replace("SRI_TOP", &sri_top)
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED", &sri_nested);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("foo".to_string(), "^1.0.0".to_string()),
            ("bar".to_string(), "^2.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    assert!(reparsed.packages.contains_key("bar@2.0.0"));
    assert!(reparsed.packages.contains_key("bar@1.0.0"));
    assert!(reparsed.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        reparsed.packages["bar@2.0.0"].integrity.as_deref(),
        Some(sri_top.as_str())
    );
    assert_eq!(
        reparsed.packages["bar@1.0.0"].integrity.as_deref(),
        Some(sri_nested.as_str())
    );
    // foo's nested bar dep still resolves to 1.0.0 (nested)
    // rather than snapping to the hoisted 2.0.0.
    assert_eq!(
        reparsed.packages["foo@1.0.0"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("1.0.0")
    );
}

/// Byte-parity with a real `bun install`-generated lockfile — the
/// fixture at `tests/fixtures/bun-native.lock` was produced by
/// bun 1.3 against a `{ chalk, picocolors, semver }` manifest. A
/// parse → write round-trip must reproduce the exact bytes;
/// anything less means `aube install --no-frozen-lockfile` churns
/// someone's bun.lock in git when nothing in the graph moved.
/// Covers the format fixes (`configVersion`, no workspace
/// `version`, trailing commas, single-line package arrays) plus
/// the data-model fixes that ride with them (declared-range
/// preservation in `declared_dependencies`, `bin:` map
/// round-trip).
#[test]
fn test_write_byte_identical_to_native_bun() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bun-native.lock");
    // Normalize line endings — Windows' `core.autocrlf=true` can
    // rewrite the checked-out fixture to CRLF even with
    // `.gitattributes eol=lf`; compare against LF form explicitly.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");
    let graph = parse(&fixture).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("aube-lockfile-stability".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("chalk".to_string(), "^4.1.2".to_string()),
            ("picocolors".to_string(), "^1.1.1".to_string()),
            ("semver".to_string(), "^7.6.3".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();

    if written != original {
        panic!(
            "bun writer drifted from native bun output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
        );
    }
}

/// `configVersion` must echo back whatever was parsed, not a
/// hardcoded `1`. Regression guard for a future bun release that
/// bumps the field — without this, aube would silently downgrade
/// every re-emit and drift against bun's own output.
#[test]
fn test_write_roundtrips_config_version() {
    let project = tempfile::TempDir::new().unwrap();
    let pj = project.path().join("package.json");
    std::fs::write(&pj, r#"{"name":"root","dependencies":{}}"#).unwrap();
    let lock_path = project.path().join("bun.lock");
    std::fs::write(
        &lock_path,
        r#"{
  "lockfileVersion": 1,
  "configVersion": 42,
  "workspaces": {
    "": { "name": "root" }
  },
  "packages": {}
}"#,
    )
    .unwrap();

    let graph = parse(&lock_path).unwrap();
    assert_eq!(graph.bun_config_version, Some(42));

    let manifest = aube_manifest::PackageJson::from_path(&pj).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        written.contains("\"configVersion\": 42,"),
        "configVersion must round-trip verbatim, got:\n{written}"
    );
}

/// bun 1.4 stamps `lockfileVersion: 2` on content identical to v1. The
/// parser must read it, and re-saving must keep the 2 the way bun does
/// rather than churn the file back to 1.
#[test]
fn test_parse_and_write_roundtrips_lockfile_version_2() {
    let project = tempfile::TempDir::new().unwrap();
    let pj = project.path().join("package.json");
    std::fs::write(&pj, r#"{"name":"root","dependencies":{"foo":"^1.0.0"}}"#).unwrap();
    let lock_path = project.path().join("bun.lock");
    let integrity = fake_sri('a');
    std::fs::write(
        &lock_path,
        format!(
            r#"{{
  "lockfileVersion": 2,
  "configVersion": 1,
  "workspaces": {{
    "": {{ "name": "root", "dependencies": {{ "foo": "^1.0.0" }} }}
  }},
  "packages": {{
    "foo": ["foo@1.2.3", "", {{}}, "{integrity}"]
  }}
}}"#
        ),
    )
    .unwrap();

    let graph = parse(&lock_path).unwrap();
    assert!(graph.packages.contains_key("foo@1.2.3"));

    let manifest = aube_manifest::PackageJson::from_path(&pj).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        written.contains("\"lockfileVersion\": 2,"),
        "lockfileVersion 2 must round-trip, got:\n{written}"
    );
    assert_eq!(
        written.matches("lockfileVersion").count(),
        1,
        "lockfileVersion must be written once, got:\n{written}"
    );
}

/// Write `graph` back to `lock_path` and return the text.
fn rewrite(lock_path: &Path, pj: &Path, graph: &LockfileGraph) -> String {
    let manifest = aube_manifest::PackageJson::from_path(pj).unwrap();
    write(lock_path, graph, &manifest).unwrap();
    std::fs::read_to_string(lock_path).unwrap()
}

fn write_project(lockfile: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let project = tempfile::TempDir::new().unwrap();
    let pj = project.path().join("package.json");
    std::fs::write(&pj, r#"{"name":"root","dependencies":{}}"#).unwrap();
    let lock_path = project.path().join("bun.lock");
    std::fs::write(&lock_path, lockfile).unwrap();
    (project, pj, lock_path)
}

/// bun 1.4 writes v3 while scoped `overrides` exist: selector-keyed
/// groups whose `"."` child targets the parent itself. The parser must
/// read them as pnpm-style keys, and a re-save must write the same
/// groups, in bun's layout, still stamped v3.
#[test]
fn test_parse_and_write_roundtrips_lockfile_version_3_scoped_overrides() {
    let lockfile = r#"{
  "lockfileVersion": 3,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "root",
    },
  },
  "overrides": {
    "flat": "1.0.0",
    "foo": {
      ".": "2.0.0",
      "bar": "3.0.0",
    },
    "qux@^1": {
      ".": "4.0.0",
    },
  },
  "packages": {
  }
}
"#;
    let (_project, pj, lock_path) = write_project(lockfile);

    let graph = parse(&lock_path).unwrap();
    let expected: BTreeMap<String, String> = [
        ("flat", "1.0.0"),
        ("foo", "2.0.0"),
        ("foo>bar", "3.0.0"),
        ("qux@^1", "4.0.0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(graph.overrides, expected);

    let written = rewrite(&lock_path, &pj, &graph);
    assert!(
        written.contains("\"lockfileVersion\": 3,"),
        "scoped overrides must keep v3, got:\n{written}"
    );
    assert!(
        written.contains(
            "  \"overrides\": {\n    \"flat\": \"1.0.0\",\n    \"foo\": {\n      \".\": \"2.0.0\",\n      \"bar\": \"3.0.0\",\n    },\n    \"qux@^1\": {\n      \".\": \"4.0.0\",\n    },\n  },\n"
        ),
        "override groups must be written in bun's layout, got:\n{written}"
    );
    assert_eq!(parse(&lock_path).unwrap().overrides, expected);
}

/// bun stamps v3 whenever scoped rules exist, even on a lockfile it
/// loaded as v1, and walks a v3 lockfile whose scoped rules are gone
/// back down to v2.
#[test]
fn test_write_stamps_lockfile_version_from_scoped_overrides() {
    let (_project, pj, lock_path) = write_project(
        r#"{ "lockfileVersion": 1, "workspaces": { "": { "name": "root" } }, "packages": {} }"#,
    );
    let mut graph = parse(&lock_path).unwrap();
    graph
        .overrides
        .insert("parent>child".to_string(), "1.0.0".to_string());
    let written = rewrite(&lock_path, &pj, &graph);
    assert!(written.contains("\"lockfileVersion\": 3,"), "{written}");
    assert!(
        written.contains("\"parent\": {\n      \"child\": \"1.0.0\",\n    },"),
        "{written}"
    );

    let mut graph = parse(&lock_path).unwrap();
    graph.overrides.clear();
    graph
        .overrides
        .insert("plain".to_string(), "1.0.0".to_string());
    let written = rewrite(&lock_path, &pj, &graph);
    assert!(written.contains("\"lockfileVersion\": 2,"), "{written}");
    assert!(written.contains("\"plain\": \"1.0.0\","), "{written}");
}

#[test]
fn test_parse_rejects_malformed_scoped_overrides() {
    let (_project, _pj, lock_path) = write_project(
        r#"{
  "lockfileVersion": 3,
  "workspaces": { "": { "name": "root" } },
  "overrides": { "parent": { "child": { "deeper": "1.0.0" } } },
  "packages": {}
}"#,
    );
    let err = parse(&lock_path).unwrap_err().to_string();
    assert!(err.contains("must be a string"), "unexpected error: {err}");
}

#[test]
fn test_parse_rejects_unknown_lockfile_version() {
    let dir = tempfile::TempDir::new().unwrap();
    let lock_path = dir.path().join("bun.lock");
    std::fs::write(
        &lock_path,
        r#"{ "lockfileVersion": 9, "workspaces": {}, "packages": {} }"#,
    )
    .unwrap();

    let err = parse(&lock_path).unwrap_err().to_string();
    assert!(
        err.contains("lockfileVersion 9 is not supported (expected 1, 2, or 3)"),
        "unexpected error: {err}"
    );
}

/// Hand-authored bun.lock with two workspace entries (root and
/// `packages/app`) round-trips through the parser with both
/// importers populated, and the writer regenerates both
/// workspace entries from the on-disk manifests.
#[test]
fn test_parse_and_write_multi_workspace() {
    use tempfile::TempDir;
    let sri_foo = fake_sri('a');
    let sri_bar = fake_sri('b');

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0","dependencies":{"foo":"^1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"2.0.0","dependencies":{"bar":"^3.0.0"}}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "root",
      "version": "1.0.0",
      "dependencies": { "foo": "^1.0.0" }
    },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "dependencies": { "bar": "^3.0.0" }
    }
  },
  "packages": {
    "foo": ["foo@1.2.3", "", {}, "SRI_FOO"],
    "bar": ["bar@3.1.0", "", {}, "SRI_BAR"]
  }
}"#
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_BAR", &sri_bar);
    std::fs::write(&lock_path, content).unwrap();

    let graph = parse(&lock_path).unwrap();

    // Both importers are populated with their own direct deps.
    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");

    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].name, "bar");
    assert_eq!(app[0].dep_path, "bar@3.1.0");

    // Now write the graph back out and re-parse. The non-root
    // workspace entry must survive the round-trip. Write into the
    // same project dir so the writer can find
    // `packages/app/package.json` alongside the lockfile.
    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    std::fs::remove_file(&lock_path).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lock_path).unwrap();
    assert!(reparsed.importers.contains_key("."));
    assert!(reparsed.importers.contains_key("packages/app"));
    let app = &reparsed.importers["packages/app"];
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].name, "bar");
    assert_eq!(app[0].dep_path, "bar@3.1.0");
    // And the raw text keeps the workspace block by key.
    let raw = std::fs::read_to_string(&lock_path).unwrap();
    assert!(raw.contains("\"packages/app\""));
    assert!(raw.contains("\"name\": \"app\""));
}

/// Non-root workspace entries must carry `version`, `bin`, and
/// `optionalPeers` (bun's compact form of
/// `peerDependenciesMeta[name].optional`). Root stays minimal —
/// bun's own output omits those three on the root entry because
/// the adjacent project `package.json` is authoritative.
#[test]
fn test_write_workspace_entry_carries_version_bin_and_optional_peers() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/drifti")).unwrap();
    std::fs::write(
        project_dir.join("packages/drifti/package.json"),
        r#"{
  "name": "@redact/drifti",
  "version": "0.0.1",
  "bin": { "drifti": "./dist/cli/bin.mjs" },
  "peerDependencies": {
    "@electric-sql/pglite": "*",
    "kysely": "*"
  },
  "peerDependenciesMeta": {
    "kysely": { "optional": true },
    "@electric-sql/pglite": { "optional": true },
    "not-optional": { "optional": false }
  }
}"#,
    )
    .unwrap();

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/drifti".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let drifti = &v["workspaces"]["packages/drifti"];
    assert_eq!(drifti["name"], "@redact/drifti");
    assert_eq!(drifti["version"], "0.0.1");
    assert_eq!(drifti["bin"]["drifti"], "./dist/cli/bin.mjs");
    // Sorted alphabetically even though package.json lists keys
    // out of order, and the `optional: false` entry is excluded.
    let optional_peers: Vec<&str> = drifti["optionalPeers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(optional_peers, vec!["@electric-sql/pglite", "kysely"]);

    // `bin` must render inline — bun's own output puts it on one
    // line (`"bin": { "drifti": "./dist/cli/bin.mjs" }`). A
    // multi-line render here would produce the exact diff the
    // writer is trying to avoid.
    assert!(
        raw.contains(r#""bin": { "drifti": "./dist/cli/bin.mjs" },"#),
        "bin rendered multi-line or unexpected shape:\n{raw}"
    );

    // Root entry stays minimal: no version/bin/optionalPeers.
    let root = &v["workspaces"][""];
    assert!(
        root.get("version").is_none(),
        "root carried version: {root}"
    );
    assert!(root.get("bin").is_none(), "root carried bin: {root}");
    assert!(
        root.get("optionalPeers").is_none(),
        "root carried optionalPeers: {root}"
    );
}

/// Workspace-link packages must appear in `packages:` as
/// `[name@workspace:path]` so `bun install --frozen-lockfile`
/// can wire up the workspace dep without re-reading every
/// workspace package.json. Dropping them produces a lockfile
/// that errors with "Cannot find package" on the next install.
#[test]
fn test_write_emits_workspace_link_packages() {
    use crate::LocalSource;
    use std::path::PathBuf;

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tmp_dir.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"my-app","version":"0.1.0"}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "my-app@0.1.0".to_string(),
        LockedPackage {
            name: "my-app".to_string(),
            version: "0.1.0".to_string(),
            dep_path: "my-app@0.1.0".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/app"))),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/app".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let pkgs = v["packages"].as_object().unwrap();
    let entry = pkgs
        .get("my-app")
        .expect("workspace-link package missing from `packages`");
    let arr = entry.as_array().expect("entry must be a JSON array");
    assert_eq!(arr.len(), 1, "no-deps workspace entry must be `[ident]`");
    assert_eq!(arr[0].as_str(), Some("my-app@workspace:packages/app"));
    let ws = v["workspaces"].as_object().unwrap();
    assert!(ws.contains_key("packages/app"));
}

/// Workspace-to-workspace deps must survive emission. When `app`
/// depends on `lib` via `workspace:*`, `app`'s `packages:` entry
/// has to carry that dep edge in its meta or bun's frozen-install
/// pass can't wire it up. The dep target is another `LocalSource::Link`
/// package, not a registry one, so the membership check has to
/// accept workspace dep_paths in addition to canonical entries.
#[test]
fn test_write_preserves_workspace_to_workspace_dep_edge() {
    use crate::LocalSource;
    use std::path::PathBuf;
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::create_dir_all(project_dir.join("packages/lib")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"0.1.0","dependencies":{"lib":"workspace:*"}}"#,
    )
    .unwrap();
    std::fs::write(
        project_dir.join("packages/lib/package.json"),
        r#"{"name":"lib","version":"0.1.0"}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "app@workspace:packages/app".to_string(),
        LockedPackage {
            name: "app".to_string(),
            version: "workspace:packages/app".to_string(),
            dep_path: "app@workspace:packages/app".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/app"))),
            dependencies: [("lib".to_string(), "workspace:packages/lib".to_string())].into(),
            declared_dependencies: [("lib".to_string(), "workspace:*".to_string())].into(),
            ..Default::default()
        },
    );
    packages.insert(
        "lib@workspace:packages/lib".to_string(),
        LockedPackage {
            name: "lib".to_string(),
            version: "workspace:packages/lib".to_string(),
            dep_path: "lib@workspace:packages/lib".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/lib"))),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/app".to_string(), vec![]);
    importers.insert("packages/lib".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let app_entry = v["packages"]["app"].as_array().unwrap();
    assert_eq!(
        app_entry.len(),
        2,
        "workspace entry with deps must be `[ident, {{ meta }}]`"
    );
    assert_eq!(app_entry[0].as_str(), Some("app@workspace:packages/app"));
    assert_eq!(
        app_entry[1]["dependencies"]["lib"].as_str(),
        Some("workspace:*"),
        "workspace-to-workspace dep edge dropped"
    );
}

/// Parse → write → parse round-trip preserves a workspace entry
/// in `packages:`. Bun emits `[ident]` (and optionally `[ident,
/// { meta }]` when the workspace declares deps); both shapes must
/// survive without churning to the registry-package 4-tuple form.
#[test]
fn test_roundtrip_workspace_entry_in_packages_section() {
    use tempfile::TempDir;
    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"0.1.0"}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root", "version": "1.0.0" },
    "packages/app": { "name": "app", "version": "0.1.0" }
  },
  "packages": {
    "app": ["app@workspace:packages/app"]
  }
}"#;
    std::fs::write(&lock_path, content).unwrap();

    let graph = parse(&lock_path).unwrap();
    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    std::fs::remove_file(&lock_path).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let arr = v["packages"]["app"]
        .as_array()
        .expect("workspace entry survived as array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0].as_str(), Some("app@workspace:packages/app"));
}

/// When the root and a non-root workspace declare the same dep
/// name at *different* versions, the writer must emit a
/// consistent top-level `packages` entry and still walk the
/// chosen version's transitive deps. Regression test for a
/// corruption in `build_hoist_tree`'s root-seeding loop: without
/// name-dedupe, the second version would overwrite the first in
/// `placed` but never get queued, so neither version's
/// transitive deps were walked correctly and the top-level entry
/// pointed at a package whose deps were never expanded.
#[test]
fn test_write_dedupes_duplicate_direct_deps_across_workspaces() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","dependencies":{"foo":"^1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","dependencies":{"foo":"^2.0.0"}}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dependencies: [("bar".to_string(), "2.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    importers.insert(
        "packages/app".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lock_path).unwrap();
    // The root's version wins the hoisted `foo` slot (BTreeMap
    // iteration puts `.` before `packages/app`), and `bar` — only
    // reachable by walking root-foo's transitive deps — must be
    // present. Before the fix, `foo@2.0.0` would overwrite
    // `foo@1.0.0` in `placed` but never get queued, and neither
    // version's transitive deps (including `bar`) would make it
    // into the output.
    let foo = reparsed.packages.get("foo@1.0.0").expect("foo@1.0.0");
    assert_eq!(foo.version, "1.0.0");
    assert!(
        reparsed.packages.contains_key("bar@2.0.0"),
        "root foo's transitive `bar` was dropped: {:?}",
        reparsed.packages.keys().collect::<Vec<_>>()
    );
}

/// When a workspace directory path (e.g. `packages/app`) happens
/// to share its first segment with a literal npm package name,
/// the parser must not wrongly resolve a workspace dep to that
/// package's nested entry. Here there's an npm package literally
/// named `packages` with a nested `bar@9.9.9`, and the workspace
/// `packages/app` depends on `bar`. The workspace's `bar` must
/// resolve to the hoisted `bar@1.0.0`, not to `packages/bar`'s
/// `9.9.9`.
#[test]
fn test_parse_workspace_path_does_not_alias_npm_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "packages": "^1.0.0" } },
    "packages/app": {
      "name": "app",
      "dependencies": { "bar": "^1.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@1.0.0", "", {}, "SRI"],
    "packages": ["packages@1.0.0", "", { "dependencies": { "bar": "^9.0.0" } }, "SRI"],
    "packages/bar": ["bar@9.9.9", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    let bar = app.iter().find(|d| d.name == "bar").expect("bar dep");
    assert_eq!(
        bar.dep_path, "bar@1.0.0",
        "workspace `bar` must resolve to hoisted 1.0.0, not packages/bar@9.9.9"
    );
}

/// Bun scopes non-hoisted direct deps under the workspace package
/// name, not the workspace directory path. A workspace at
/// `packages/z-app` named `z-app` can therefore depend on
/// `z-app/tslib` while another workspace gets the hoisted `tslib`.
#[test]
fn test_parse_workspace_dep_prefers_workspace_name_scope() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/a-other": {
      "name": "a-other",
      "dependencies": { "tslib": "2.8.1" }
    },
    "packages/z-app": {
      "name": "z-app",
      "dependencies": { "tslib": "2.4.0" }
    }
  },
  "packages": {
    "a-other": ["a-other@workspace:packages/a-other"],
    "tslib": ["tslib@2.8.1", "", {}, "SRI"],
    "z-app": ["z-app@workspace:packages/z-app"],
    "z-app/tslib": ["tslib@2.4.0", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let other = graph
        .importers
        .get("packages/a-other")
        .expect("packages/a-other importer");
    let hoisted_tslib = other.iter().find(|d| d.name == "tslib").expect("tslib dep");
    assert_eq!(
        hoisted_tslib.dep_path, "tslib@2.8.1",
        "sibling workspace must still resolve to the hoisted tslib"
    );

    let app = graph
        .importers
        .get("packages/z-app")
        .expect("packages/z-app importer");
    let tslib = app.iter().find(|d| d.name == "tslib").expect("tslib dep");
    assert_eq!(
        tslib.dep_path, "tslib@2.4.0",
        "workspace dep must resolve to z-app/tslib, not hoisted tslib"
    );
}

#[test]
fn test_parse_rebases_workspace_scoped_local_tarball() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "dependencies": { "local-tar": "file:../../vendor/local-tar-1.0.0.tgz" }
    }
  },
  "packages": {
    "app": ["app@workspace:packages/app"],
    "app/local-tar": ["local-tar@../../vendor/local-tar-1.0.0.tgz", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let local_tar = graph
        .packages
        .values()
        .find(|p| p.name == "local-tar")
        .expect("local-tar package");
    assert_eq!(local_tar.version, "../../vendor/local-tar-1.0.0.tgz");
    assert_eq!(
        local_tar.local_source,
        Some(LocalSource::Tarball(PathBuf::from(
            "vendor/local-tar-1.0.0.tgz"
        )))
    );
}

/// Top-level `overrides` / `patchedDependencies` / `trustedDependencies`
/// and the unnamed `catalog` / named `catalogs` blocks must round-trip
/// verbatim — bun preserves all five on re-emit, so aube dropping any
/// of them is a real-repo churn source on every install. Keep this
/// test format-agnostic (no SRI hashes, no packages) so it only
/// exercises the metadata-preservation path.
#[test]
fn test_roundtrip_top_level_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" }
  },
  "overrides": {
    "lodash": "^4.17.21",
    "lodash>debug": "^4.0.0"
  },
  "patchedDependencies": {
    "lodash@4.17.21": "patches/lodash@4.17.21.patch"
  },
  "trustedDependencies": ["sharp", "esbuild"],
  "catalog": {
    "react": "^18.2.0"
  },
  "catalogs": {
    "evens": { "date-fns": "^2.30.0" }
  },
  "packages": {}
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert_eq!(
        graph.overrides.get("lodash").map(String::as_str),
        Some("^4.17.21")
    );
    assert_eq!(
        graph.overrides.get("lodash>debug").map(String::as_str),
        Some("^4.0.0")
    );
    assert_eq!(
        graph
            .patched_dependencies
            .get("lodash@4.17.21")
            .map(String::as_str),
        Some("patches/lodash@4.17.21.patch")
    );
    assert_eq!(
        graph.trusted_dependencies,
        vec!["sharp".to_string(), "esbuild".to_string()],
        "trustedDependencies must preserve bun's original order on parse"
    );
    assert_eq!(graph.catalogs["default"]["react"].specifier, "^18.2.0");
    assert_eq!(graph.catalogs["evens"]["date-fns"].specifier, "^2.30.0");

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // Every round-tripped block must appear in the re-emitted
    // lockfile — the exact rendering is implementation-defined
    // but a substring check is enough to catch regression.
    assert!(
        written.contains("\"overrides\""),
        "overrides dropped:\n{written}"
    );
    assert!(
        written.contains("\"patchedDependencies\""),
        "patchedDependencies dropped:\n{written}"
    );
    assert!(
        written.contains("\"trustedDependencies\""),
        "trustedDependencies dropped:\n{written}"
    );
    // trustedDependencies must round-trip in insertion order
    // (bun writes [sharp, esbuild] — alphabetized emit would
    // produce a gratuitous diff against bun's own output).
    let sharp_at = written
        .find("\"sharp\"")
        .expect("sharp in trustedDependencies");
    let esbuild_at = written
        .find("\"esbuild\"")
        .expect("esbuild in trustedDependencies");
    assert!(
        sharp_at < esbuild_at,
        "trustedDependencies reordered on write — expected sharp before esbuild:\n{written}"
    );
    assert!(
        written.contains("\"catalog\""),
        "catalog dropped:\n{written}"
    );
    assert!(
        written.contains("\"catalogs\""),
        "catalogs dropped:\n{written}"
    );

    let reparsed = parse(out.path()).unwrap();
    assert_eq!(reparsed.overrides, graph.overrides);
    assert_eq!(reparsed.patched_dependencies, graph.patched_dependencies);
    assert_eq!(reparsed.trusted_dependencies, graph.trusted_dependencies);
    assert_eq!(reparsed.catalogs["default"]["react"].specifier, "^18.2.0");
}

/// Non-registry specifier classes (github:, file:, link:, https:,
/// workspace:) must parse into `LocalSource` rather than fall
/// through as registry pins. The installer routes by
/// `LocalSource`, so mis-classification here sends the package
/// through the default registry and either 404s or downloads the
/// wrong tarball — bug class #1 in the parity report.
#[test]
fn test_parse_routes_non_registry_specs_to_localsource() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": {
        "vfs": "github:collinstevens/vfs#0b6ea53",
        "localdir": "file:./vendor/localdir",
        "localtgz": "file:./vendor/thing.tgz",
        "sibling": "link:../sibling",
        "remote": "https://example.com/thing.tgz"
      }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", {}, "collinstevens-vfs-0b6ea53abcdef"],
    "localdir": ["localdir@file:./vendor/localdir", {}],
    "localtgz": ["localtgz@file:./vendor/thing.tgz", {}],
    "sibling": ["sibling@link:../sibling", {}],
    "remote": ["remote@https://example.com/thing.tgz", {}]
  }
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let vfs = graph
        .packages
        .values()
        .find(|p| p.name == "vfs")
        .expect("vfs package");
    assert!(
        matches!(vfs.local_source, Some(LocalSource::Git(_))),
        "github dep must be LocalSource::Git, got {:?}",
        vfs.local_source
    );

    let localdir = graph
        .packages
        .values()
        .find(|p| p.name == "localdir")
        .expect("localdir package");
    assert!(
        matches!(localdir.local_source, Some(LocalSource::Directory(_))),
        "file:./dir must be LocalSource::Directory, got {:?}",
        localdir.local_source
    );

    let localtgz = graph
        .packages
        .values()
        .find(|p| p.name == "localtgz")
        .expect("localtgz package");
    assert!(
        matches!(localtgz.local_source, Some(LocalSource::Tarball(_))),
        "file:./*.tgz must be LocalSource::Tarball, got {:?}",
        localtgz.local_source
    );

    let sibling = graph
        .packages
        .values()
        .find(|p| p.name == "sibling")
        .expect("sibling package");
    assert!(
        matches!(sibling.local_source, Some(LocalSource::Link(_))),
        "link: must be LocalSource::Link, got {:?}",
        sibling.local_source
    );

    let remote = graph
        .packages
        .values()
        .find(|p| p.name == "remote")
        .expect("remote package");
    assert!(
        matches!(remote.local_source, Some(LocalSource::RemoteTarball(_))),
        "https://*.tgz must be LocalSource::RemoteTarball, got {:?}",
        remote.local_source
    );
}

#[test]
fn test_parse_bun_workspace_package_path_as_link_target() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "dependencies": { "lib": "workspace:*" }
    },
    "packages/lib": { "name": "lib" }
  },
  "packages": {
    "app": ["app@workspace:packages/app"],
    "lib": ["lib@workspace:packages/lib"]
  }
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let lib = graph.packages.get("lib@workspace:packages/lib").unwrap();
    assert_eq!(
        lib.local_source.as_ref().and_then(LocalSource::path),
        Some(Path::new("packages/lib"))
    );

    let app_deps = graph.importers.get("packages/app").unwrap();
    assert_eq!(app_deps[0].dep_path, "lib@workspace:packages/lib");
}

/// npm-alias ident: bun writes `<real>@<version>` as the ident
/// string while using the alias name as the `packages[]` hoist
/// key. Aube's earlier writer emitted `<alias>@<version>` and
/// produced a gratuitous diff against bun's own output. Cover
/// both parse (populates `alias_of`) and write (emits real name
/// in ident).
#[test]
fn test_parse_and_write_npm_alias() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "h3-v2": "npm:h3@2.0.1" } }
  },
  "packages": {
    "h3-v2": ["h3@2.0.1", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let h3 = graph
        .packages
        .values()
        .find(|p| p.name == "h3-v2")
        .expect("h3-v2 package");
    assert_eq!(h3.alias_of.as_deref(), Some("h3"));
    assert_eq!(h3.version, "2.0.1");

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("h3-v2".to_string(), "npm:h3@2.0.1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // Ident reads `h3@2.0.1` (registry identity), not `h3-v2@...`.
    assert!(
        written.contains("\"h3@2.0.1\""),
        "expected ident `h3@2.0.1`, got:\n{written}"
    );
    assert!(
        !written.contains("\"h3-v2@2.0.1\""),
        "alias-name ident leaked into packages entry:\n{written}"
    );
}

/// Per-entry meta blocks bun preserves that aube historically
/// dropped: `peerDependencies`, `optionalPeers`, `os`, `cpu`,
/// `libc`. Round-trip through a single package entry and confirm
/// every field survives re-parse.
#[test]
fn test_roundtrip_peer_and_platform_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "foo": "^1.0.0" } } },
  "packages": {
    "foo": ["foo@1.0.0", "", {
      "peerDependencies": { "react": "^18.0.0" },
      "optionalPeers": ["react"],
      "os": ["darwin", "linux"],
      "cpu": ["arm64", "x64"],
      "libc": ["glibc"]
    }, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.peer_dependencies.get("react").map(String::as_str),
        Some("^18.0.0")
    );
    assert!(
        foo.peer_dependencies_meta
            .get("react")
            .is_some_and(|m| m.optional)
    );
    assert_eq!(
        foo.os.as_slice(),
        &["darwin".to_string(), "linux".to_string()]
    );
    assert_eq!(
        foo.cpu.as_slice(),
        &["arm64".to_string(), "x64".to_string()]
    );
    assert_eq!(foo.libc.as_slice(), &["glibc".to_string()]);

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();
    let foo2 = &reparsed.packages["foo@1.0.0"];
    assert_eq!(foo2.peer_dependencies, foo.peer_dependencies);
    assert_eq!(foo2.os, foo.os);
    assert_eq!(foo2.cpu, foo.cpu);
    assert_eq!(foo2.libc, foo.libc);
}

#[test]
fn test_parse_scalar_platform_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "@esbuild/darwin-arm64": "0.27.2" } } },
  "packages": {
    "@esbuild/darwin-arm64": ["@esbuild/darwin-arm64@0.27.2", "", {
      "os": "darwin",
      "cpu": "arm64",
      "libc": "glibc"
    }, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["@esbuild/darwin-arm64@0.27.2"];
    assert_eq!(pkg.os.as_slice(), &["darwin".to_string()]);
    assert_eq!(pkg.cpu.as_slice(), &["arm64".to_string()]);
    assert_eq!(pkg.libc.as_slice(), &["glibc".to_string()]);
}

/// Workspace-level `peerDependencies` must survive round-trip
/// through the serde-flatten `extra` map even though aube's
/// typed workspace model doesn't claim the field directly. The
/// prior revision had a typed slot that silently drained bun's
/// peer block without plumbing it anywhere — regression guard.
#[test]
fn test_roundtrip_workspace_peer_dependencies() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    // Non-root workspace's package.json deliberately omits
    // peerDependencies; the lockfile is the only place they live.
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"2.0.0"}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    std::fs::write(
        &lock_path,
        r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "peerDependencies": { "react": "^18.0.0" }
    }
  },
  "packages": {}
}"#,
    )
    .unwrap();

    let graph = parse(&lock_path).unwrap();
    let app_extras = graph
        .workspace_extra_fields
        .get("packages/app")
        .expect("packages/app workspace_extra_fields entry");
    let peers = app_extras
        .get("peerDependencies")
        .and_then(serde_json::Value::as_object)
        .expect("peerDependencies captured in extras");
    assert_eq!(peers.get("react").and_then(|v| v.as_str()), Some("^18.0.0"));

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        written.contains("\"peerDependencies\""),
        "workspace peerDependencies dropped on re-emit:\n{written}"
    );
    assert!(
        written.contains("\"react\""),
        "workspace peerDependencies.react dropped on re-emit:\n{written}"
    );
}

/// A package declared in both `devDependencies` and
/// `optionalDependencies` must yield exactly one root `DirectDep`,
/// classified under the first declaring section. See discussion #1544.
#[test]
fn dev_and_optional_overlap_yields_one_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_foo = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "test",
      "devDependencies": {
        "foo": "^1.0.0",
      },
      "optionalDependencies": {
        "foo": "^1.0.0",
      },
    },
  },
  "packages": {
    "foo": ["foo@1.2.3", "", {}, "SRI_FOO"],
  }
}"#
    .replace("SRI_FOO", &sri_foo);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1, "expected one direct dep, got {root:?}");
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_type, DepType::Dev);
}

/// The `packages` section of the `bun.lock` at `path`.
fn written_packages(path: &Path) -> serde_json::Value {
    let written = std::fs::read_to_string(path).unwrap();
    let mut lockfile: serde_json::Value = serde_json::from_str(&strip_jsonc(&written)).unwrap();
    lockfile["packages"].take()
}

/// A fresh resolve keys `file:` packages by a path-hash dep_path and
/// keeps no declared specs for them. The writer must still emit them in
/// bun's shape, or `bun install --frozen-lockfile` sees them missing.
#[test]
fn test_write_file_directory_and_tarball_packages() {
    let dir = LocalSource::Directory(PathBuf::from("vendor/x"));
    let nested = LocalSource::Directory(PathBuf::from("vendor/y"));
    let tarball = LocalSource::Tarball(PathBuf::from("vendor/t.tgz"));
    let (x_path, y_path, t_path) = (
        dir.dep_path("x"),
        nested.dep_path("y"),
        tarball.dep_path("t"),
    );
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        x_path.clone(),
        LockedPackage {
            name: "x".to_string(),
            version: "1.2.3".to_string(),
            dep_path: x_path.clone(),
            local_source: Some(dir),
            dependencies: BTreeMap::from([(
                "y".to_string(),
                y_path.strip_prefix("y@").unwrap().to_string(),
            )]),
            ..Default::default()
        },
    );
    for (name, version, dep_path, local) in [
        ("y", "0.1.0", &y_path, nested),
        ("t", "3.0.0", &t_path, tarball),
    ] {
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: dep_path.clone(),
                local_source: Some(local),
                ..Default::default()
            },
        );
    }
    graph.importers.insert(
        ".".to_string(),
        [("x", &x_path), ("t", &t_path)]
            .into_iter()
            .map(|(name, dep_path)| DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            })
            .collect(),
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bun.lock");
    write(&path, &graph, &manifest).unwrap();

    assert_eq!(
        written_packages(&path),
        serde_json::json!({
            "t": ["t@./vendor/t.tgz", {}, ""],
            "x": ["x@file:vendor/x", { "dependencies": { "y": "file:../y" } }],
            "y": ["y@file:vendor/y", {}],
        })
    );

    let reparsed = parse(&path).unwrap();
    let local_sources: Vec<_> = reparsed
        .packages
        .values()
        .filter_map(|pkg| pkg.local_source.clone())
        .collect();
    assert!(local_sources.contains(&LocalSource::Directory(PathBuf::from("vendor/x"))));
    assert!(local_sources.contains(&LocalSource::Directory(PathBuf::from("vendor/y"))));
    assert!(local_sources.contains(&LocalSource::Tarball(PathBuf::from("./vendor/t.tgz"))));
}

/// bun keeps an absolute `file:` tarball absolute, and writes a local
/// child's spec relative to its parent even when the parent sits outside
/// the project.
#[test]
fn test_write_file_packages_with_absolute_and_outside_paths() {
    let project = tempfile::tempdir().unwrap();
    let archive = project.path().join("elsewhere").join("t.tgz");
    let tarball = LocalSource::Tarball(archive.clone());
    let parent = LocalSource::Directory(PathBuf::from("../x"));
    let child = LocalSource::Directory(PathBuf::from("vendor/y"));
    let (t_path, x_path, y_path) = (
        tarball.dep_path("t"),
        parent.dep_path("x"),
        child.dep_path("y"),
    );
    let mut graph = LockfileGraph::default();
    for (name, dep_path, local, deps) in [
        ("t", &t_path, tarball, BTreeMap::new()),
        (
            "x",
            &x_path,
            parent,
            BTreeMap::from([(
                "y".to_string(),
                y_path.strip_prefix("y@").unwrap().to_string(),
            )]),
        ),
        ("y", &y_path, child, BTreeMap::new()),
    ] {
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                dep_path: dep_path.clone(),
                local_source: Some(local),
                dependencies: deps,
                ..Default::default()
            },
        );
    }
    graph.importers.insert(
        ".".to_string(),
        [("t", &t_path), ("x", &x_path)]
            .into_iter()
            .map(|(name, dep_path)| DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            })
            .collect(),
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let path = project.path().join("app").join("bun.lock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write(&path, &graph, &manifest).unwrap();

    let absolute = archive.to_string_lossy().replace('\\', "/");
    assert_eq!(
        written_packages(&path),
        serde_json::json!({
            "t": [format!("t@{absolute}"), {}, ""],
            "x": ["x@file:../x", { "dependencies": { "y": "file:../app/vendor/y" } }],
            "y": ["y@file:vendor/y", {}],
        })
    );
}

/// With a relative lockfile path, a local child under the project still
/// gets a spec that leads to it from an absolute parent directory.
#[test]
fn test_local_child_spec_from_an_absolute_parent_with_a_relative_project_dir() {
    let parent_dir = std::env::temp_dir().join("elsewhere").join("x");
    let parent = LockedPackage {
        local_source: Some(LocalSource::Directory(parent_dir.clone())),
        ..Default::default()
    };
    let child = LockedPackage {
        local_source: Some(LocalSource::Directory(PathBuf::from("vendor/y"))),
        ..Default::default()
    };
    let spec = super::write::local_child_spec(Path::new("proj"), &parent, &child).unwrap();

    let target = std::path::absolute(Path::new("proj/vendor/y")).unwrap();
    let relative = spec.strip_prefix("file:").unwrap();
    assert_eq!(
        aube_util::path::normalize_lexical(&parent_dir.join(relative)),
        aube_util::path::normalize_lexical(&target),
        "{spec}"
    );
}

/// bun.lock lists no version for `file:` directories or workspace
/// members; reading it fills the real ones from their package.json, and
/// writing that graph back must still reproduce what bun wrote.
#[test]
fn test_parse_fills_local_versions_and_writes_bun_lock_back_unchanged() {
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    for (path, manifest) in [
        (
            "package.json",
            r#"{"name":"root","private":true,"workspaces":["packages/*","lib"],"dependencies":{"x":"file:./vendor/x"}}"#,
        ),
        (
            "packages/a/package.json",
            r#"{"name":"a","version":"1.0.0","dependencies":{"b":"workspace:*","lib":"workspace:*"}}"#,
        ),
        (
            "packages/b/package.json",
            r#"{"name":"b","version":"2.0.0"}"#,
        ),
        ("lib/package.json", r#"{"name":"lib","version":"3.1.4"}"#),
        (
            "vendor/x/package.json",
            r#"{"name":"x","version":"1.2.3","dependencies":{"y":"file:../y"}}"#,
        ),
        ("vendor/y/package.json", r#"{"name":"y","version":"0.1.0"}"#),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, manifest).unwrap();
    }
    // What bun 1.4 writes for this project.
    let original = r#"{
  "lockfileVersion": 2,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "root",
      "dependencies": {
        "x": "file:./vendor/x",
      },
    },
    "lib": {
      "name": "lib",
      "version": "3.1.4",
    },
    "packages/a": {
      "name": "a",
      "version": "1.0.0",
      "dependencies": {
        "b": "workspace:*",
        "lib": "workspace:*",
      },
    },
    "packages/b": {
      "name": "b",
      "version": "2.0.0",
    },
  },
  "packages": {
    "a": ["a@workspace:packages/a"],

    "b": ["b@workspace:packages/b"],

    "lib": ["lib@workspace:lib"],

    "x": ["x@file:vendor/x", { "dependencies": { "y": "file:../y" } }],

    "x/y": ["y@file:vendor/y", {}],
  }
}
"#;
    std::fs::write(dir.join("bun.lock"), original).unwrap();
    let manifest = aube_manifest::PackageJson::from_path(&dir.join("package.json")).unwrap();

    let graph = crate::parse_lockfile(dir, &manifest).unwrap();
    let version_of = |name: &str| {
        graph
            .packages
            .values()
            .find(|pkg| pkg.name == name)
            .map(|pkg| pkg.version.as_str())
    };
    // aube hoists `y` where bun nests it under `x`; bun accepts both.
    let out = dir.join("bun.lock.out");
    write(&out, &graph, &manifest).unwrap();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        original.replace(r#""x/y":"#, r#""y":"#)
    );
    assert_eq!(version_of("x"), Some("1.2.3"));
    assert_eq!(version_of("y"), Some("0.1.0"));
    assert_eq!(version_of("b"), Some("2.0.0"));
    assert_eq!(version_of("lib"), Some("3.1.4"));
}

/// A fresh resolve reaches a sibling workspace by version, so the graph
/// has no `link:` package for any member. The writer must still list
/// every member, or installs from the lockfile skip workspace links.
#[test]
fn test_write_lists_workspace_members_without_link_packages() {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, manifest) in [
        (
            "packages/a",
            r#"{"name":"a","version":"1.0.0","dependencies":{"b":"workspace:*"}}"#,
        ),
        ("packages/b", r#"{"name":"b","version":"1.0.0"}"#),
    ] {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        std::fs::write(tmp.path().join(dir).join("package.json"), manifest).unwrap();
    }
    let mut graph = LockfileGraph::default();
    graph.importers.insert(".".to_string(), Vec::new());
    graph.importers.insert(
        "packages/a".to_string(),
        vec![DirectDep {
            name: "b".to_string(),
            dep_path: "b@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("workspace:*".to_string()),
        }],
    );
    graph.importers.insert("packages/b".to_string(), Vec::new());
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let path = tmp.path().join("bun.lock");
    write(&path, &graph, &manifest).unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains(r#""a": ["a@workspace:packages/a"]"#),
        "{written}"
    );
    assert!(
        written.contains(r#""b": ["b@workspace:packages/b"]"#),
        "{written}"
    );

    let reparsed = parse(&path).unwrap();
    let a_deps = &reparsed.importers["packages/a"];
    assert!(a_deps.iter().any(|dep| dep.name == "b"), "{a_deps:?}");
}

/// bun keeps a resolved package under a name a workspace member also
/// has, and nests the member under each member that asks for it with
/// `workspace:`. A second top-level key would replace one on reparse.
#[test]
fn test_write_nests_workspace_members_named_like_a_package() {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, manifest) in [
        (
            "packages/is-number",
            r#"{"name":"is-number","version":"1.0.0"}"#,
        ),
        (
            "packages/a",
            r#"{"name":"a","version":"1.0.0","dependencies":{"is-number":"workspace:*"}}"#,
        ),
    ] {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        std::fs::write(tmp.path().join(dir).join("package.json"), manifest).unwrap();
    }
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            ..Default::default()
        },
    );
    let direct = |dep_path: &str, specifier: &str| DirectDep {
        name: "is-number".to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: Some(specifier.to_string()),
    };
    graph
        .importers
        .insert(".".to_string(), vec![direct("is-number@7.0.0", "^7.0.0")]);
    graph.importers.insert(
        "packages/a".to_string(),
        vec![direct("is-number@1.0.0", "workspace:*")],
    );
    graph
        .importers
        .insert("packages/is-number".to_string(), Vec::new());
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("is-number".to_string(), "^7.0.0".to_string())].into(),
        ..Default::default()
    };
    let path = tmp.path().join("bun.lock");
    write(&path, &graph, &manifest).unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written.matches(r#""is-number": ["#).count(), 1, "{written}");
    assert!(
        written.contains(r#""a/is-number": ["is-number@workspace:packages/is-number"]"#),
        "{written}"
    );
    let reparsed = parse(&path).unwrap();
    assert_eq!(reparsed.importers["."][0].dep_path, "is-number@7.0.0");
    let member_dep = &reparsed.importers["packages/a"][0];
    assert!(
        matches!(
            reparsed.packages[&member_dep.dep_path].local_source,
            Some(LocalSource::Link(_))
        ),
        "{member_dep:?}"
    );
}

/// Writes `graph` for a workspace whose member manifests are `members`
/// (path, package.json) and whose root declares `root_deps`.
fn write_workspace(
    graph: &LockfileGraph,
    members: &[(&str, &str)],
    root_deps: &[(&str, &str)],
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, manifest) in members {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        std::fs::write(tmp.path().join(dir).join("package.json"), manifest).unwrap();
    }
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: root_deps
            .iter()
            .map(|(name, spec)| (name.to_string(), spec.to_string()))
            .collect(),
        ..Default::default()
    };
    let path = tmp.path().join("bun.lock");
    write(&path, graph, &manifest).unwrap();
    (tmp, path)
}

fn is_number_dep(dep_path: &str, specifier: &str) -> DirectDep {
    DirectDep {
        name: "is-number".to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: Some(specifier.to_string()),
    }
}

/// A package keyed like the dependent member would read the nested
/// member as its own dependency, so the member is not nested there.
#[test]
fn test_write_does_not_nest_members_under_a_package_key() {
    let mut graph = LockfileGraph::default();
    for (name, version, deps) in [
        ("is-number", "6.0.0", vec![]),
        ("is-odd", "3.0.1", vec![("is-number", "6.0.0")]),
    ] {
        graph.packages.insert(
            format!("{name}@{version}"),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: format!("{name}@{version}"),
                dependencies: deps
                    .into_iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
                ..Default::default()
            },
        );
    }
    graph.importers.insert(
        ".".to_string(),
        vec![
            is_number_dep("is-number@6.0.0", "^6.0.0"),
            DirectDep {
                name: "is-odd".to_string(),
                dep_path: "is-odd@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^3.0.1".to_string()),
            },
        ],
    );
    graph.importers.insert(
        "packages/is-odd".to_string(),
        vec![is_number_dep("is-number@1.0.0", "workspace:*")],
    );
    graph
        .importers
        .insert("packages/is-number".to_string(), Vec::new());
    let (_tmp, path) = write_workspace(
        &graph,
        &[
            (
                "packages/is-number",
                r#"{"name":"is-number","version":"1.0.0"}"#,
            ),
            (
                "packages/is-odd",
                r#"{"name":"is-odd","version":"1.0.0","dependencies":{"is-number":"workspace:*"}}"#,
            ),
        ],
        &[("is-number", "^6.0.0"), ("is-odd", "^3.0.1")],
    );

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains(r#""is-odd/is-number""#), "{written}");
    let reparsed = parse(&path).unwrap();
    assert_eq!(
        reparsed.packages["is-odd@3.0.1"].dependencies["is-number"],
        "6.0.0"
    );
}

/// A member whose path is the dependent's name would read the nested
/// member through its path scope, so the member is not nested there.
#[test]
fn test_write_does_not_nest_members_under_another_members_path() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![is_number_dep("is-number@7.0.0", "^7.0.0")],
    );
    graph.importers.insert(
        "packages/a".to_string(),
        vec![is_number_dep("is-number@1.0.0", "workspace:*")],
    );
    graph.importers.insert(
        "a".to_string(),
        vec![is_number_dep("is-number@7.0.0", "^7.0.0")],
    );
    graph
        .importers
        .insert("packages/is-number".to_string(), Vec::new());
    let (_tmp, path) = write_workspace(
        &graph,
        &[
            (
                "packages/is-number",
                r#"{"name":"is-number","version":"1.0.0"}"#,
            ),
            (
                "packages/a",
                r#"{"name":"a","version":"1.0.0","dependencies":{"is-number":"workspace:*"}}"#,
            ),
            (
                "a",
                r#"{"name":"c","version":"1.0.0","dependencies":{"is-number":"^7.0.0"}}"#,
            ),
        ],
        &[("is-number", "^7.0.0")],
    );

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains(r#""a/is-number""#), "{written}");
    let reparsed = parse(&path).unwrap();
    assert_eq!(reparsed.importers["a"][0].dep_path, "is-number@7.0.0");
}

/// bun gives the top-level slot of a member the root asks for with
/// `workspace:` to that member, and nests a registry package of the same
/// name under the package that depends on it.
#[test]
fn test_write_keeps_the_root_workspace_dep_at_the_top_level() {
    let mut graph = LockfileGraph::default();
    for (name, version, deps) in [
        ("is-number", "6.0.0", vec![]),
        ("is-odd", "3.0.1", vec![("is-number", "6.0.0")]),
    ] {
        graph.packages.insert(
            format!("{name}@{version}"),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: format!("{name}@{version}"),
                dependencies: deps
                    .into_iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
                ..Default::default()
            },
        );
    }
    graph.importers.insert(
        ".".to_string(),
        vec![
            is_number_dep("is-number@1.0.0", "workspace:*"),
            DirectDep {
                name: "is-odd".to_string(),
                dep_path: "is-odd@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^3.0.1".to_string()),
            },
        ],
    );
    graph
        .importers
        .insert("packages/is-number".to_string(), Vec::new());
    let (_tmp, path) = write_workspace(
        &graph,
        &[(
            "packages/is-number",
            r#"{"name":"is-number","version":"1.0.0"}"#,
        )],
        &[("is-number", "workspace:*"), ("is-odd", "^3.0.1")],
    );

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains(r#""is-number": ["is-number@workspace:packages/is-number"]"#),
        "{written}"
    );
    assert!(
        written.contains(r#""is-odd/is-number": ["is-number@6.0.0""#),
        "{written}"
    );
    let reparsed = parse(&path).unwrap();
    let root_dep = reparsed.importers["."]
        .iter()
        .find(|dep| dep.name == "is-number")
        .unwrap();
    assert!(
        matches!(
            reparsed.packages[&root_dep.dep_path].local_source,
            Some(LocalSource::Link(_))
        ),
        "{root_dep:?}"
    );
    assert_eq!(
        reparsed.packages["is-odd@3.0.1"].dependencies["is-number"],
        "6.0.0"
    );
}

/// A member's own registry dep on a name the root reserves for a
/// workspace member goes under that member, as bun writes it.
#[test]
fn test_write_nests_a_members_registry_dep_on_a_reserved_name() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "is-number@6.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "6.0.0".to_string(),
            dep_path: "is-number@6.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![is_number_dep("is-number@1.0.0", "workspace:*")],
    );
    graph.importers.insert(
        "packages/app".to_string(),
        vec![is_number_dep("is-number@6.0.0", "^6.0.0")],
    );
    graph
        .importers
        .insert("packages/is-number".to_string(), Vec::new());
    let (_tmp, path) = write_workspace(
        &graph,
        &[
            (
                "packages/is-number",
                r#"{"name":"is-number","version":"1.0.0"}"#,
            ),
            (
                "packages/app",
                r#"{"name":"app","version":"1.0.0","dependencies":{"is-number":"^6.0.0"}}"#,
            ),
        ],
        &[("is-number", "workspace:*")],
    );

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains(r#""is-number": ["is-number@workspace:packages/is-number"]"#),
        "{written}"
    );
    assert!(
        written.contains(r#""app/is-number": ["is-number@6.0.0""#),
        "{written}"
    );
    let reparsed = parse(&path).unwrap();
    assert_eq!(
        reparsed.importers["packages/app"][0].dep_path,
        "is-number@6.0.0"
    );
    let root_dep = &reparsed.importers["."][0];
    assert!(
        matches!(
            reparsed.packages[&root_dep.dep_path].local_source,
            Some(LocalSource::Link(_))
        ),
        "{root_dep:?}"
    );
}

/// The deps of a member's nested registry package go below that package,
/// so they don't shadow the member's own direct deps.
#[test]
fn test_write_nests_the_deps_of_a_members_nested_package_below_it() {
    let mut graph = LockfileGraph::default();
    for (name, version, deps) in [
        ("is-number", "6.0.0", vec![]),
        ("is-number", "7.0.0", vec![]),
        ("is-odd", "3.0.1", vec![("is-number", "6.0.0")]),
    ] {
        graph.packages.insert(
            format!("{name}@{version}"),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: format!("{name}@{version}"),
                dependencies: deps
                    .into_iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
                ..Default::default()
            },
        );
    }
    let is_odd_dep = |dep_path: &str, specifier: &str| DirectDep {
        name: "is-odd".to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: Some(specifier.to_string()),
    };
    graph.importers.insert(
        ".".to_string(),
        vec![is_odd_dep("is-odd@1.0.0", "workspace:*")],
    );
    graph.importers.insert(
        "packages/app".to_string(),
        vec![
            is_odd_dep("is-odd@3.0.1", "^3.0.1"),
            is_number_dep("is-number@7.0.0", "^7.0.0"),
        ],
    );
    graph
        .importers
        .insert("packages/is-odd".to_string(), Vec::new());
    let (_tmp, path) = write_workspace(
        &graph,
        &[
            ("packages/is-odd", r#"{"name":"is-odd","version":"1.0.0"}"#),
            (
                "packages/app",
                r#"{"name":"app","version":"1.0.0","dependencies":{"is-odd":"^3.0.1","is-number":"^7.0.0"}}"#,
            ),
        ],
        &[("is-odd", "workspace:*")],
    );

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains(r#""app/is-odd/is-number": ["is-number@6.0.0""#),
        "{written}"
    );
    let reparsed = parse(&path).unwrap();
    let app_number = reparsed.importers["packages/app"]
        .iter()
        .find(|dep| dep.name == "is-number")
        .unwrap();
    assert_eq!(app_number.dep_path, "is-number@7.0.0", "{written}");
    assert_eq!(
        reparsed.packages["is-odd@3.0.1"].dependencies["is-number"],
        "6.0.0"
    );
}
