use super::berry::{
    PatchSelector, parse_berry_spec, patch_protocol_path, range_has_protocol, split_berry_header,
};
use super::classic::{parse_npm_alias_real_name, parse_spec_name};
use super::*;
use crate::{DepType, LocalSource, LockedPackage};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn make_manifest(deps: &[(&str, &str)], dev: &[(&str, &str)]) -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: deps
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        dev_dependencies: dev
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        peer_dependencies: Default::default(),
        optional_dependencies: Default::default(),
        update_config: None,
        scripts: Default::default(),
        engines: Default::default(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: Default::default(),
    }
}

#[test]
fn test_parse_spec_name() {
    assert_eq!(parse_spec_name("foo@^1.0.0"), Some("foo".to_string()));
    assert_eq!(parse_spec_name("foo@1.2.3"), Some("foo".to_string()));
    assert_eq!(
        parse_spec_name("@scope/pkg@^1.0.0"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(parse_spec_name("foo"), None);
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  resolved "https://example.com/foo-1.2.3.tgz"
  integrity sha512-aaa
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  resolved "https://example.com/bar-2.5.0.tgz"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    assert_eq!(
        foo.tarball_url.as_deref(),
        Some("https://example.com/foo-1.2.3.tgz")
    );
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn classic_import_preserves_custom_registry_tarball_url() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@scope/pkg@^1.0.0":
  version "1.0.0"
  resolved "https://npm.example.test/@scope/pkg/-/pkg-1.0.0.tgz#0123456789abcdef0123456789abcdef01234567"
  integrity sha512-private
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let pkg = graph
        .packages
        .get("@scope/pkg@1.0.0")
        .expect("scoped package");
    assert_eq!(
        pkg.tarball_url.as_deref(),
        Some("https://npm.example.test/@scope/pkg/-/pkg-1.0.0.tgz")
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    crate::pnpm::write_with_project_root(
        out.path(),
        out.path().parent().unwrap(),
        &graph,
        &manifest,
    )
    .unwrap();
    let yaml = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        yaml.contains("tarball: https://npm.example.test/@scope/pkg/-/pkg-1.0.0.tgz"),
        "{yaml}"
    );

    let round_trip_graph = crate::pnpm::parse(out.path()).unwrap();
    let second_out = tempfile::NamedTempFile::new().unwrap();
    crate::pnpm::write_with_project_root(
        second_out.path(),
        second_out.path().parent().unwrap(),
        &round_trip_graph,
        &manifest,
    )
    .unwrap();
    let second_yaml = std::fs::read_to_string(second_out.path()).unwrap();
    assert!(
        second_yaml.contains("tarball: https://npm.example.test/@scope/pkg/-/pkg-1.0.0.tgz"),
        "{second_yaml}"
    );
}

#[test]
fn classic_parse_does_not_treat_git_resolved_url_as_tarball_url() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"git-pkg@git+https://github.com/acme/git-pkg.git#main":
  version "1.0.0"
  resolved "https://github.com/acme/git-pkg.git#abcdef0123456789abcdef0123456789abcdef01"
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[("git-pkg", "git+https://github.com/acme/git-pkg.git#main")],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();
    let pkg = graph.packages.get("git-pkg@1.0.0").expect("git package");
    assert!(pkg.tarball_url.is_none());
}

#[test]
fn test_parse_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
  version "1.1.0"
  integrity sha512-zzz
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// Yarn classic supports the `npm:` protocol to rename a dep on
/// import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under
/// `node_modules/react-loadable/`. The lockfile records the alias
/// in the spec key and the real name only behind the `npm:` value.
/// Without surfacing the real name into `LockedPackage.alias_of`,
/// the install path would fetch the alias-qualified URL and 404
/// (https://github.com/aubepkg/aube/discussions/681).
#[test]
fn test_parse_npm_protocol_alias_transitive() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@docusaurus/core@2.1.0":
  version "2.1.0"
  integrity sha512-aaa
  dependencies:
    react-loadable "npm:@docusaurus/react-loadable@5.5.2"

"react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@docusaurus/core", "2.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = graph
        .packages
        .get("react-loadable@5.5.2")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(aliased.name, "react-loadable");
    assert_eq!(aliased.version, "5.5.2");
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
    assert_eq!(aliased.registry_name(), "@docusaurus/react-loadable");

    // The parent must still resolve the transitive ref to the
    // alias dep_path — symlinks under node_modules/.aube/<parent>/
    // key on the alias, not the real name.
    let core = &graph.packages["@docusaurus/core@2.1.0"];
    assert_eq!(
        core.dependencies.get("react-loadable").map(String::as_str),
        Some("5.5.2")
    );
}

#[test]
fn test_parse_classic_dependency_values_are_dep_path_tails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@rollup/plugin-replace@2.4.1":
  version "2.4.1"
  integrity sha512-aaa
  dependencies:
    "@rollup/pluginutils" "^3.1.0"
    magic-string "^0.25.9"

"@rollup/pluginutils@^3.1.0":
  version "3.1.0"
  integrity sha512-bbb

magic-string@^0.25.9:
  version "0.25.9"
  integrity sha512-ccc
  dependencies:
    sourcemap-codec "^1.4.8"

sourcemap-codec@^1.4.8:
  version "1.4.8"
  integrity sha512-ddd
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@rollup/plugin-replace", "2.4.1")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let replace = &graph.packages["@rollup/plugin-replace@2.4.1"];
    assert_eq!(
        replace
            .dependencies
            .get("@rollup/pluginutils")
            .map(String::as_str),
        Some("3.1.0")
    );
    assert_eq!(
        replace.dependencies.get("magic-string").map(String::as_str),
        Some("0.25.9")
    );

    let magic_string = &graph.packages["magic-string@0.25.9"];
    assert_eq!(
        magic_string
            .dependencies
            .get("sourcemap-codec")
            .map(String::as_str),
        Some("1.4.8")
    );
}

/// Round-trip safety: our writer emits the canonical
/// `"name@version"` spec first and the npm-alias spec alongside it.
/// On reparse the `[0]` spec carries no `npm:`, so the alias must
/// be detected by scanning every spec in the header — not just the
/// first one.
#[test]
fn test_parse_npm_protocol_alias_canonical_spec_first() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"react-loadable@5.5.2", "react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = &graph.packages["react-loadable@5.5.2"];
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
}

#[test]
fn test_parse_npm_alias_real_name_helper() {
    assert_eq!(
        parse_npm_alias_real_name("react-loadable@npm:@docusaurus/react-loadable@5.5.2"),
        Some("@docusaurus/react-loadable".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("h3-v2@npm:h3@2.0.1-rc.20"),
        Some("h3".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("@my-scope/alias@npm:@upstream/pkg@^1.0.0"),
        Some("@upstream/pkg".to_string())
    );
    // No npm: protocol — the common case.
    assert_eq!(parse_npm_alias_real_name("foo@^1.0.0"), None);
    assert_eq!(parse_npm_alias_real_name("@scope/pkg@^1.0.0"), None);
    // Other protocols pass through as non-aliases (workspace:, file:, …).
    assert_eq!(parse_npm_alias_real_name("foo@workspace:*"), None);
}

#[test]
fn test_detect_berry_vs_classic() {
    // The `__metadata:` marker is what distinguishes berry from
    // classic; `is_berry` is the primary dispatcher signal so we
    // assert it fires on every version berry has emitted
    // (`__metadata.version` 3 through 8 across yarn 2–4).
    assert!(is_berry("__metadata:\n  version: 6\n"));
    assert!(is_berry("# comment\n__metadata:\n  version: 8\n"));
    assert!(!is_berry(
        "# yarn lockfile v1\n\nfoo@^1.0.0:\n  version \"1.0.0\"\n"
    ));
}

#[test]
fn test_write_classic_drops_saved_resolved_when_tarball_url_changes() {
    let content = r#"# yarn lockfile v1


foo@^1.0.0:
  version "1.2.3"
  resolved "https://registry.yarnpkg.com/foo/-/foo-1.2.3.tgz#0123"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let mut graph = parse(tmp.path(), &manifest).unwrap();
    graph.packages.get_mut("foo@1.2.3").unwrap().tarball_url =
        Some("https://npm.example.com/foo/-/foo-1.2.3.tgz".to_string());

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("resolved \"https://npm.example.com/foo/-/foo-1.2.3.tgz\""));
    assert!(!written.contains("registry.yarnpkg.com"));
}

#[test]
fn test_write_classic_rewrites_git_resolved_when_commit_changes() {
    let content = r#"# yarn lockfile v1


"foo@git+https://github.com/o/foo.git":
  version "1.0.0"
  resolved "git+https://github.com/o/foo.git#aaaaaaaa"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "git+https://github.com/o/foo.git")], &[]);
    let mut graph = parse(tmp.path(), &manifest).unwrap();
    graph.packages.get_mut("foo@1.0.0").unwrap().local_source =
        Some(LocalSource::Git(crate::GitSource {
            url: "https://github.com/o/foo.git".to_string(),
            committish: None,
            resolved: "bbbbbbbb".to_string(),
            integrity: None,
            subpath: None,
        }));

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("resolved \"git+https://github.com/o/foo.git#bbbbbbbb\""));
    assert!(!written.contains("aaaaaaaa"));
}

#[test]
fn test_write_classic_new_entry_resolved_urls() {
    let manifest = make_manifest(&[("foo", "^1.0.0"), ("bar", "^2.0.0")], &[]);
    let mut graph = LockfileGraph::default();
    let mut direct = Vec::new();
    for (name, version, url) in [
        (
            "foo",
            "1.0.0",
            Some("https://registry.npmjs.org/foo/-/foo-1.0.0.tgz"),
        ),
        ("bar", "2.0.0", None),
    ] {
        let dep_path = format!("{name}@{version}");
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: dep_path.clone(),
                tarball_url: url.map(str::to_string),
                ..Default::default()
            },
        );
        direct.push(crate::DirectDep {
            name: name.to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: None,
        });
    }
    graph.importers.insert(".".to_string(), direct);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("resolved \"https://registry.yarnpkg.com/foo/-/foo-1.0.0.tgz\""));
    assert_eq!(written.matches("resolved").count(), 1);
}

#[test]
fn test_write_classic_matches_yarn_layout() {
    let content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1


"@scope/baz@^3.0.0":
  version "3.1.0"
  resolved "https://registry.yarnpkg.com/@scope/baz/-/baz-3.1.0.tgz#abc123"
  integrity sha512-baz

bar@~2.0.0:
  version "2.0.1"
  resolved "https://registry.yarnpkg.com/bar/-/bar-2.0.1.tgz#def456"
  integrity sha512-bar

foo@1.2.3:
  version "1.2.3"
  resolved "https://registry.yarnpkg.com/foo/-/foo-1.2.3.tgz#0123"
  integrity sha512-foo
  dependencies:
    "@scope/baz" "^3.0.0"
    bar "~2.0.0"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "1.2.3")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    assert_eq!(std::fs::read_to_string(out.path()).unwrap(), content);
}

/// Parse → write → parse should preserve package set,
/// versions, integrity, and the resolved transitive graph. If
/// the writer emits malformed block headers or forgets to
/// requote, round-trip breaks here.
#[test]
fn test_write_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  integrity sha512-foo
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  integrity sha512-bar
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();

    // Re-parse the output. The manifest is the same — direct-dep
    // resolution requires a spec key of `foo@^1.0.0`, but the
    // writer emits `"foo@1.2.3"`. So direct-dep lookup will
    // miss; we only assert the packages/transitives round-trip.
    let reparsed_manifest = make_manifest(&[], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].integrity.as_deref(),
        Some("sha512-foo")
    );
    // foo's transitive dep on bar must still resolve: the writer
    // emits `bar "2.5.0"` under foo's dependencies, and reparse
    // finds the block keyed `"bar@2.5.0"` via spec_to_dep_path.
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("2.5.0")
    );
}

#[test]
fn test_dev_dep_classification() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"foo@^1.0.0:
  version "1.0.0"
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[("foo", "^1.0.0")]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].dep_type, DepType::Dev);
}

// ---- berry (v2+) ---------------------------------------------------

#[test]
fn test_parse_berry_spec() {
    assert_eq!(
        parse_berry_spec("lodash@npm:^4.17.0"),
        Some(("lodash", "npm", "^4.17.0"))
    );
    assert_eq!(
        parse_berry_spec("@types/node@npm:20.1.0"),
        Some(("@types/node", "npm", "20.1.0"))
    );
    assert_eq!(
        parse_berry_spec("my-pkg@workspace:."),
        Some(("my-pkg", "workspace", "."))
    );
    // Missing protocol colon: malformed.
    assert_eq!(parse_berry_spec("no-protocol"), None);
}

#[test]
fn test_split_berry_header() {
    let specs = split_berry_header("lodash@npm:^4.17.0, lodash@npm:^4.18.0");
    assert_eq!(
        specs,
        vec![
            "lodash@npm:^4.17.0".to_string(),
            "lodash@npm:^4.18.0".to_string()
        ]
    );
    let single = split_berry_header("foo@npm:1.0.0");
    assert_eq!(single, vec!["foo@npm:1.0.0".to_string()]);
}

#[test]
fn test_range_has_protocol() {
    assert!(range_has_protocol("npm:^1.0.0"));
    assert!(range_has_protocol("workspace:*"));
    assert!(range_has_protocol("file:./pkgs/foo"));
    assert!(range_has_protocol("patch:react@^18.0.0#./mypatch.patch"));
    // Compound transports: berry emits these for git-over-ssh /
    // git-over-https, and the writer must not re-prefix them with
    // `npm:` when building header specs from the manifest range.
    assert!(range_has_protocol("git+ssh://git@github.com/u/r.git"));
    assert!(range_has_protocol("git+https://github.com/u/r.git"));
    assert!(range_has_protocol("git+file:./vendored.git"));
    // Bare semver ranges never have a protocol.
    assert!(!range_has_protocol("^1.0.0"));
    assert!(!range_has_protocol("1.2.3"));
    assert!(!range_has_protocol(">=1.0 <2.0"));
}

/// Realistic yarn 4 lockfile with `npm:` deps — the overwhelming
/// majority real-world case. Exercises `__metadata` parsing,
/// multi-spec block headers, nested `dependencies:`, and the
/// direct-dep pass that prepends `npm:` to manifest ranges.
#[test]
fn test_parse_berry_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/abcdef
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/123456
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.version, "1.2.3");
    assert_eq!(foo.yarn_checksum.as_deref(), Some("10c0/abcdef"));
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("bar@2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_parse_berry_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123":
  version: 7.0.0
  resolution: "is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("is-number@7.0.0"));
    assert_eq!(
        graph
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some("./.yarn/patches/is-number.patch")
    );
    assert_eq!(graph.importers["."][0].dep_path, "is-number@7.0.0");
}

#[test]
fn test_parse_berry_skips_builtin_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"glob@patch:glob@npm%3A8.1.0#builtin<compat/glob>":
  version: 8.1.0
  resolution: "glob@patch:glob@npm%3A8.1.0#builtin<compat/glob>"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("glob", "^8.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.is_empty());
    assert!(graph.patched_dependencies.is_empty());
    assert!(graph.importers["."].is_empty());
}

/// `optional!builtin<…>` is the canonical compat-patch spelling in real
/// yarn 4 lockfiles; it must be skipped like a bare `builtin<…>`.
#[test]
fn test_parse_berry_skips_optional_builtin_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"fsevents@patch:fsevents@npm%3A2.3.2#optional!builtin<compat/fsevents>::version=2.3.2&hash=df0bf1":
  version: 2.3.2
  resolution: "fsevents@patch:fsevents@npm%3A2.3.2#optional!builtin<compat/fsevents>::version=2.3.2&hash=df0bf1"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("fsevents", "^2.3.2")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.is_empty());
    assert!(graph.patched_dependencies.is_empty());
    assert!(graph.importers["."].is_empty());
}

/// A `&`-joined selector can mix a real project patch with a builtin
/// compat patch; keep the real file, ignore the builtin segment.
#[test]
fn test_parse_berry_composed_patch_keeps_real_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@patch:foo@npm%3A1.0.0#./.yarn/patches/foo.patch&optional!builtin<compat/foo>::version=1.0.0&hash=abc123":
  version: 1.0.0
  resolution: "foo@patch:foo@npm%3A1.0.0#./.yarn/patches/foo.patch&optional!builtin<compat/foo>::version=1.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        graph
            .patched_dependencies
            .get("foo@1.0.0")
            .map(String::as_str),
        Some("./.yarn/patches/foo.patch")
    );
}

/// Composed selectors are order-independent: builtin segment first, real patch still kept.
#[test]
fn test_parse_berry_composed_patch_builtin_first_keeps_real_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@patch:foo@npm%3A1.0.0#optional!builtin<compat/foo>&./.yarn/patches/foo.patch::version=1.0.0&hash=abc123":
  version: 1.0.0
  resolution: "foo@patch:foo@npm%3A1.0.0#optional!builtin<compat/foo>&./.yarn/patches/foo.patch::version=1.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        graph
            .patched_dependencies
            .get("foo@1.0.0")
            .map(String::as_str),
        Some("./.yarn/patches/foo.patch")
    );
}

/// A selector listing two real patch files can't be represented (aube stores
/// one patch path per package). Rather than apply only the first and
/// materialize a package that doesn't match the lockfile, parsing fails
/// loudly. Yarn permits the form but its CLI never emits it.
#[test]
fn test_parse_berry_multiple_real_patches_errors() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@patch:foo@npm%3A1.0.0#./.yarn/patches/a.patch&./.yarn/patches/b.patch::version=1.0.0&hash=abc123":
  version: 1.0.0
  resolution: "foo@patch:foo@npm%3A1.0.0#./.yarn/patches/a.patch&./.yarn/patches/b.patch::version=1.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    // Pin the failure to the multi-patch guard, not an unrelated parse error,
    // so this can't pass for the wrong reason.
    let err = parse(tmp.path(), &manifest).expect_err("multi-patch selector must fail");
    assert!(
        err.to_string().contains("multiple patch files"),
        "expected the multi-patch guard to fire, got: {err}"
    );
}

/// Yarn writes project-root-relative patch paths with a `~/` prefix
/// (the default form, e.g. `#~/.yarn/patches/foo.patch`). The `~/` must
/// be stripped or the materializer looks for a literal `~` dir and fails.
#[test]
fn test_parse_berry_patch_protocol_project_root_tilde() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"ink@patch:ink@npm%3A3.2.0#~/.yarn/patches/ink-npm-3.2.0-2f1df5b094.patch":
  version: 3.2.0
  resolution: "ink@patch:ink@npm%3A3.2.0#~/.yarn/patches/ink-npm-3.2.0-2f1df5b094.patch::version=3.2.0&hash=b7953f"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("ink", "^3.2.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("ink@3.2.0"));
    // The `~/` project-root prefix is stripped (Yarn `onProject`), so the
    // path resolves against the project root just like a `./` path.
    assert_eq!(
        graph
            .patched_dependencies
            .get("ink@3.2.0")
            .map(String::as_str),
        Some(".yarn/patches/ink-npm-3.2.0-2f1df5b094.patch")
    );
}

/// Builtin compat patches coexist with the plain `name@npm:…` block, and
/// transitive consumers reference the npm spec — skipping the builtin
/// patch must leave that edge resolving cleanly, never strand the package.
#[test]
fn test_parse_berry_builtin_skip_keeps_transitive_npm_edge() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"consumer@npm:^1.0.0":
  version: 1.0.0
  resolution: "consumer@npm:1.0.0"
  dependencies:
    resolve: "npm:^1.20.0"
  checksum: 10c0/consumer
  languageName: node
  linkType: hard

"resolve@npm:1.22.2, resolve@npm:^1.20.0":
  version: 1.22.2
  resolution: "resolve@npm:1.22.2"
  checksum: 10c0/resolvereal
  languageName: node
  linkType: hard

"resolve@patch:resolve@npm%3A1.22.2#optional!builtin<compat/resolve>, resolve@patch:resolve@npm%3A^1.20.0#optional!builtin<compat/resolve>":
  version: 1.22.2
  resolution: "resolve@patch:resolve@npm%3A1.22.2#optional!builtin<compat/resolve>::version=1.22.2&hash=c3c19d"
  checksum: 10c0/resolvepatched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("consumer", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // The builtin patch block is skipped: no bogus patch entry, only the
    // consumer and the real npm resolve survive.
    assert!(graph.patched_dependencies.is_empty());
    assert_eq!(graph.packages.len(), 2);
    assert!(graph.packages.contains_key("consumer@1.0.0"));
    assert!(graph.packages.contains_key("resolve@1.22.2"));

    // The transitive edge still resolves to the real npm package — the
    // skip didn't strand it.
    assert_eq!(
        graph.packages["consumer@1.0.0"]
            .dependencies
            .get("resolve")
            .map(String::as_str),
        Some("resolve@1.22.2")
    );
}

/// The compat-builtin skip must be identical across every
/// `__metadata.version` and every spelling Yarn has emitted:
///   `builtin<…>`           yarn 2.x      (pre-flag)
///   `~builtin<…>`          yarn 3.0–3.6  (leading-`~` optional flag)
///   `optional!builtin<…>`  yarn 4.0+     (`!`-delimited flag)
#[test]
fn test_parse_berry_builtin_skip_is_meta_version_independent() {
    for v in [3u64, 6, 8] {
        for sel in [
            "builtin<compat/fsevents>",
            "~builtin<compat/fsevents>",
            "optional!builtin<compat/fsevents>",
        ] {
            let res =
                format!("fsevents@patch:fsevents@npm%3A2.3.2#{sel}::version=2.3.2&hash=df0bf1");
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let content = format!(
                "__metadata:\n  version: {v}\n  cacheKey: 10c0\n\n\"{res}\":\n  version: 2.3.2\n  resolution: \"{res}\"\n  checksum: 10c0/patched\n  languageName: node\n  linkType: hard\n"
            );
            std::fs::write(tmp.path(), &content).unwrap();
            let manifest = make_manifest(&[("fsevents", "^2.3.2")], &[]);
            let graph = parse(tmp.path(), &manifest).unwrap();
            assert!(
                graph.packages.is_empty(),
                "v{v} sel={sel}: package not skipped"
            );
            assert!(
                graph.patched_dependencies.is_empty(),
                "v{v} sel={sel}: bogus patch recorded"
            );
            assert!(
                graph.importers["."].is_empty(),
                "v{v} sel={sel}: importer not empty"
            );
        }
    }
}

/// All-builtin multi-segment selectors (two compat shims joined with `&`) have
/// no real file to apply, so they classify as `Skip`, never `Multiple`. The
/// `Multiple` guard counts real paths *after* the builtin filter, so this
/// guards against a regression that miscounts builtins toward the limit and
/// wrongly hard-errors a perfectly normal two-builtin block.
#[test]
fn test_patch_protocol_all_builtin_multi_is_skip() {
    let body = "fsevents@npm%3A2.3.2#builtin<compat/a>&optional!builtin<compat/b>::version=2.3.2&hash=df0bf1";
    assert!(matches!(patch_protocol_path(body), PatchSelector::Skip));
}

/// Yarn's `optional` flag can prefix a *real* patch, not just a builtin. aube
/// strips the `!`-delimited flags and keeps the path (it does not yet preserve
/// the optionality: Yarn tolerates a failed apply, aube treats it as required;
/// see the investigation's known limitations). Every other `!` test path is a
/// builtin that gets filtered, so this pins the `!`-strip on a *kept* path.
#[test]
fn test_patch_protocol_optional_flag_on_real_patch_keeps_path() {
    let body = "foo@npm%3A1.0.0#optional!./.yarn/patches/a.patch::version=1.0.0&hash=abc123";
    assert!(matches!(
        patch_protocol_path(body),
        PatchSelector::Single(p) if p == "./.yarn/patches/a.patch"
    ));
}

/// One Yarn-shaped patch-selector segment (every form the grammar emits).
fn berry_patch_segment() -> impl Strategy<Value = String> {
    let name = "[a-z][a-z0-9._/-]{0,12}";
    prop_oneof![
        name.prop_map(|n| format!("builtin<compat/{n}>")),
        name.prop_map(|n| format!("~builtin<compat/{n}>")),
        name.prop_map(|n| format!("optional!builtin<compat/{n}>")),
        name.prop_map(|n| format!("~/.yarn/patches/{n}.patch")),
        name.prop_map(|n| format!("./.yarn/patches/{n}.patch")),
        name.prop_map(|n| format!(".yarn/patches/{n}.patch")),
        name.prop_map(|n| format!("optional!~/.yarn/patches/{n}.patch")),
    ]
}

proptest! {
    /// The bug-class invariant: for any `&`-joined selector of Yarn-shaped
    /// segments, a `PatchSelector::Single` path is always clean — never a
    /// leftover `~` prefix and never a `builtin<…>` marker (the exact forms
    /// that broke installs). Must hold across arbitrary combinations, with or
    /// without `::`-params.
    #[test]
    fn prop_patch_protocol_path_never_leaks_builtin_or_tilde(
        segments in prop::collection::vec(berry_patch_segment(), 1..4),
        with_params in any::<bool>(),
    ) {
        let params = if with_params { "::version=1.0.0&hash=deadbeef" } else { "" };
        let body = format!("foo@npm%3A1.0.0#{}{params}", segments.join("&"));
        if let PatchSelector::Single(p) = patch_protocol_path(&body) {
            prop_assert!(!p.is_empty(), "empty path from {body}");
            prop_assert!(!p.starts_with('~'), "leaked ~ prefix {p:?} from {body}");
            prop_assert!(
                !(p.starts_with("builtin<") && p.ends_with('>')),
                "leaked builtin marker {p:?} from {body}"
            );
        }
    }

    /// Robustness: arbitrary garbage after `#` must never panic, and a
    /// returned path is never empty.
    #[test]
    fn prop_patch_protocol_path_no_panic_on_arbitrary_input(selector in ".*") {
        if let PatchSelector::Single(p) = patch_protocol_path(&format!("foo#{selector}")) {
            prop_assert!(!p.is_empty());
        }
    }
}

/// Scoped package names (`@types/node`) and the `, `-joined
/// multi-spec header format berry uses when two package.json
/// ranges resolve to the same version.
#[test]
fn test_parse_berry_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
  version: 1.1.0
  resolution: "@scope/pkg@npm:1.1.0"
  checksum: 10c0/zzz
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// Blocks for the project's own workspace entry shouldn't become
/// `LockedPackage`s — they're the root importer, not a
/// resolved dep. Skipping them keeps the graph shape identical to
/// what parsing the `package.json` alone would produce.
#[test]
fn test_parse_berry_skips_workspace_root() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"my-project@workspace:.":
  version: 0.0.0-use.local
  resolution: "my-project@workspace:."
  dependencies:
    foo: "npm:^1.0.0"
  languageName: unknown
  linkType: soft

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // Workspace block is skipped; only the real resolved dep survives.
    assert_eq!(graph.packages.len(), 1);
    assert!(graph.packages.contains_key("foo@1.0.0"));
    assert!(!graph.packages.contains_key("my-project@0.0.0-use.local"));
}

/// Berry emits `version:` unquoted, so scalar-looking values can
/// parse as numbers instead of strings. Our parser must unfold
/// those back to strings instead of failing with "has no version" —
/// real packages with fewer-than-three-component versions do exist
/// (even if rare).
#[test]
fn test_parse_berry_unquoted_numeric_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"int-version@npm:5":
  version: 5
  resolution: "int-version@npm:5"
  languageName: node
  linkType: hard

"two-part@npm:1.0":
  version: 1.0
  resolution: "two-part@npm:1.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("int-version@5"));
    assert!(graph.packages.contains_key("two-part@1.0"));
    assert_eq!(graph.packages["int-version@5"].version, "5");
    assert_eq!(graph.packages["two-part@1.0"].version, "1.0");
}

/// Same scalar hazard applies to dependency values:
/// `peerDependencies: { foo: 5 }` writes a YAML number, and
/// boolean-looking tags or ranges can parse as booleans. The parser
/// routes dep values through `yaml_scalar_as_string` so a future
/// regression shows up as a missing peer edge rather than a parse
/// error.
#[test]
fn test_parse_berry_typed_dep_values() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  peerDependencies:
    numeric-peer: 5
    bool-peer: true
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.peer_dependencies
            .get("numeric-peer")
            .map(String::as_str),
        Some("5")
    );
    assert_eq!(
        foo.peer_dependencies.get("bool-peer").map(String::as_str),
        Some("true")
    );
}

/// Berry's `https:` tarball protocol and `git+ssh:` / `git:`
/// transports both survive parsing with a populated
/// `LocalSource`, rather than falling through to the "unknown
/// protocol" skip path.
///
/// The hazard this guards against: `parse_berry_spec` splits
/// `"foo@https://host/path"` into `res_protocol = "https"` /
/// `res_body = "//host/path"` — the body never starts with
/// `https://`, so a URL-body check would always miss. Parsing the
/// file and verifying the package lands in the graph with the
/// right `LocalSource` catches any future regression of the
/// dispatch match arms.
#[test]
fn test_parse_berry_http_and_git_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"tarball-pkg@https://example.com/pkg-1.0.0.tgz":
  version: 1.0.0
  resolution: "tarball-pkg@https://example.com/pkg-1.0.0.tgz"
  languageName: node
  linkType: hard

"git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01":
  version: 2.0.0
  resolution: "git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01"
  languageName: node
  linkType: hard

"ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef":
  version: 3.0.0
  resolution: "ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // All three packages should be present — none silently
    // skipped as "unrecognized protocol". The `.values()` scan
    // below asserts the `LocalSource` shape for each.
    assert_eq!(graph.packages.len(), 3);
    let by_name: BTreeMap<&str, &LockedPackage> = graph
        .packages
        .values()
        .map(|p| (p.name.as_str(), p))
        .collect();

    // `.tgz` on https → remote tarball.
    let tar = by_name["tarball-pkg"];
    assert!(matches!(
        &tar.local_source,
        Some(LocalSource::RemoteTarball(_))
    ));

    // `.git` on https → git source, not tarball.
    let git = by_name["git-pkg"];
    let Some(LocalSource::Git(git)) = &git.local_source else {
        panic!("expected git LocalSource");
    };
    assert_eq!(git.url, "https://github.com/user/repo.git");
    assert_eq!(git.resolved, "abcdef0123456789abcdef0123456789abcdef01");

    // `git+ssh:` prefix → git source.
    let ssh = by_name["ssh-git-pkg"];
    assert!(matches!(&ssh.local_source, Some(LocalSource::Git(_))));
}

#[test]
fn test_parse_berry_portal_and_exec_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"portal-pkg@portal:./packages/portal":
  version: 1.0.0
  resolution: "portal-pkg@portal:./packages/portal"
  dependencies:
    left-pad: "npm:^1.3.0"
  languageName: node
  linkType: soft

"exec-pkg@exec:./scripts/generate-exec.js":
  version: 2.0.0
  resolution: "exec-pkg@exec:./scripts/generate-exec.js"
  languageName: node
  linkType: hard

"left-pad@npm:^1.3.0":
  version: 1.3.0
  resolution: "left-pad@npm:1.3.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[
            ("portal-pkg", "portal:./packages/portal"),
            ("exec-pkg", "exec:./scripts/generate-exec.js"),
        ],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    let portal_key = LocalSource::Portal(PathBuf::from("./packages/portal")).dep_path("portal-pkg");
    let portal = &graph.packages[&portal_key];
    assert!(matches!(
        &portal.local_source,
        Some(LocalSource::Portal(p)) if p == &PathBuf::from("./packages/portal")
    ));
    assert_eq!(
        portal.dependencies.get("left-pad").map(String::as_str),
        Some("left-pad@1.3.0")
    );

    let exec_key =
        LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js")).dep_path("exec-pkg");
    let exec = &graph.packages[&exec_key];
    assert!(matches!(
        &exec.local_source,
        Some(LocalSource::Exec(p)) if p == &PathBuf::from("./scripts/generate-exec.js")
    ));
    assert_eq!(graph.importers["."].len(), 2);
}

/// Round-trip: parse berry → write berry → parse berry should
/// preserve packages, versions, checksum (via `yarn_checksum`),
/// and transitive edges. This is the core round-trip contract.
#[test]
fn test_write_berry_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();

    // Confirm the output is berry-shaped so dispatcher picks the
    // right parser on reparse.
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(is_berry(&written));

    let reparsed_manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].yarn_checksum.as_deref(),
        Some("10c0/foohash")
    );
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("bar@2.5.0")
    );
    // The manifest spec `foo@^1.0.0` appears verbatim (with `npm:`
    // prepended) in the block header, so direct-dep lookup
    // succeeds on reparse — which it did NOT for classic, so this
    // is a stronger round-trip guarantee.
    let root = reparsed.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_write_berry_roundtrips_patch_protocol() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            yarn_checksum: Some("10c0/patched".to_string()),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(
                ".".to_string(),
                vec![crate::DirectDep {
                    name: "is-number".to_string(),
                    dep_path: "is-number@7.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                }],
            );
            m
        },
        packages,
        patched_dependencies: BTreeMap::from([(
            "is-number@7.0.0".to_string(),
            ".yarn/patches/is-number.patch".to_string(),
        )]),
        ..Default::default()
    };
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    let full_spec = "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123";
    assert!(written.contains(full_spec));
    assert!(!written.contains(&format!(
        "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch, {full_spec}"
    )));

    let reparsed = parse(out.path(), &manifest).unwrap();
    assert_eq!(
        reparsed
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some(".yarn/patches/is-number.patch")
    );
    assert_eq!(reparsed.importers["."][0].dep_path, "is-number@7.0.0");
}

/// `link:` deps are pure symlinks in berry's model, which means
/// the block must carry `linkType: soft` — writing `hard` makes
/// yarn's own linker try to copy/hardlink the target into the
/// virtual store on the next install. Registry packages (no
/// `local_source`) stay `hard`, the default.
#[test]
fn test_write_berry_link_type_soft_for_link_deps() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "linked-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "linked-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "linked-pkg@1.0.0".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("./vendor/linked-pkg"))),
            ..Default::default()
        },
    );
    packages.insert(
        "regular-pkg@2.0.0".to_string(),
        LockedPackage {
            name: "regular-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "regular-pkg@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The `link:` block gets `soft`; the registry block stays `hard`.
    // Block order is sorted by canonical key, so `linked-pkg`
    // comes before `regular-pkg` and each block's `linkType`
    // appears after its `languageName` line.
    let linked_idx = written.find("linked-pkg@").unwrap();
    let regular_idx = written.find("regular-pkg@").unwrap();
    let linked_block = &written[linked_idx..regular_idx];
    let regular_block = &written[regular_idx..];
    assert!(
        linked_block.contains("linkType: soft"),
        "link: block should be soft-linked:\n{linked_block}"
    );
    assert!(
        regular_block.contains("linkType: hard"),
        "registry block should be hard-linked:\n{regular_block}"
    );
}

#[test]
fn test_write_berry_roundtrips_portal_and_exec_protocols() {
    let portal_source = LocalSource::Portal(PathBuf::from("./packages/portal"));
    let exec_source = LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js"));
    let mut packages = BTreeMap::new();
    packages.insert(
        portal_source.dep_path("portal-pkg"),
        LockedPackage {
            name: "portal-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: portal_source.dep_path("portal-pkg"),
            local_source: Some(portal_source),
            ..Default::default()
        },
    );
    packages.insert(
        exec_source.dep_path("exec-pkg"),
        LockedPackage {
            name: "exec-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: exec_source.dep_path("exec-pkg"),
            local_source: Some(exec_source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(written.contains("portal-pkg@portal:./packages/portal"));
    assert!(written.contains("exec-pkg@exec:./scripts/generate-exec.js"));
    let portal_idx = written.find("portal-pkg@portal:").unwrap();
    let exec_idx = written.find("exec-pkg@exec:").unwrap();
    assert!(
        exec_idx < portal_idx,
        "expected exec block before portal block:\n{written}"
    );
    let portal_block = &written[portal_idx..];
    let exec_block = &written[exec_idx..portal_idx];
    assert!(portal_block.contains("linkType: soft"));
    assert!(exec_block.contains("linkType: hard"));
}

/// Header and `resolution:` both carry spec strings that may
/// contain backslashes (Windows-style `file:` paths) or embedded
/// quotes (patched-package descriptors). The writer must route
/// them through `quote_yaml_scalar` so the emitted YAML is
/// well-formed. We can't easily drive backslashes into the model
/// from a parsed berry file (berry itself doesn't emit them on
/// macOS/Linux), so we construct a package with a `file:` source
/// that contains a backslash directly and assert the output
/// escapes it and round-trips through `yaml_serde::from_str`.
#[test]
fn test_write_berry_escapes_resolution_and_header() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "weird-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "weird-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "weird-pkg@1.0.0".to_string(),
            // A file: source whose path has a backslash. The
            // header and resolution both become
            // `weird-pkg@file:./a\b/c`; without escaping, the
            // raw backslash in the YAML string would be a
            // malformed escape.
            local_source: Some(LocalSource::Directory(PathBuf::from("./a\\b/c"))),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The emitted file must parse as YAML — any missing escape
    // blows up here instead of corrupting a real install.
    let _doc: yaml_serde::Value = yaml_serde::from_str(&written)
        .unwrap_or_else(|e| panic!("berry writer produced malformed YAML: {e}\n{written}"));
}

/// A package declared in both `devDependencies` and
/// `optionalDependencies` must yield exactly one root `DirectDep`,
/// classified under the first declaring section (dev beats optional).
/// A second `DirectDep` for the same name reads as section drift under
/// `--frozen-lockfile` and makes the linker create the root symlink
/// twice. See discussion #1544.
#[test]
fn classic_dev_and_optional_overlap_yields_one_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "foo@^1.0.0:\n  version \"1.0.0\"\n").unwrap();
    let manifest = aube_manifest::PackageJson {
        optional_dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..make_manifest(&[], &[("foo", "^1.0.0")])
    };
    let graph = parse(tmp.path(), &manifest).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1, "expected one direct dep, got {root:?}");
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_type, DepType::Dev);
}

#[test]
fn berry_dev_and_optional_overlap_yields_one_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = aube_manifest::PackageJson {
        optional_dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..make_manifest(&[], &[("foo", "^1.0.0")])
    };
    let graph = parse(tmp.path(), &manifest).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1, "expected one direct dep, got {root:?}");
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_type, DepType::Dev);
}

const BERRY_V10_WITH_COMPAT_PATCH: &str = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 10
  cacheKey: 10c0

"hono@npm:4.6.0":
  version: 4.6.0
  resolution: "hono@npm:4.6.0"
  checksum: 10c0/2095be31
  languageName: node
  linkType: hard

"t@workspace:.":
  version: 0.0.0-use.local
  resolution: "t@workspace:."
  dependencies:
    hono: "npm:4.6.0"
    typescript: "npm:5.5.4"
  languageName: unknown
  linkType: soft

"typescript@npm:5.5.4":
  version: 5.5.4
  resolution: "typescript@npm:5.5.4"
  checksum: 10c0/aaaa
  languageName: node
  linkType: hard

"typescript@patch:typescript@npm%3A5.5.4#optional!builtin<compat/typescript>":
  version: 5.5.4
  resolution: "typescript@patch:typescript@npm%3A5.5.4#optional!builtin<compat/typescript>::version=5.5.4&hash=379a07"
  checksum: 10c0/bbbb
  languageName: node
  linkType: hard
"#;

fn berry_manifest() -> aube_manifest::PackageJson {
    let mut manifest = make_manifest(&[("hono", "4.6.0")], &[("typescript", "5.5.4")]);
    manifest.name = Some("t".to_string());
    manifest
}

#[test]
fn berry_write_preserves_metadata_workspace_block_and_compat_patch() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), BERRY_V10_WITH_COMPAT_PATCH).unwrap();
    let manifest = berry_manifest();
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(written.contains("__metadata:\n  version: 10\n  cacheKey: 10c0\n"));
    assert!(written.contains("  version: 4.6.0\n"), "{written}");
    assert!(written.contains("\"t@workspace:.\":\n  version: 0.0.0-use.local\n"));
    assert!(written.contains("    hono: \"npm:4.6.0\"\n"));
    assert!(written.contains(
        "\"typescript@patch:typescript@npm%3A5.5.4#optional!builtin<compat/typescript>\":\n  version: 5.5.4\n  resolution: \"typescript@patch:typescript@npm%3A5.5.4#optional!builtin<compat/typescript>::version=5.5.4&hash=379a07\"\n  checksum: 10c0/bbbb\n"
    ));
    assert_eq!(written, BERRY_V10_WITH_COMPAT_PATCH);
}

#[test]
fn berry_direct_deps_record_specifiers_so_manifest_drift_is_detected() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), BERRY_V10_WITH_COMPAT_PATCH).unwrap();
    let graph = parse(tmp.path(), &berry_manifest()).unwrap();

    let mut bumped = berry_manifest();
    bumped.dependencies.insert("hono".into(), "4.6.20".into());
    let status = graph.check_drift(&bumped, &BTreeMap::new(), &[], &BTreeMap::new());
    assert!(
        matches!(status, crate::DriftStatus::Stale { .. }),
        "{status:?}"
    );
    assert!(matches!(
        graph.check_drift(&berry_manifest(), &BTreeMap::new(), &[], &BTreeMap::new()),
        crate::DriftStatus::Fresh
    ));
}

#[test]
fn berry_drift_compares_manifest_against_root_workspace_block() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Another dependency's range makes `hono@npm:5.0.0` a known header, so
    // the new manifest range resolves in the parse; only the root block
    // (still recording 4.6.0) can reveal the drift.
    let lock = BERRY_V10_WITH_COMPAT_PATCH
        .replace("\"hono@npm:4.6.0\":", "\"hono@npm:4.6.0, hono@npm:5.0.0\":");
    std::fs::write(tmp.path(), lock).unwrap();
    let mut changed = berry_manifest();
    changed.dependencies.insert("hono".into(), "5.0.0".into());
    let graph = parse(tmp.path(), &changed).unwrap();
    assert!(matches!(
        graph.check_drift(&changed, &BTreeMap::new(), &[], &BTreeMap::new()),
        crate::DriftStatus::Stale { .. }
    ));
}

#[test]
fn berry_write_dedupes_shared_dev_dependency_and_quotes_special_keys() {
    let mut manifest = make_manifest(
        &[("foo", "^1.0.0")],
        &[("foo", "^2.0.0"), ("true", "1.0.0")],
    );
    manifest.name = Some("t".to_string());
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &LockfileGraph::default(), &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert_eq!(written.matches("    foo:").count(), 1, "{written}");
    assert!(written.contains("    foo: \"npm:^1.0.0\"\n"));
    assert!(written.contains("    \"true\": "));
    assert!(parse(out.path(), &make_manifest(&[], &[])).is_ok());
}

#[test]
fn berry_preserves_compat_patch_in_crlf_lockfile_and_numeric_cache_key() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let crlf = BERRY_V10_WITH_COMPAT_PATCH
        .replace("version: 10\n  cacheKey: 10c0", "version: 6\n  cacheKey: 8")
        .replace('\n', "\r\n");
    std::fs::write(tmp.path(), crlf).unwrap();
    let manifest = berry_manifest();
    let graph = parse(tmp.path(), &manifest).unwrap();
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("__metadata:\n  version: 6\n  cacheKey: 8\n"));
    assert!(written.contains("builtin<compat/typescript>::version=5.5.4&hash=379a07"));
}

#[test]
fn berry_root_block_folds_optional_dependencies_into_dependencies_meta() {
    let mut manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    manifest.name = Some("t".to_string());
    manifest
        .optional_dependencies
        .insert("fsevents".into(), "^2.0.0".into());
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &LockfileGraph::default(), &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains(
        "  dependencies:\n    foo: \"npm:^1.0.0\"\n    fsevents: \"npm:^2.0.0\"\n  dependenciesMeta:\n    fsevents:\n      optional: true\n  languageName: unknown\n"
    ), "{written}");
    assert!(!written.contains("optionalDependencies:"));
}

#[test]
fn berry_write_preserves_conditions() {
    let lock = r#"__metadata:
  version: 8
  cacheKey: 10c0

"fsevents@npm:^2.0.0":
  version: 2.3.3
  resolution: "fsevents@npm:2.3.3"
  checksum: 10c0/abc
  conditions: os=darwin
  languageName: node
  linkType: hard
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), lock).unwrap();
    let manifest = make_manifest(&[("fsevents", "^2.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        written.contains("  checksum: 10c0/abc\n  conditions: os=darwin\n  languageName: node\n")
    );
}

#[test]
fn berry_root_block_uses_regular_range_when_optional_overlaps() {
    let mut manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    manifest.name = Some("t".to_string());
    manifest
        .optional_dependencies
        .insert("foo".into(), "^2.0.0".into());
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &LockfileGraph::default(), &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("    foo: \"npm:^1.0.0\"\n"), "{written}");
    assert!(!written.contains("^2.0.0"));
}

#[test]
fn berry_write_escapes_conditions_that_would_inject_fields() {
    let lock = "__metadata:\n  version: 8\n  cacheKey: 10c0\n\n\"foo@npm:^1.0.0\":\n  version: 1.0.0\n  resolution: \"foo@npm:1.0.0\"\n  conditions: \"os=darwin\\n  injected: value\"\n  languageName: node\n  linkType: hard\n";
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), lock).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(!written.contains("\n  injected:"), "{written}");
    assert!(parse(out.path(), &manifest).is_ok());
}

#[test]
fn berry_rewrite_keeps_requested_descriptors_optional_meta_and_bare_peer_ranges() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
    native: "npm:^3.0.0"
  peerDependencies:
    core: ^1.7.1 || ^2.0.0-alpha.4
    any: "*"
  dependenciesMeta:
    native:
      optional: true
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard

"native@npm:^3.0.0":
  version: 3.1.0
  resolution: "native@npm:3.1.0"
  checksum: 10c0/nativehash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    assert!(
        graph.packages["foo@1.2.3"]
            .optional_dependencies
            .contains_key("native")
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(written.contains("\n\"bar@npm:^2.0.0\":\n"), "{written}");
    assert!(!written.contains("bar@npm:2.5.0, "), "{written}");
    assert!(
        written.contains("    core: ^1.7.1 || ^2.0.0-alpha.4\n"),
        "{written}"
    );
    assert!(written.contains("    any: \"*\"\n"), "{written}");
    assert!(
        written.contains("  dependenciesMeta:\n    native:\n      optional: true\n"),
        "{written}"
    );
    assert!(!written.contains("optionalDependencies"), "{written}");
}

#[test]
fn berry_header_keeps_exact_descriptor_when_requested_exactly() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:1.2.3, foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "1.2.3")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(written.contains("\n\"foo@npm:1.2.3\":\n"), "{written}");
}
