use super::layout::package_name_from_install_path;
use super::source::local_git_source_from_resolved;
use super::*;
use crate::{
    DepType, DirectDep, DriftStatus, Error, GitSource, LocalSource, LockedPackage, LockfileGraph,
    LockfileKind,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[test]
fn test_package_name_from_install_path() {
    assert_eq!(
        package_name_from_install_path("node_modules/foo"),
        Some("foo".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/bar"),
        Some("bar".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" },
                    "devDependencies": { "bar": "^2.0.0" }
                },
                "node_modules/foo": {
                    "version": "1.2.3",
                    "integrity": "sha512-aaa",
                    "dependencies": { "nested": "^3.0.0" }
                },
                "node_modules/nested": {
                    "version": "3.1.0",
                    "integrity": "sha512-bbb"
                },
                "node_modules/bar": {
                    "version": "2.5.0",
                    "integrity": "sha512-ccc",
                    "dev": true
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    assert_eq!(graph.packages.len(), 3);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("nested@3.1.0"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    // `LockedPackage.dependencies` values are dep_path *tails* (the
    // substring after `<name>@`), not full dep_paths — matches the
    // pnpm parser and the linker's sibling-symlink builder.
    assert_eq!(
        foo.dependencies.get("nested").map(String::as_str),
        Some("3.1.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    let foo_direct = root.iter().find(|d| d.name == "foo").unwrap();
    assert_eq!(foo_direct.dep_type, DepType::Production);
    assert_eq!(foo_direct.specifier.as_deref(), Some("^1.0.0"));
    let bar_direct = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar_direct.dep_type, DepType::Dev);
    assert_eq!(bar_direct.specifier.as_deref(), Some("^2.0.0"));
}

#[test]
fn test_parse_root_specifiers_drive_drift() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "@raycast/api": "^1.104.13" }
                },
                "node_modules/@raycast/api": {
                    "version": "1.104.20",
                    "integrity": "sha512-aaa"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = &graph.importers["."];
    let raycast = root.iter().find(|d| d.name == "@raycast/api").unwrap();
    assert_eq!(raycast.specifier.as_deref(), Some("^1.104.13"));

    let manifest = aube_manifest::PackageJson {
        dependencies: [("@vicinae/api".to_string(), "^0.21.5".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    assert_eq!(
        graph.check_drift_for_kind(
            &manifest,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            LockfileKind::Npm,
        ),
        DriftStatus::Stale {
            reason: "manifest adds @vicinae/api@^0.21.5".to_string()
        }
    );
}

#[test]
fn test_parse_git_resolved_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let content = format!(
        r#"{{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 2,
            "packages": {{
                "": {{
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {{ "git-only": "github:owner/repo#{sha}" }}
                }},
                "node_modules/git-only": {{
                    "version": "1.2.3",
                    "resolved": "git+ssh://git@github.com/owner/repo.git#{sha}",
                    "integrity": "sha512-aaa"
                }}
            }}
        }}"#
    );
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = &graph.importers["."];
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "git-only");
    assert!(!graph.packages.contains_key("git-only@1.2.3"));

    let pkg = &graph.packages[&root[0].dep_path];
    assert_eq!(pkg.name, "git-only");
    assert_eq!(pkg.version, "1.2.3");
    assert_eq!(pkg.integrity.as_deref(), Some("sha512-aaa"));
    assert!(pkg.tarball_url.is_none());

    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert_eq!(git.url, "ssh://git@github.com/owner/repo.git");
    assert_eq!(git.committish.as_deref(), Some(sha));
    assert_eq!(git.resolved, sha);
}

#[test]
fn test_unpinned_git_resolved_url_is_not_locked_git_source() {
    assert!(local_git_source_from_resolved("git+https://github.com/owner/repo.git").is_none());
}

#[test]
fn test_write_preserves_git_resolved_url() {
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let mut graph = LockfileGraph::default();
    let local = LocalSource::Git(GitSource {
        url: "ssh://git@github.com/owner/repo.git".to_string(),
        committish: Some(sha.to_string()),
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = local.dep_path("git-only");
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "git-only".to_string(),
            version: "1.2.3".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "git-only".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some(format!("github:owner/repo#{sha}")),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("git-only".to_string(), format!("github:owner/repo#{sha}"))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains(&format!(
            "\"resolved\": \"git+ssh://git@github.com/owner/repo.git#{sha}\""
        )),
        "expected git resolved URL emitted; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages[&graph.importers["."][0].dep_path];
    assert!(matches!(pkg.local_source, Some(LocalSource::Git(_))));
}

#[test]
fn test_write_emits_file_directory_dep_as_link_and_target() {
    let local = LocalSource::Directory(PathBuf::from("vendor/local-dir"));
    let dep_path = local.dep_path("local-dir");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "local-dir".to_string(),
            version: "1.0.0".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "local-dir".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("file:vendor/local-dir".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("local-dir".to_string(), "file:vendor/local-dir".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    let lock: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        lock["packages"]["node_modules/local-dir"],
        serde_json::json!({"resolved": "vendor/local-dir", "link": true})
    );
    assert_eq!(
        lock["packages"]["vendor/local-dir"],
        serde_json::json!({"name": "local-dir", "version": "1.0.0"})
    );
    parse(out.path()).unwrap();
}

#[test]
fn test_write_nests_file_directory_children_under_target() {
    let local = LocalSource::Directory(PathBuf::from("vendor/local-dir"));
    let dir_path = local.dep_path("local-dir");
    let pkg = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        dep_path: format!("{name}@{version}"),
        dependencies: deps
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        os: ["linux".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let mut graph = LockfileGraph::default();
    for p in [pkg("foo", "1.0.0", &[]), pkg("foo", "2.0.0", &[])] {
        graph.packages.insert(p.dep_path.clone(), p);
    }
    graph.packages.insert(
        dir_path.clone(),
        LockedPackage {
            dep_path: dir_path.clone(),
            local_source: Some(local),
            ..pkg("local-dir", "1.0.0", &[("foo", "2.0.0")])
        },
    );
    let direct = |name: &str, dep_path: &str| DirectDep {
        name: name.to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: None,
    };
    graph.importers.insert(
        ".".to_string(),
        vec![direct("foo", "foo@1.0.0"), direct("local-dir", &dir_path)],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(lock["packages"]["vendor/local-dir"]["os"][0], "linux");
    assert_eq!(
        lock["packages"]["vendor/local-dir/node_modules/foo"]["version"],
        "2.0.0"
    );
    assert!(lock["packages"]["node_modules/local-dir/node_modules/foo"].is_null());

    let reparsed = parse(out.path()).unwrap();
    let local_pkg = reparsed
        .packages
        .values()
        .find(|p| p.name == "local-dir")
        .unwrap();
    assert_eq!(local_pkg.dependencies["foo"], "2.0.0");
}

#[test]
fn test_parse_file_resolved_without_link() {
    // npm writes `resolved: "file:..."` without `link: true` for
    // local tarball deps (`npm install file:../foo-1.0.0.tgz`) and
    // for some directory deps. Both shapes must surface as a
    // LocalSource so the resolver dispatches the local-source
    // branch and doesn't fall through to a registry fetch.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": {
                        "tar-dep": "file:../utils/tar-dep-1.0.0.tgz",
                        "dir-dep": "file:../utils"
                    }
                },
                "node_modules/tar-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils/tar-dep-1.0.0.tgz",
                    "integrity": "sha512-aaa"
                },
                "node_modules/dir-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    let tar_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "tar-dep")
        .expect("tar-dep entry");
    assert!(
        matches!(&tar_pkg.local_source, Some(LocalSource::Tarball(p)) if p == Path::new("../utils/tar-dep-1.0.0.tgz")),
        "expected Tarball source, got {:?}",
        tar_pkg.local_source,
    );
    assert!(
        tar_pkg.dep_path.starts_with("tar-dep@file+"),
        "tarball dep_path should be local-source-keyed, got {}",
        tar_pkg.dep_path,
    );

    let dir_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "dir-dep")
        .expect("dir-dep entry");
    assert!(
        matches!(&dir_pkg.local_source, Some(LocalSource::Directory(p)) if p == Path::new("../utils")),
        "expected Directory source, got {:?}",
        dir_pkg.local_source,
    );
    assert!(
        dir_pkg.dep_path.starts_with("dir-dep@file+"),
        "directory dep_path should be local-source-keyed, got {}",
        dir_pkg.dep_path,
    );

    let root = graph.importers.get(".").unwrap();
    let tar_direct = root.iter().find(|d| d.name == "tar-dep").unwrap();
    assert_eq!(tar_direct.dep_path, tar_pkg.dep_path);
    let dir_direct = root.iter().find(|d| d.name == "dir-dep").unwrap();
    assert_eq!(dir_direct.dep_path, dir_pkg.dep_path);
}

#[test]
fn test_parse_remote_tarball_from_declared_url() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let url = "https://cdn.sheetjs.com/xlsx-0.20.3/xlsx-0.20.3.tgz";
    let content = format!(
        r#"{{
            "lockfileVersion": 3,
            "packages": {{
                "": {{
                    "dependencies": {{
                        "xlsx": "{url}",
                        "semver": "^7.7.0"
                    }}
                }},
                "node_modules/xlsx": {{
                    "version": "0.20.3",
                    "resolved": "{url}",
                    "integrity": "sha512-sheetjs"
                }},
                "node_modules/semver": {{
                    "version": "7.7.2",
                    "resolved": "https://registry.npmjs.org/semver/-/semver-7.7.2.tgz",
                    "integrity": "sha512-semver"
                }}
            }}
        }}"#
    );
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let direct = graph.importers["."]
        .iter()
        .find(|dep| dep.name == "xlsx")
        .unwrap();
    let pkg = &graph.packages[&direct.dep_path];

    assert_eq!(pkg.version, "0.20.3");
    assert!(pkg.tarball_url.is_none());
    let Some(LocalSource::RemoteTarball(source)) = &pkg.local_source else {
        panic!("expected remote tarball source, got {:?}", pkg.local_source);
    };
    assert_eq!(source.url, url);
    assert_eq!(source.integrity, "sha512-sheetjs");
    let registry_pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "semver")
        .unwrap();
    assert!(registry_pkg.local_source.is_none());

    let manifest = aube_manifest::PackageJson {
        dependencies: [
            ("xlsx".to_string(), url.to_string()),
            ("semver".to_string(), "^7.7.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();
    let reparsed_direct = reparsed.importers["."]
        .iter()
        .find(|dep| dep.name == "xlsx")
        .unwrap();
    let reparsed_pkg = &reparsed.packages[&reparsed_direct.dep_path];
    let Some(LocalSource::RemoteTarball(reparsed_source)) = &reparsed_pkg.local_source else {
        panic!(
            "expected remote tarball source, got {:?}",
            reparsed_pkg.local_source
        );
    };
    assert_eq!(reparsed_source.url, url);
    assert_eq!(reparsed_source.integrity, "sha512-sheetjs");
}

#[test]
fn test_parse_scoped_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "@scope/pkg": "^1.0.0" }
                },
                "node_modules/@scope/pkg": {
                    "version": "1.0.0",
                    "integrity": "sha512-zzz"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.0.0");
}

#[test]
fn test_parse_multi_version_nested() {
    // bar exists at two versions: 2.0.0 hoisted to root, 1.0.0 nested under foo.
    // foo's transitive dep on bar must resolve to 1.0.0, not 2.0.0.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    // Both versions of bar should be present.
    assert!(graph.packages.contains_key("bar@2.0.0"));
    assert!(graph.packages.contains_key("bar@1.0.0"));
    assert!(graph.packages.contains_key("foo@1.0.0"));

    // foo's transitive dep must point to the nested (1.0.0), not the hoisted (2.0.0).
    // Value is the dep_path tail (version) — see the `LockedPackage.dependencies` doc.
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("1.0.0")
    );

    // Root's direct bar dep points to the hoisted 2.0.0.
    let root = graph.importers.get(".").unwrap();
    let root_bar = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(root_bar.dep_path, "bar@2.0.0");
}

#[test]
fn test_write_preserves_reachable_existing_root_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "dependencies": { "app": "1.0.0" } },
            "node_modules/app": {
                "version": "1.0.0",
                "dependencies": { "a-newer": "1.0.0", "z-older": "1.0.0" }
            },
            "node_modules/a-newer": {
                "version": "1.0.0",
                "dependencies": { "shared": "^2.0.0" }
            },
            "node_modules/a-newer/node_modules/shared": { "version": "2.0.0" },
            "node_modules/shared": { "version": "1.0.0" },
            "node_modules/z-older": {
                "version": "1.0.0",
                "dependencies": { "shared": "^1.0.0" }
            }
        }
    }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let mut graph = parse(tmp.path()).unwrap();
    let manifest = aube_manifest::PackageJson {
        dependencies: [("app".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    write(tmp.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path()).unwrap()).unwrap();
    assert_eq!(json["packages"]["node_modules/shared"]["version"], "1.0.0");
    assert_eq!(
        json["packages"]["node_modules/a-newer/node_modules/shared"]["version"],
        "2.0.0"
    );

    // Once the last edge to the preferred version disappears, it must not be
    // kept merely because the previous lockfile had hoisted it.
    graph
        .packages
        .get_mut("app@1.0.0")
        .unwrap()
        .dependencies
        .remove("z-older");
    graph.packages.remove("z-older@1.0.0");
    write(tmp.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path()).unwrap()).unwrap();
    assert_eq!(json["packages"]["node_modules/shared"]["version"], "2.0.0");
    assert!(
        json["packages"]
            .get("node_modules/a-newer/node_modules/shared")
            .is_none(),
        "the stale preferred root must be replaced instead of nested"
    );
}

/// Regression: a package reachable from both a dev root and
/// an optional root (but *not* from any production root) must
/// be written with `devOptional: true`, not with both `dev: true`
/// and `optional: true`. Emitting both trips `npm install
/// --omit=dev` (and `--omit=optional`) into dropping a package
/// the other chain still needs.
#[test]
fn test_write_dev_and_optional_reachable_uses_dev_optional() {
    let mut graph = LockfileGraph::default();
    let mk = |name: &str| LockedPackage {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        integrity: Some(format!("sha512-{name}")),
        dep_path: format!("{name}@1.0.0"),
        dependencies: [("shared".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    graph
        .packages
        .insert("dev-root@1.0.0".to_string(), mk("dev-root"));
    graph
        .packages
        .insert("opt-root@1.0.0".to_string(), mk("opt-root"));
    graph.packages.insert(
        "shared@1.0.0".to_string(),
        LockedPackage {
            name: "shared".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-shared".to_string()),
            dep_path: "shared@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "dev-root".to_string(),
                dep_path: "dev-root@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
            DirectDep {
                name: "opt-root".to_string(),
                dep_path: "opt-root@1.0.0".to_string(),
                dep_type: DepType::Optional,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dev_dependencies: [("dev-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: [("opt-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let shared = &json["packages"]["node_modules/shared"];
    assert_eq!(shared["devOptional"], true, "expected devOptional flag");
    assert!(
        shared.get("dev").is_none(),
        "must not emit dev: true alongside devOptional",
    );
    assert!(
        shared.get("optional").is_none(),
        "must not emit optional: true alongside devOptional",
    );

    // Roots themselves retain their specific flag.
    assert_eq!(json["packages"]["node_modules/dev-root"]["dev"], true);
    assert_eq!(json["packages"]["node_modules/opt-root"]["optional"], true);
}

/// Regression: an `optionalDependencies` edge out of a production
/// package (esbuild -> @esbuild/*) must write `optional: true` on the
/// child, and `hasInstallScript` must survive a parse/write cycle.
#[test]
fn test_write_optional_edge_and_install_script_flags_round_trip() {
    let lock = r#"{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {
    "": {"name": "t", "version": "1.0.0", "dependencies": {"esbuild": "0.25.10"}},
    "node_modules/@esbuild/aix-ppc64": {
      "version": "0.25.10", "resolved": "https://r/a.tgz", "integrity": "sha512-a",
      "cpu": ["ppc64"], "optional": true, "os": ["aix"]
    },
    "node_modules/esbuild": {
      "version": "0.25.10", "resolved": "https://r/e.tgz", "integrity": "sha512-e",
      "hasInstallScript": true,
      "optionalDependencies": {"@esbuild/aix-ppc64": "0.25.10"}
    }
  }
}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("package-lock.json");
    std::fs::write(&path, lock).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("t".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("esbuild".to_string(), "0.25.10".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let graph = parse(&path).unwrap();
    write(&path, &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let child = &json["packages"]["node_modules/@esbuild/aix-ppc64"];
    assert_eq!(child["optional"], true);
    let parent = &json["packages"]["node_modules/esbuild"];
    assert_eq!(parent["hasInstallScript"], true);
    assert!(parent.get("optional").is_none());

    let dev_manifest = aube_manifest::PackageJson {
        dependencies: Default::default(),
        dev_dependencies: manifest.dependencies.clone(),
        ..manifest
    };
    let mut dev_graph = graph;
    for dep in dev_graph.importers.get_mut(".").unwrap() {
        dep.dep_type = DepType::Dev;
    }
    write(&path, &dev_graph, &dev_manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let child = &json["packages"]["node_modules/@esbuild/aix-ppc64"];
    assert_eq!(child["dev"], true);
    assert_eq!(child["optional"], true);
    assert!(child.get("devOptional").is_none());
}

/// An optional child shared by a production and a dev dependency is
/// kept by the production chain, so it must be `optional` only, never
/// `dev`.
#[test]
fn test_write_optional_child_of_prod_and_dev_is_not_dev() {
    let mut graph = LockfileGraph::default();
    let mk = |name: &str, opt: bool| LockedPackage {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        dep_path: format!("{name}@1.0.0"),
        dependencies: if opt {
            [("child".to_string(), "1.0.0".to_string())].into()
        } else {
            Default::default()
        },
        optional_dependencies: if opt {
            [("child".to_string(), "1.0.0".to_string())].into()
        } else {
            Default::default()
        },
        ..Default::default()
    };
    for (n, o) in [("prod", true), ("dev", true), ("child", false)] {
        graph.packages.insert(format!("{n}@1.0.0"), mk(n, o));
    }
    let root = |n: &str, t| DirectDep {
        name: n.to_string(),
        dep_path: format!("{n}@1.0.0"),
        dep_type: t,
        specifier: None,
    };
    graph.importers.insert(
        ".".to_string(),
        vec![root("prod", DepType::Production), root("dev", DepType::Dev)],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("t".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let child = &json["packages"]["node_modules/child"];
    assert_eq!(child["optional"], true);
    assert!(child.get("dev").is_none());
    assert!(child.get("devOptional").is_none());
}

/// Regression: the npm writer must drop `dependencies` entries
/// whose target isn't in the canonical map. Platform-filtered
/// optionals and `ignoredOptionalDependencies` leave the parent's
/// declared `dependencies` map pointing at packages the resolver
/// already removed; emitting them anyway produces a lockfile
/// where `npm ci` sees a reference with no matching `packages`
/// entry and refuses to install. Must match the bun/yarn
/// writers, which already filter this way.
#[test]
fn test_write_filters_missing_canonical_deps() {
    let mut graph = LockfileGraph::default();
    // Root has one real package, `foo`, which declares a dep on
    // `ghost@1.0.0` — but `ghost` was filtered out of the graph
    // (e.g. a platform-gated optional). The canonical map won't
    // contain it.
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            dependencies: [("ghost".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    // Parse the raw JSON directly — the aube reparser tolerates
    // dangling references so we assert on the serialized shape.
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let foo_entry = &json["packages"]["node_modules/foo"];
    assert!(
        foo_entry
            .get("dependencies")
            .and_then(|d| d.get("ghost"))
            .is_none(),
        "writer emitted a ghost dep that has no packages entry: {foo_entry}",
    );
    // And there should be no node_modules/ghost entry at all.
    assert!(
        json["packages"].get("node_modules/ghost").is_none(),
        "writer hallucinated a ghost entry",
    );
}

/// Regression for the shadow-nesting bug: if an intermediate
/// ancestor carries the *wrong* version of a dep, Node's
/// runtime walk stops there and never reaches a correct entry
/// at root. The writer must nest a fresh entry inside the
/// current parent's own `node_modules` instead of assuming
/// hoisting is fine just because root happens to have the
/// right version.
///
/// Shape:
///   root → foo → baz, baz depends on bar@2.0.0
///   foo already pulled in bar@1.0.0 for a sibling, so bar@1.0.0
///     lives at node_modules/foo/node_modules/bar
///   root has bar@2.0.0 at node_modules/bar
///
///   When we walk baz's deps and get to bar@2.0.0, the nearest
///   ancestor hit is bar@1.0.0 (shadowing), not root. We must
///   place a fresh entry at
///   `node_modules/foo/node_modules/baz/node_modules/bar` so
///   Node resolves the right version.
#[test]
fn test_nested_shadow_forces_nested_placement() {
    // Build a graph by hand to control the dep order deterministically.
    let mut graph = LockfileGraph::default();
    let mk = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        integrity: Some(format!("sha512-{name}-{version}")),
        dep_path: format!("{name}@{version}"),
        dependencies: deps
            .iter()
            .map(|(n, v)| (n.to_string(), (*v).to_string()))
            .collect(),
        ..Default::default()
    };
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        mk(
            "foo",
            "1.0.0",
            &[
                // foo pulls in bar@1.0.0 and baz@1.0.0 as siblings.
                ("bar", "1.0.0"),
                ("baz", "1.0.0"),
            ],
        ),
    );
    graph.packages.insert(
        "baz@1.0.0".to_string(),
        // baz wants bar@2.0.0, which matches the root version.
        mk("baz", "1.0.0", &[("bar", "2.0.0")]),
    );
    graph
        .packages
        .insert("bar@1.0.0".to_string(), mk("bar", "1.0.0", &[]));
    graph
        .packages
        .insert("bar@2.0.0".to_string(), mk("bar", "2.0.0", &[]));
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // baz's transitive dep must resolve to bar@2.0.0, not the
    // shadowing bar@1.0.0 under foo. Value is the dep_path tail
    // (version) so the linker can recombine it with the dep name.
    let baz = &reparsed.packages["baz@1.0.0"];
    assert_eq!(
        baz.dependencies.get("bar").map(String::as_str),
        Some("2.0.0"),
        "baz's bar dep was shadowed by foo/bar@1.0.0 — shadow-nest fix regressed",
    );
}

#[test]
fn test_parse_npm_preserves_platform_optional_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "platform-optional-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "platform-optional-root",
                    "version": "1.0.0",
                    "dependencies": { "host": "file:host" }
                },
                "node_modules/host": {
                    "resolved": "host",
                    "link": true
                },
                "host": {
                    "name": "host",
                    "version": "1.0.0",
                    "optionalDependencies": { "native-win": "1.0.0" }
                },
                "node_modules/native-win": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/native-win/-/native-win-1.0.0.tgz",
                    "integrity": "sha512-native",
                    "optional": true,
                    "os": ["win32"],
                    "cpu": ["x64"],
                    "libc": ["glibc"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let host_dep_path = &graph.importers["."][0].dep_path;
    let host = &graph.packages[host_dep_path];
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );

    let native = &graph.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// npm sometimes emits `os` / `cpu` / `libc` as scalar strings instead
/// of arrays (e.g. `sass-embedded-linux-arm@1.99.0` ships
/// `"libc": "glibc"`). Verbatim-roundtripped into package-lock.json,
/// the field stays scalar — accept both shapes the same way the
/// pnpm + bun parsers already do.
#[test]
fn parse_npm_package_platform_fields_accept_scalar_strings() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "scalar-platform-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "scalar-platform-root",
                    "version": "1.0.0",
                    "dependencies": { "sass-embedded-linux-arm": "1.99.0" }
                },
                "node_modules/sass-embedded-linux-arm": {
                    "version": "1.99.0",
                    "resolved": "https://registry.npmjs.org/sass-embedded-linux-arm/-/sass-embedded-linux-arm-1.99.0.tgz",
                    "integrity": "sha512-native",
                    "cpu": "arm",
                    "os": "linux",
                    "libc": "glibc"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["sass-embedded-linux-arm@1.99.0"];
    assert_eq!(
        pkg.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["linux"]
    );
    assert_eq!(
        pkg.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["arm"]
    );
    assert_eq!(
        pkg.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

#[test]
fn test_write_npm_preserves_platform_optional_metadata() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "host@1.0.0".to_string(),
        LockedPackage {
            name: "host".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-host".to_string()),
            dep_path: "host@1.0.0".to_string(),
            dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            declared_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "native-win@1.0.0".to_string(),
        LockedPackage {
            name: "native-win".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-native".to_string()),
            dep_path: "native-win@1.0.0".to_string(),
            os: vec!["win32".to_string()].into(),
            cpu: vec!["x64".to_string()].into(),
            libc: vec!["glibc".to_string()].into(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "host".to_string(),
            dep_path: "host@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".to_string()),
        }],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("platform-optional-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("host".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        extra: [(
            "packageManager".to_string(),
            serde_json::json!("npm@11.11.0"),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let host = &json["packages"]["node_modules/host"];
    assert_eq!(host["optionalDependencies"]["native-win"], "1.0.0");
    assert!(
        host.get("dependencies")
            .and_then(|deps| deps.get("native-win"))
            .is_none(),
        "optional child must not be duplicated as a required dependency: {host}",
    );

    let native = &json["packages"]["node_modules/native-win"];
    assert_eq!(native["os"], serde_json::json!(["win32"]));
    assert_eq!(native["cpu"], serde_json::json!(["x64"]));
    assert_eq!(native["libc"], serde_json::json!(["glibc"]));

    let reparsed = parse(out.path()).unwrap();
    let host = &reparsed.packages["host@1.0.0"];
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    let native = &reparsed.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// Regression: `canonical_key_from_dep_path` must strip the
/// peer-context suffix *before* splitting on `@`. A naive
/// `rfind('@')` lands inside the peer suffix and returns the
/// input unchanged, which silently drops every peer-contextualized
/// root dep from the written lockfile. The capped hash form
/// `(<short-hash>)` shares the same canonical identity and is
/// stripped the same way — it also begins at the first `(`.
#[test]
fn test_canonical_key_strips_peer_suffix() {
    assert_eq!(canonical_key_from_dep_path("foo@1.0.0"), "foo@1.0.0");
    assert_eq!(
        canonical_key_from_dep_path("styled-components@6.1.0(react@18.2.0)"),
        "styled-components@6.1.0"
    );
    assert_eq!(
        canonical_key_from_dep_path("@scope/pkg@2.0.0(peer@1.0.0)"),
        "@scope/pkg@2.0.0"
    );
    // Capped suffix: a single parenthesized short hash (pnpm's
    // `createPeerDepGraphHash`), stripped like any other `(…)` tail.
    let hashed = "expo-router@4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)";
    assert_eq!(canonical_key_from_dep_path(hashed), "expo-router@4.0.22");
    assert_eq!(
        child_canonical_key("expo-router", "4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)"),
        "expo-router@4.0.22"
    );
    assert_eq!(
        dep_value_as_version(
            "expo-router",
            "expo-router@4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)"
        ),
        "4.0.22"
    );
}

fn test_manifest() -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("foo".to_string(), "^1.0.0".to_string()),
            ("bar".to_string(), "^2.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

/// Parse a fixture, write it back, re-parse: the resulting graph
/// must have the same packages, direct deps, and integrity hashes.
/// Catches silent data loss in the hoist/nest walk.
#[test]
fn test_write_roundtrip_multi_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let manifest = test_manifest();

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // Both versions of bar survived the round-trip.
    assert!(reparsed.packages.contains_key("bar@1.0.0"));
    assert!(reparsed.packages.contains_key("bar@2.0.0"));
    assert!(reparsed.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        reparsed.packages["bar@2.0.0"].integrity.as_deref(),
        Some("sha512-top-bar")
    );
    assert_eq!(
        reparsed.packages["bar@1.0.0"].integrity.as_deref(),
        Some("sha512-nested-bar")
    );
    // foo's nested bar dep still resolves to 1.0.0, not the
    // hoisted 2.0.0. If the writer failed to nest, reparse would
    // snap this to bar@2.0.0. Value is the dep_path tail.
    assert_eq!(
        reparsed.packages["foo@1.0.0"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("1.0.0")
    );
}

/// Dev-only and optional-only packages get the right flags after
/// round-trip so `npm install --omit=dev` on the written file
/// does the right thing.
#[test]
fn test_write_dev_optional_flags() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "devdep@1.0.0".to_string(),
        LockedPackage {
            name: "devdep".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-dev".to_string()),
            dep_path: "devdep@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "devdep".to_string(),
                dep_path: "devdep@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        dev_dependencies: [("devdep".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let packages = &json["packages"];
    assert_eq!(packages["node_modules/devdep"]["dev"], true);
    // Prod dep should have no dev field (skipped when false).
    assert!(packages["node_modules/foo"].get("dev").is_none());
}

#[test]
fn test_reject_v1() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "lockfileVersion": 1,
            "dependencies": {}
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let err = parse(tmp.path()).unwrap_err();
    assert!(matches!(err, Error::Parse(_, msg) if msg.contains("lockfileVersion 1")));
}

/// Pre-npm-2.x packages (e.g. `ansi-html-community@0.0.8`) ship
/// `"engines": ["node >= 0.8.0"]` as an array; npm preserves that
/// shape verbatim in v2/v3 lockfiles. Without tolerant parsing, a
/// single such entry blows up the whole `aube ci`. Normalize to an
/// empty map (matches what modern npm does for engine-strict on
/// the array shape) so the install proceeds.
#[test]
fn test_parse_legacy_array_engines() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "ansi-html-community": "0.0.8" }
                },
                "node_modules/ansi-html-community": {
                    "version": "0.0.8",
                    "integrity": "sha512-aaa",
                    "engines": ["node >= 0.8.0"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["ansi-html-community@0.0.8"];
    // Array shape gets normalized to an empty map — same as the
    // manifest parser, and same as what modern npm honors for the
    // engine-strict check on the array form.
    assert!(pkg.engines.is_empty());
}

/// npm writes `"h3-v2": "npm:h3@..."` aliases as a packages entry
/// at `node_modules/h3-v2` with `name: "h3"` and the real registry
/// `resolved:` URL. Aube keys the graph on the *alias* (so
/// `node_modules/h3-v2` ends up at `.aube/h3-v2@.../node_modules/h3-v2`)
/// but remembers the real package name in `alias_of` so fetches
/// and store-index lookups use the URL that actually exists.
#[test]
fn test_parse_npm_alias_dependency() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "h3-v2": "npm:h3@2.0.1-rc.20" }
                },
                "node_modules/h3-v2": {
                    "name": "h3",
                    "version": "2.0.1-rc.20",
                    "resolved": "https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz",
                    "integrity": "sha512-aliased"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(graph.packages.len(), 1);
    // Graph key and LockedPackage.name both carry the alias —
    // that's what consumers (and the linker's folder-name logic)
    // refer to when they say "h3-v2".
    let pkg = graph
        .packages
        .get("h3-v2@2.0.1-rc.20")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(pkg.name, "h3-v2");
    assert_eq!(pkg.version, "2.0.1-rc.20");
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
    // `resolved:` round-trips into `tarball_url` so the fetcher
    // skips re-deriving from the alias-qualified name (which
    // would 404 the registry).
    assert_eq!(
        pkg.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "h3-v2");
    assert_eq!(root[0].dep_path, "h3-v2@2.0.1-rc.20");
}

/// Non-aliased entries (the common case) leave `alias_of` unset
/// and `registry_name()` degenerates to `name`. Regression guard
/// against over-aggressive alias detection that would flag every
/// entry carrying an explicit `name:` field (npm sometimes emits
/// one for non-aliased roots too).
#[test]
fn test_parse_non_alias_preserves_empty_alias_of() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" }
                },
                "node_modules/foo": {
                    "name": "foo",
                    "version": "1.2.3",
                    "integrity": "sha512-foo"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["foo@1.2.3"];
    assert_eq!(pkg.name, "foo");
    assert!(pkg.alias_of.is_none());
    assert_eq!(pkg.registry_name(), "foo");
    assert!(pkg.tarball_url.is_none());
}

/// Round-trip: writer must emit `name:` and `resolved:` for the
/// aliased entry so a subsequent `parse()` still recognizes it as
/// an alias. Without both fields the re-parser would see
/// `node_modules/h3-v2` with no `name:` and treat it as a plain
/// package called `h3-v2` — which doesn't exist on the registry.
#[test]
fn test_write_roundtrip_npm_alias() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "h3-v2@2.0.1-rc.20".to_string(),
        LockedPackage {
            name: "h3-v2".to_string(),
            version: "2.0.1-rc.20".to_string(),
            integrity: Some("sha512-aliased".to_string()),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            alias_of: Some("h3".to_string()),
            tarball_url: Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz".to_string()),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "h3-v2".to_string(),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            dep_type: DepType::Production,
            specifier: Some("npm:h3@2.0.1-rc.20".to_string()),
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"name\": \"h3\""),
        "expected `name: h3` emitted for aliased entry; got:\n{body}"
    );
    assert!(
        body.contains("\"resolved\": \"https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz\""),
        "expected `resolved:` URL emitted for aliased entry; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages["h3-v2@2.0.1-rc.20"];
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
}

/// npm v7+ writes `peerDependencies` / `peerDependenciesMeta` onto
/// every package entry. The parser must populate the matching
/// `LockedPackage` fields so the resolver's `apply_peer_contexts`
/// pass (run on npm-lockfile installs to wire peer siblings in the
/// isolated virtual store) actually has peer info to work with.
/// Before this parser change, peer-dependent packages like
/// `@tanstack/devtools-vite` would install without a sibling
/// `vite` link and die at runtime.
#[test]
fn test_parse_peer_dependencies() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "peer-test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "peer-test",
                    "version": "1.0.0",
                    "dependencies": { "devtools-vite": "0.6.0", "vite": "8.0.0" }
                },
                "node_modules/devtools-vite": {
                    "version": "0.6.0",
                    "integrity": "sha512-a",
                    "peerDependencies": {
                        "vite": "^6.0.0 || ^7.0.0 || ^8.0.0"
                    },
                    "peerDependenciesMeta": {
                        "vite": { "optional": false }
                    }
                },
                "node_modules/vite": {
                    "version": "8.0.0",
                    "integrity": "sha512-b"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let devtools = &graph.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(false)
    );
}

/// Packages without peer fields keep both maps empty — guard
/// against accidental defaulting to `optional: true` or spurious
/// keys showing up in the LockedPackage from serde leak paths.
#[test]
fn test_parse_no_peer_fields_stays_empty() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "no-peers",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "no-peers", "version": "1.0.0", "dependencies": { "foo": "1.0.0" } },
                "node_modules/foo": { "version": "1.0.0", "integrity": "sha512-x" }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert!(foo.peer_dependencies.is_empty());
    assert!(foo.peer_dependencies_meta.is_empty());
}

/// Writer round-trips `peerDependencies` so a second `parse()` on
/// the rewritten lockfile still feeds the peer-context pass. The
/// install path writes out the lockfile after every install; if
/// peers vanished on the first write-back, the *next* install
/// would ship without peer siblings again.
#[test]
fn test_write_roundtrip_peer_dependencies() {
    let mut graph = LockfileGraph::default();
    let mut peer_deps = BTreeMap::new();
    peer_deps.insert("vite".to_string(), "^6.0.0 || ^7.0.0 || ^8.0.0".to_string());
    // Include an `optional: true` entry so the round-trip covers
    // `peerDependenciesMeta` — without it, the writer's meta
    // block isn't exercised and the round-trip would silently
    // re-flag the peer as required on every subsequent install
    // (see `hoist_auto_installed_peers` + `detect_unmet_peers`,
    // which key off `optional`).
    let mut peer_deps_meta = BTreeMap::new();
    peer_deps_meta.insert("vite".to_string(), crate::PeerDepMeta { optional: true });
    graph.packages.insert(
        "devtools-vite@0.6.0".to_string(),
        LockedPackage {
            name: "devtools-vite".to_string(),
            version: "0.6.0".to_string(),
            integrity: Some("sha512-a".to_string()),
            dep_path: "devtools-vite@0.6.0".to_string(),
            peer_dependencies: peer_deps,
            peer_dependencies_meta: peer_deps_meta,
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@8.0.0".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "8.0.0".to_string(),
            integrity: Some("sha512-b".to_string()),
            dep_path: "vite@8.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "devtools-vite".to_string(),
                dep_path: "devtools-vite@0.6.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@8.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"peerDependencies\""),
        "expected peerDependencies block to round-trip; got:\n{body}"
    );
    assert!(
        body.contains("\"peerDependenciesMeta\""),
        "expected peerDependenciesMeta block to round-trip; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let devtools = &reparsed.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(true),
        "peerDependenciesMeta.optional must survive write → parse round-trip"
    );
}

#[test]
fn test_parse_npm_workspace_importers() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["web"]
                },
                "node_modules/mise-versions-web": {
                    "resolved": "web",
                    "link": true
                },
                "web": {
                    "name": "mise-versions-web",
                    "version": "0.0.1",
                    "dependencies": { "astro": "^6.0.0" },
                    "devDependencies": { "vite": "^7.3.2" }
                },
                "web/node_modules/astro": {
                    "version": "6.2.1",
                    "integrity": "sha512-astro"
                },
                "web/node_modules/vite": {
                    "version": "7.3.2",
                    "integrity": "sha512-vite"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "mise-versions-web");
    assert!(matches!(
        graph.packages[&root[0].dep_path].local_source,
        Some(LocalSource::Link(_))
    ));

    let web = graph.importers.get("web").expect("web importer");
    assert_eq!(web.len(), 2);
    assert!(web.iter().any(|dep| {
        dep.name == "astro"
            && dep.dep_type == DepType::Production
            && dep.specifier.as_deref() == Some("^6.0.0")
    }));
    assert!(web.iter().any(|dep| {
        dep.name == "vite"
            && dep.dep_type == DepType::Dev
            && dep.specifier.as_deref() == Some("^7.3.2")
    }));
}

#[test]
fn test_write_npm_workspace_importers() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("mise-versions-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "mise-versions-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@7.3.2".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "7.3.2".to_string(),
            integrity: Some("sha512-vite".to_string()),
            dep_path: "vite@7.3.2".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mise-versions-web".to_string(),
            dep_path: web_dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@7.3.2".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^7.3.2".to_string()),
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["link"],
        true
    );
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["resolved"],
        "web"
    );
    assert_eq!(json["packages"]["web"]["dependencies"]["astro"], "^6.0.0");
    assert_eq!(json["packages"]["web"]["devDependencies"]["vite"], "^7.3.2");
    assert_eq!(
        json["packages"]["web/node_modules/astro"]["version"],
        "6.2.1"
    );
    assert_eq!(
        json["packages"]["web/node_modules/vite"]["version"],
        "7.3.2"
    );

    let reparsed = parse(out.path()).unwrap();
    assert!(reparsed.importers.contains_key("web"));
}

/// A fresh resolve of an npm workspace whose root depends on no member
/// has no `link:` package for them. The writer still has to emit each
/// member, reading its name and version from its own package.json.
#[test]
fn test_write_npm_workspace_members_nothing_depends_on() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("packages/a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("packages/unnamed")).unwrap();
    std::fs::write(
        tmp.path().join("packages/a/package.json"),
        r#"{"name":"a","version":"1.0.0","peerDependencies":{"p":"^1"}}"#,
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("packages/unnamed/package.json"),
        r#"{"version":"2.0.0"}"#,
    )
    .unwrap();
    let mut graph = LockfileGraph::default();
    graph.importers.insert(".".to_string(), Vec::new());
    graph.importers.insert("packages/a".to_string(), Vec::new());
    graph
        .importers
        .insert("packages/unnamed".to_string(), Vec::new());
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        workspaces: Some(aube_manifest::Workspaces::Array(vec![
            "packages/*".to_string(),
        ])),
        ..Default::default()
    };
    let path = tmp.path().join("package-lock.json");
    write(&path, &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let packages = &json["packages"];
    assert_eq!(
        packages[""]["workspaces"],
        serde_json::json!(["packages/*"])
    );
    assert_eq!(packages["node_modules/a"]["resolved"], "packages/a");
    assert_eq!(packages["node_modules/a"]["link"], true);
    assert_eq!(packages["packages/a"]["version"], "1.0.0");
    assert_eq!(packages["packages/a"]["peerDependencies"]["p"], "^1");
    assert_eq!(
        packages["node_modules/unnamed"]["resolved"],
        "packages/unnamed"
    );

    let reparsed = parse(&path).unwrap();
    assert!(reparsed.importers.contains_key("packages/a"));
    assert!(reparsed.importers.contains_key("packages/unnamed"));
}

/// When the root tree already hoists a package to
/// `node_modules/<name>`, the workspace tree must NOT emit a
/// redundant `<workspace>/node_modules/<name>` for the same
/// version — Node's upward `node_modules` walk resolves the root
/// copy. Real `npm install` omits the redundant entry, and
/// emitting it produces a diff on every round-trip.
#[test]
fn test_write_npm_workspace_skips_root_hoisted_dups() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("workspace-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "workspace-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "workspace-web".to_string(),
                dep_path: web_dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![DirectDep {
            name: "astro".to_string(),
            dep_path: "astro@6.2.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^6.0.0".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("astro".to_string(), "^6.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(json["packages"]["node_modules/astro"]["version"], "6.2.1");
    assert!(
        json["packages"].get("web/node_modules/astro").is_none(),
        "redundant workspace-nested astro should not be emitted"
    );
}

/// Byte-parity with a real `npm install`-generated lockfile. The
/// fixture at `tests/fixtures/npm-native.json` was produced by
/// `npm install` (v11) against a `{ chalk, picocolors, semver }`
/// manifest. A parse → write round-trip must reproduce the exact
/// bytes. Covers `resolved:` on every entry, `license:` /
/// `engines:` / `bin:` / `funding:` field preservation, and the
/// sibling declared-range preservation that rides on
/// `declared_dependencies`.
#[test]
fn test_write_byte_identical_to_native_npm() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/npm-native.json");
    // Same LF normalization as the pnpm / bun byte-parity tests —
    // Windows' `core.autocrlf=true` rewrites the checked-out
    // fixture to CRLF even with `.gitattributes eol=lf`.
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
            "npm writer drifted from native npm output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
        );
    }
}

#[test]
fn test_parse_workspace_links() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "dependencies": { "@scope/app": "file:packages/app" }
                },
                "node_modules/@scope/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "packages/app": {
                    "name": "@scope/app",
                    "version": "0.68.1",
                    "dependencies": {
                        "chalk": "^5.4.1"
                    }
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let dep_path = LocalSource::Link(PathBuf::from("packages/app")).dep_path("@scope/app");

    let importer = &graph.importers["."];
    assert_eq!(importer.len(), 1);
    assert_eq!(importer[0].name, "@scope/app");
    assert_eq!(importer[0].dep_path, dep_path);
    assert!(matches!(importer[0].dep_type, DepType::Production));
    assert_eq!(importer[0].specifier.as_deref(), Some("file:packages/app"));

    let app = &graph.packages[&importer[0].dep_path];
    assert_eq!(app.version, "0.68.1");
    assert_eq!(
        app.local_source,
        Some(LocalSource::Link(PathBuf::from("packages/app")))
    );
    assert_eq!(
        app.dependencies.get("chalk").map(String::as_str),
        Some("5.4.1")
    );
    assert!(!graph.packages.contains_key("@scope/app@0.68.1"));
}

/// npm workspaces that aren't listed in the root manifest's
/// `dependencies`/`devDependencies` still get a `node_modules/<name>`
/// link entry in the lockfile — npm symlinks every workspace member
/// at the workspace root regardless. The siemens/element repo
/// (https://github.com/siemens/element) hits this: its 11 workspace
/// projects under `projects/*` aren't declared as deps of the root
/// `package.json`, so the linker had nothing to link at the root and
/// `node_modules/@siemens/element-ng` (and friends) silently went
/// missing — breaking Angular CLI builds that resolve workspace
/// libraries from the repo root.
#[test]
fn test_parse_workspace_links_undeclared_in_root_deps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["projects/element-ng", "projects/charts-ng"],
                    "dependencies": { "chalk": "^5.4.1" }
                },
                "node_modules/@siemens/element-ng": {
                    "resolved": "projects/element-ng",
                    "link": true
                },
                "node_modules/@siemens/charts-ng": {
                    "resolved": "projects/charts-ng",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "projects/element-ng": {
                    "name": "@siemens/element-ng",
                    "version": "21.0.0"
                },
                "projects/charts-ng": {
                    "name": "@siemens/charts-ng",
                    "version": "21.0.0"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let importer = &graph.importers["."];

    let names: Vec<&str> = importer.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"chalk"));
    assert!(
        names.contains(&"@siemens/element-ng"),
        "workspace package `@siemens/element-ng` should be a direct dep of root \
             so the linker creates `node_modules/@siemens/element-ng`, even though \
             the root manifest doesn't list it; got importer deps {names:?}"
    );
    assert!(
        names.contains(&"@siemens/charts-ng"),
        "workspace package `@siemens/charts-ng` should be a direct dep of root; \
             got importer deps {names:?}"
    );

    // Each workspace dep_path round-trips through LocalSource::Link.
    let element_ng = importer
        .iter()
        .find(|d| d.name == "@siemens/element-ng")
        .unwrap();
    assert_eq!(
        graph.packages[&element_ng.dep_path].local_source,
        Some(LocalSource::Link(PathBuf::from("projects/element-ng")))
    );
}

/// npm copies `funding:` verbatim from each package's
/// `package.json`, so all three registry-permitted shapes (bare
/// string, `{url}` object, mixed array of either) appear in real
/// lockfiles. The pre-fix parser only accepted the object form
/// and would hard-fail on any project pulling in `htmlparser2`,
/// `@csstools/*`, etc. Aube only carries one URL per package, so
/// the contract is "first URL wins, no shape rejected".
#[test]
fn test_parse_funding_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-funding": "1.0.0",
                        "object-funding": "1.0.0",
                        "array-funding": "1.0.0",
                        "mixed-array-funding": "1.0.0",
                        "no-funding": "1.0.0"
                    }
                },
                "node_modules/string-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "funding": "https://example.com/sponsor"
                },
                "node_modules/object-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "funding": { "type": "github", "url": "https://github.com/sponsors/foo" }
                },
                "node_modules/array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "funding": [
                        { "type": "github", "url": "https://github.com/sponsors/csstools" },
                        { "type": "opencollective", "url": "https://opencollective.com/csstools" }
                    ]
                },
                "node_modules/mixed-array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "funding": [
                        "https://github.com/fb55/htmlparser2?sponsor=1",
                        { "type": "github", "url": "https://github.com/sponsors/fb55" }
                    ]
                },
                "node_modules/no-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://example.com/sponsor"),
    );
    assert_eq!(
        graph.packages["object-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/sponsors/foo"),
    );
    // Array form: aube collapses to the first URL.
    assert_eq!(
        graph.packages["array-funding@1.0.0"].funding_url.as_deref(),
        Some("https://github.com/sponsors/csstools"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the URL.
    assert_eq!(
        graph.packages["mixed-array-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/fb55/htmlparser2?sponsor=1"),
    );
    assert!(graph.packages["no-funding@1.0.0"].funding_url.is_none());
}

/// Real-world `package-lock.json` entries can carry the legacy
/// object / array-of-objects shapes for `license:` (npm copies
/// whatever's in the package's `package.json` verbatim, and older
/// packages like `tv4` still ship the deprecated forms). Regression
/// guard for https://github.com/aubepkg/aube/discussions/510.
#[test]
fn test_parse_license_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-license": "1.0.0",
                        "object-license": "1.0.0",
                        "array-license": "1.0.0",
                        "mixed-array-license": "1.0.0",
                        "no-license": "1.0.0"
                    }
                },
                "node_modules/string-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "license": "MIT"
                },
                "node_modules/object-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "license": { "type": "ISC", "url": "https://example.com/ISC" }
                },
                "node_modules/array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "license": [
                        { "type": "Public Domain", "url": "http://geraintluff.github.io/tv4/LICENSE.txt" },
                        { "type": "MIT", "url": "http://jsonary.com/LICENSE.txt" }
                    ]
                },
                "node_modules/mixed-array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "license": [
                        "MIT",
                        { "type": "Apache-2.0", "url": "https://example.com/apache" }
                    ]
                },
                "node_modules/no-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-license@1.0.0"].license.as_deref(),
        Some("MIT"),
    );
    assert_eq!(
        graph.packages["object-license@1.0.0"].license.as_deref(),
        Some("ISC"),
    );
    // Array form: aube collapses to the first license type.
    assert_eq!(
        graph.packages["array-license@1.0.0"].license.as_deref(),
        Some("Public Domain"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the license.
    assert_eq!(
        graph.packages["mixed-array-license@1.0.0"]
            .license
            .as_deref(),
        Some("MIT"),
    );
    assert!(graph.packages["no-license@1.0.0"].license.is_none());
}

/// A package declared in both `devDependencies` and
/// `optionalDependencies` must yield exactly one root `DirectDep`.
/// npm records such a declaration as a plain `dev: true` entry, so dev
/// wins — matching `seed_direct_deps` in aube-resolver. Emitting a
/// second `DirectDep` made `--frozen-lockfile` reject an untouched
/// npm-generated lockfile as section drift, and made the linker create
/// the root symlink twice. See discussion #1544.
#[test]
fn dev_and_optional_overlap_yields_one_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
        "name": "test",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "test",
                "version": "1.0.0",
                "devDependencies": { "foo": "1.2.3" },
                "optionalDependencies": { "foo": "1.2.3" }
            },
            "node_modules/foo": {
                "version": "1.2.3",
                "resolved": "https://registry.npmjs.org/foo/-/foo-1.2.3.tgz",
                "integrity": "sha512-aaa",
                "dev": true
            }
        }
    }"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1, "expected one direct dep, got {root:?}");
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_type, DepType::Dev);
}

/// The overlap guard keys on the *declared* name, so two npm aliases of
/// the same underlying package remain separate direct deps.
#[test]
fn distinct_aliases_of_one_package_stay_separate_direct_deps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
        "name": "test",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "test",
                "version": "1.0.0",
                "dependencies": { "foo-v1": "npm:foo@1.2.3", "foo-v2": "npm:foo@2.0.0" }
            },
            "node_modules/foo-v1": {
                "name": "foo",
                "version": "1.2.3",
                "resolved": "https://registry.npmjs.org/foo/-/foo-1.2.3.tgz",
                "integrity": "sha512-aaa"
            },
            "node_modules/foo-v2": {
                "name": "foo",
                "version": "2.0.0",
                "resolved": "https://registry.npmjs.org/foo/-/foo-2.0.0.tgz",
                "integrity": "sha512-bbb"
            }
        }
    }"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2, "both aliases must survive, got {root:?}");
}

/// The overlap guard applies per importer, not just to the root: a
/// workspace member declaring the same package in two sections must
/// also yield a single `DirectDep`. The drift validator resolves one
/// expected section per name for every importer.
#[test]
fn workspace_member_dev_and_optional_overlap_yields_one_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["packages/app"]
                },
                "node_modules/@scope/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "packages/app": {
                    "name": "@scope/app",
                    "version": "0.68.1",
                    "devDependencies": { "chalk": "^5.4.1" },
                    "optionalDependencies": { "chalk": "^5.4.1" }
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let member = graph.importers.get("packages/app").unwrap();
    let chalk: Vec<_> = member.iter().filter(|d| d.name == "chalk").collect();
    assert_eq!(chalk.len(), 1, "expected one direct dep, got {chalk:?}");
    assert_eq!(chalk[0].dep_type, DepType::Dev);
}

/// npm installs an importer's required peers and records them only in
/// the importer's `peerDependencies`. The reader must surface them as
/// direct deps so a frozen install doesn't read the peer-only
/// declaration as "manifest adds". Optional peers stay out, and an
/// owned section wins over a peer declaration of the same name.
#[test]
fn workspace_member_required_peers_are_direct_deps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["packages/*"],
                    "devDependencies": { "is-number": "7.0.0" },
                    "peerDependencies": { "kind-of": "^6.0.0" }
                },
                "node_modules/is-number": {
                    "version": "7.0.0",
                    "integrity": "sha512-isnumber"
                },
                "node_modules/is-odd": {
                    "version": "3.0.1",
                    "integrity": "sha512-isodd"
                },
                "node_modules/is-even": {
                    "version": "1.0.0",
                    "integrity": "sha512-iseven"
                },
                "node_modules/kind-of": {
                    "version": "6.0.3",
                    "integrity": "sha512-kindof"
                },
                "node_modules/peer-consumer": {
                    "resolved": "packages/consumer",
                    "link": true
                },
                "packages/consumer": {
                    "name": "peer-consumer",
                    "version": "1.0.0",
                    "dependencies": { "is-even": "1.0.0", "is-odd": "3.0.1" },
                    "peerDependencies": {
                        "is-even": "1.0.0",
                        "is-number": "7.0.0",
                        "is-odd": "^3.0.0",
                        "kind-of": "^6.0.0"
                    },
                    "peerDependenciesMeta": { "kind-of": { "optional": true } }
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let root = &graph.importers["."];
    let kind_of = root.iter().find(|d| d.name == "kind-of").unwrap();
    assert_eq!(kind_of.dep_type, DepType::Production);
    assert_eq!(kind_of.specifier.as_deref(), Some("^6.0.0"));

    let member = &graph.importers["packages/consumer"];
    let is_number = member.iter().find(|d| d.name == "is-number").unwrap();
    assert_eq!(is_number.dep_type, DepType::Production);
    assert_eq!(is_number.specifier.as_deref(), Some("7.0.0"));
    let is_odd: Vec<_> = member.iter().filter(|d| d.name == "is-odd").collect();
    assert_eq!(is_odd.len(), 1);
    assert_eq!(is_odd[0].specifier.as_deref(), Some("3.0.1"));
    assert!(!member.iter().any(|d| d.name == "kind-of"));

    let root_manifest: aube_manifest::PackageJson = serde_json::from_str(
        r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "workspaces": ["packages/*"],
            "devDependencies": { "is-number": "7.0.0" },
            "peerDependencies": { "kind-of": "^6.0.0" }
        }"#,
    )
    .unwrap();
    let member_manifest: aube_manifest::PackageJson = serde_json::from_str(
        r#"{
            "name": "peer-consumer",
            "version": "1.0.0",
            "dependencies": { "is-even": "1.0.0", "is-odd": "3.0.1" },
            "peerDependencies": {
                "is-even": "1.0.0",
                "is-number": "7.0.0",
                "is-odd": "^3.0.0",
                "kind-of": "^6.0.0"
            },
            "peerDependenciesMeta": { "kind-of": { "optional": true } }
        }"#,
    )
    .unwrap();
    assert_eq!(
        graph.check_drift_workspace_for_kind(
            &[
                (".".to_string(), root_manifest.clone()),
                ("packages/consumer".to_string(), member_manifest),
            ],
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
            LockfileKind::Npm,
        ),
        DriftStatus::Fresh
    );

    // Write-back keeps the peer-only declaration out of `dependencies`
    // while owned deps stay, including one whose spec equals its peer's.
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &root_manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let consumer = &written["packages"]["packages/consumer"];
    assert_eq!(
        consumer["dependencies"],
        serde_json::json!({ "is-even": "1.0.0", "is-odd": "3.0.1" })
    );
    assert_eq!(consumer["peerDependencies"]["is-number"], "7.0.0");
    assert_eq!(consumer["peerDependencies"]["is-even"], "1.0.0");
    assert_eq!(
        written["packages"][""]["peerDependencies"],
        serde_json::json!({ "kind-of": "^6.0.0" })
    );
    assert!(written["packages"][""].get("dependencies").is_none());
}

/// npm records the target of every `file:` dep as an importer, including
/// ones a workspace member declares. Given their manifests, as the install
/// passes them, the drift check accepts those importers and compares their
/// dependencies like a project's.
#[test]
fn test_drift_checks_file_dep_importers_of_workspace_members() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // What npm 11 writes for a member depending on `file:./vendor/x`,
    // which in turn depends on `file:../y`.
    std::fs::write(
        tmp.path(),
        r#"
{
  "name": "root",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "root",
      "workspaces": [
        "packages/*"
      ]
    },
    "node_modules/app": {
      "resolved": "packages/app",
      "link": true
    },
    "node_modules/x": {
      "resolved": "packages/app/vendor/x",
      "link": true
    },
    "node_modules/y": {
      "resolved": "packages/app/vendor/y",
      "link": true
    },
    "packages/app": {
      "version": "1.0.0",
      "dependencies": {
        "x": "file:./vendor/x"
      }
    },
    "packages/app/vendor/x": {
      "version": "1.0.0",
      "dependencies": {
        "y": "file:../y"
      }
    },
    "packages/app/vendor/y": {
      "version": "1.0.0"
    }
  }
}
"#,
    )
    .unwrap();
    let graph = parse(tmp.path()).unwrap();
    let root: aube_manifest::PackageJson =
        serde_json::from_str(r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#)
            .unwrap();
    let app: aube_manifest::PackageJson = serde_json::from_str(
        r#"{"name":"app","version":"1.0.0","dependencies":{"x":"file:./vendor/x"}}"#,
    )
    .unwrap();
    let members = [(".".to_string(), root), ("packages/app".to_string(), app)];
    let check = |targets: &[(String, aube_manifest::PackageJson)]| {
        graph.check_drift_workspace_with_local_targets_for_kind(
            &members,
            targets,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
            LockfileKind::Npm,
        )
    };

    // The install reads the reachable `file:` targets' manifests from disk.
    assert_eq!(
        graph.npm_local_importers(&[".", "packages/app"]),
        ["packages/app/vendor/x", "packages/app/vendor/y"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let targets = |x_deps: &str| {
        vec![
            (
                "packages/app/vendor/x".to_string(),
                serde_json::from_str(&format!(
                    r#"{{"name":"x","version":"1.0.0","dependencies":{x_deps}}}"#
                ))
                .unwrap(),
            ),
            (
                "packages/app/vendor/y".to_string(),
                serde_json::from_str(r#"{"name":"y","version":"1.0.0"}"#).unwrap(),
            ),
        ]
    };

    assert_eq!(check(&targets(r#"{"y":"file:../y"}"#)), DriftStatus::Fresh);
    // A dependency added to a `file:` target is drift, as `npm ci` reports.
    assert!(matches!(
        check(&targets(r#"{"y":"file:../y","z":"^1.0.0"}"#)),
        DriftStatus::Stale { reason } if reason.contains("packages/app/vendor/x")
    ));
    // Without the targets' manifests they read as projects that left.
    assert!(matches!(
        check(&[]),
        DriftStatus::Stale { reason } if reason.contains("packages/app/vendor/")
    ));
    assert_eq!(graph.npm_local_importers(&["."]), BTreeSet::new());
}

/// npm writes a workspace member without a `version` in its package.json
/// as an empty entry. Reading it must not fail.
#[test]
fn test_parse_workspace_member_without_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "root",
            "lockfileVersion": 3,
            "requires": true,
            "packages": {
                "": {
                    "name": "root",
                    "workspaces": ["packages/*"]
                },
                "node_modules/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "packages/app": {}
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let dep_path = LocalSource::Link(PathBuf::from("packages/app")).dep_path("app");
    assert_eq!(graph.packages[&dep_path].version, "0.0.0");
    assert!(graph.importers.contains_key("packages/app"));
}

/// A member nothing depends on is written from its package.json: one
/// without a version stays versionless, as npm writes it, and its
/// optional peers keep `peerDependenciesMeta`, so reading the lockfile
/// back keeps them out of its required deps.
#[test]
fn test_write_npm_workspace_member_versionless_and_optional_peers_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, manifest) in [
        (
            "packages/a",
            r#"{"name":"a","version":"1.0.0","peerDependencies":{"p":"^1"},"peerDependenciesMeta":{"p":{"optional":true}}}"#,
        ),
        ("packages/b", r#"{"name":"b"}"#),
    ] {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        std::fs::write(tmp.path().join(dir).join("package.json"), manifest).unwrap();
    }
    let mut graph = LockfileGraph::default();
    for importer in [".", "packages/a", "packages/b"] {
        graph.importers.insert(importer.to_string(), Vec::new());
    }
    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let path = tmp.path().join("package-lock.json");
    write(&path, &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let packages = &json["packages"];
    assert_eq!(
        packages["packages/a"]["peerDependenciesMeta"]["p"]["optional"],
        true
    );
    assert!(packages["packages/b"].get("version").is_none());

    let reparsed = parse(&path).unwrap();
    assert!(
        reparsed.importers["packages/a"]
            .iter()
            .all(|dep| dep.name != "p"),
        "an optional peer must not become a required dep"
    );
    let b = LocalSource::Link(PathBuf::from("packages/b")).dep_path("b");
    assert_eq!(reparsed.packages[&b].version, "0.0.0");

    // Rewriting the reread graph keeps `b` versionless.
    write(&path, &reparsed, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(json["packages"]["packages/b"].get("version").is_none());
}

/// npm copies an object-form `workspaces` field as declared: an absent
/// `nohoist` stays absent and a declared one is kept.
#[test]
fn test_write_npm_object_form_workspaces_as_declared() {
    for (nohoist, expected) in [
        (Vec::new(), serde_json::json!({"packages": ["packages/*"]})),
        (
            vec!["**/x".to_string()],
            serde_json::json!({"packages": ["packages/*"], "nohoist": ["**/x"]}),
        ),
    ] {
        let manifest = aube_manifest::PackageJson {
            name: Some("root".to_string()),
            workspaces: Some(aube_manifest::Workspaces::Object {
                packages: vec!["packages/*".to_string()],
                nohoist,
                catalog: BTreeMap::new(),
                catalogs: BTreeMap::new(),
            }),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package-lock.json");
        write(&path, &LockfileGraph::default(), &manifest).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["packages"][""]["workspaces"], expected);
    }
}

fn dev_funding_manifest(with_is_number: bool) -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("x".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: with_is_number
            .then(|| ("is-number".to_string(), "^7.0.0".to_string()))
            .into_iter()
            .collect(),
        dev_dependencies: [("ajv".to_string(), "8.17.1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

fn read_fixture(name: &str) -> (PathBuf, String) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    // LF-normalize like `test_write_byte_identical_to_native_npm`.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");
    (fixture, original)
}

/// npm 11 output for a lone `ajv@8.17.1` dev dependency: `dev: true`
/// sits before `license`, funding objects keep `type`, and `fast-uri`'s
/// two-element funding array sits before `license` (npm orders array
/// values with scalars). A parse → write round-trip must keep every byte.
#[test]
fn test_write_keeps_native_npm_dev_flags_and_funding() {
    let (fixture, original) = read_fixture("npm-native-dev-funding.json");
    let graph = parse(&fixture).unwrap();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &dev_funding_manifest(false)).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(written, original);
}

/// The `aube add is-number@7.0.0` case from discussion #1810: adding a
/// freshly resolved package (URL-only metadata, no funding) to the graph
/// read from npm's lockfile must match what `npm install is-number@7.0.0`
/// writes, which leaves every existing entry untouched.
#[test]
fn test_write_added_package_leaves_existing_npm_entries_untouched() {
    let (fixture, _) = read_fixture("npm-native-dev-funding.json");
    let (_, expected) = read_fixture("npm-native-dev-funding-added.json");
    let mut graph = parse(&fixture).unwrap();
    graph.packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            integrity: Some(
                "sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng=="
                    .to_string(),
            ),
            tarball_url: Some(
                "https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz".to_string(),
            ),
            license: Some("MIT".to_string()),
            engines: [("node".to_string(), ">=0.12.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.importers.get_mut(".").unwrap().push(DirectDep {
        name: "is-number".to_string(),
        dep_path: "is-number@7.0.0".to_string(),
        dep_type: DepType::Production,
        specifier: Some("^7.0.0".to_string()),
    });

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &dev_funding_manifest(true)).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(written, expected);
}

/// Spot checks against `keys.sort((a, b) => a.localeCompare(b, 'en'))`
/// as Node evaluates it.
#[test]
fn test_npm_key_order_matches_locale_compare() {
    let mut value = serde_json::json!({
        "JSONStream": 1, "abab": 1, "Ab": 1, "aB": 1, "a/b": 1, "a@b": 1,
        "a.b": 1, "a-b": 1, "a_b": 1, "a1": 1, "a": 1, "ab": 1, "AB": 1,
    });
    super::write::sort_keys_like_npm(&mut value);
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "a",
            "a_b",
            "a-b",
            "a.b",
            "a@b",
            "a/b",
            "a1",
            "ab",
            "aB",
            "Ab",
            "AB",
            "abab",
            "JSONStream"
        ]
    );
}

fn arb_funding() -> impl proptest::strategy::Strategy<Value = serde_json::Value> {
    use proptest::prelude::*;
    let url = "https://[a-z]{1,8}\\.example/[a-z]{0,6}";
    let object = (proptest::option::of("[a-z]{1,8}"), url).prop_map(|(kind, url)| match kind {
        Some(kind) => serde_json::json!({ "type": kind, "url": url }),
        None => serde_json::json!({ "url": url }),
    });
    let element = prop_oneof![url.prop_map(serde_json::Value::from), object.clone()];
    prop_oneof![
        url.prop_map(serde_json::Value::from),
        object,
        proptest::collection::vec(element, 1..4).prop_map(serde_json::Value::Array),
    ]
}

/// One generated package entry: how the root depends on it, plus the
/// optional fields npm copies from its manifest.
type ArbEntry = (
    DepType,
    Option<&'static str>,
    Option<serde_json::Value>,
    Option<&'static str>,
);

/// Lay a package-lock.json out in npm's key order, inserting keys by
/// hand (serde_json keeps insertion order) so the expectation doesn't
/// lean on the writer's sort: scalars and arrays first, led by
/// `version`/`resolved`/`integrity`, the rest alphabetical; objects after.
fn npm_ordered_lockfile(entries: &[ArbEntry]) -> String {
    use serde_json::{Map, Value, json};
    let mut root = Map::new();
    root.insert("name".into(), json!("x"));
    root.insert("version".into(), json!("1.0.0"));
    for (dep_type, field) in [
        (DepType::Production, "dependencies"),
        (DepType::Dev, "devDependencies"),
        (DepType::Optional, "optionalDependencies"),
    ] {
        let deps: Map<String, Value> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.0 == dep_type)
            .map(|(idx, _)| (format!("pkg-{idx}"), json!(format!("1.0.{idx}"))))
            .collect();
        if !deps.is_empty() {
            root.insert(field.into(), Value::Object(deps));
        }
    }
    let mut packages = Map::new();
    packages.insert(String::new(), Value::Object(root));
    for (idx, (dep_type, license, funding, engine)) in entries.iter().enumerate() {
        let mut entry = Map::new();
        entry.insert("version".into(), json!(format!("1.0.{idx}")));
        entry.insert(
            "resolved".into(),
            json!(format!(
                "https://registry.npmjs.org/pkg-{idx}/-/pkg-{idx}-1.0.{idx}.tgz"
            )),
        );
        entry.insert("integrity".into(), json!(format!("sha512-{idx}")));
        if *dep_type == DepType::Dev {
            entry.insert("dev".into(), json!(true));
        }
        let object_funding = funding.as_ref().filter(|f| f.is_object());
        if let Some(funding) = funding.as_ref().filter(|f| !f.is_object()) {
            entry.insert("funding".into(), funding.clone());
        }
        if let Some(license) = license {
            entry.insert("license".into(), json!(license));
        }
        if *dep_type == DepType::Optional {
            entry.insert("optional".into(), json!(true));
        }
        if let Some(engine) = engine {
            entry.insert("engines".into(), json!({ "node": engine }));
        }
        if let Some(funding) = object_funding {
            entry.insert("funding".into(), funding.clone());
        }
        packages.insert(format!("node_modules/pkg-{idx}"), Value::Object(entry));
    }
    let lock = json!({
        "name": "x",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "requires": true,
        "packages": packages,
    });
    let mut text = serde_json::to_string_pretty(&lock).unwrap();
    text.push('\n');
    text
}

proptest::proptest! {
    /// read → write → read: an npm-ordered lockfile with dev/optional
    /// flags, licenses, engines and every funding shape is written back
    /// byte-identical, and re-reading it yields the same metadata.
    #[test]
    fn npm_ordered_entries_roundtrip_byte_identical(
        entries in proptest::collection::vec(
            (
                proptest::sample::select(vec![DepType::Production, DepType::Dev, DepType::Optional]),
                proptest::option::of(proptest::sample::select(vec!["MIT", "ISC", "BSD-3-Clause"])),
                proptest::option::of(arb_funding()),
                proptest::option::of(proptest::sample::select(vec![">=12", ">=0.10.0"])),
            ),
            1..8,
        )
    ) {
        let original = npm_ordered_lockfile(&entries);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &original).unwrap();
        let graph = parse(tmp.path()).unwrap();
        let mut manifest = aube_manifest::PackageJson {
            name: Some("x".to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
        };
        for (idx, entry) in entries.iter().enumerate() {
            let section = match entry.0 {
                DepType::Production => &mut manifest.dependencies,
                DepType::Dev => &mut manifest.dev_dependencies,
                DepType::Optional => &mut manifest.optional_dependencies,
            };
            section.insert(format!("pkg-{idx}"), format!("1.0.{idx}"));
        }
        write(tmp.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        proptest::prop_assert_eq!(&written, &original);

        let reparsed = parse(tmp.path()).unwrap();
        proptest::prop_assert_eq!(reparsed.packages.len(), graph.packages.len());
        for (key, pkg) in &graph.packages {
            let again = &reparsed.packages[key];
            proptest::prop_assert_eq!(&again.funding_url, &pkg.funding_url);
            proptest::prop_assert_eq!(&again.extra_meta, &pkg.extra_meta);
            proptest::prop_assert_eq!(&again.license, &pkg.license);
        }
    }
}

/// JavaScript lists array-index keys (`"2"`, `"10"`) before every other
/// key, in numeric order, so npm writes numeric package names that way.
/// Expected bytes come from npm's `json-stringify-nice` with arborist's
/// key order. `"01"` isn't an array index and sorts as text.
#[test]
fn test_write_numeric_dependency_names_round_trip_in_js_key_order() {
    let original = r#"{
  "name": "x",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "x",
      "version": "1.0.0",
      "dependencies": {
        "2": "1.0.0",
        "10": "1.0.0",
        "01": "1.0.0",
        "a": "1.0.0"
      }
    },
    "node_modules/01": {
      "version": "1.0.0",
      "integrity": "sha512-01"
    },
    "node_modules/10": {
      "version": "1.0.0",
      "integrity": "sha512-10"
    },
    "node_modules/2": {
      "version": "1.0.0",
      "integrity": "sha512-2"
    },
    "node_modules/a": {
      "version": "1.0.0",
      "integrity": "sha512-a"
    }
  }
}
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), original).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("x".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: ["2", "10", "01", "a"]
            .into_iter()
            .map(|name| (name.to_string(), "1.0.0".to_string()))
            .collect(),
        ..Default::default()
    };
    write(tmp.path(), &graph, &manifest).unwrap();
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

/// The target entries of a `file:` directory and of a workspace member
/// keep the verbatim `funding` read from package-lock.json too.
#[test]
fn test_write_preserves_funding_on_local_targets() {
    let funding = serde_json::json!([
        { "type": "github", "url": "https://github.com/sponsors/a" },
        "https://example.com/b"
    ]);
    let with_funding = |mut pkg: LockedPackage| {
        pkg.funding_url = Some("https://github.com/sponsors/a".to_string());
        pkg.extra_meta
            .insert(crate::EXTRA_NPM_FUNDING.to_string(), funding.clone());
        pkg
    };
    let dir = LocalSource::Directory(PathBuf::from("vendor/local-dir"));
    let dir_dep_path = dir.dep_path("local-dir");
    let link = LocalSource::Link(PathBuf::from("web"));
    let link_dep_path = link.dep_path("web");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dir_dep_path.clone(),
        with_funding(LockedPackage {
            name: "local-dir".to_string(),
            version: "1.0.0".to_string(),
            dep_path: dir_dep_path.clone(),
            local_source: Some(dir),
            ..Default::default()
        }),
    );
    graph.packages.insert(
        link_dep_path.clone(),
        with_funding(LockedPackage {
            name: "web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: link_dep_path.clone(),
            local_source: Some(link),
            ..Default::default()
        }),
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "local-dir".to_string(),
                dep_path: dir_dep_path,
                dep_type: DepType::Production,
                specifier: Some("file:vendor/local-dir".to_string()),
            },
            DirectDep {
                name: "web".to_string(),
                dep_path: link_dep_path,
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    graph.importers.insert("web".to_string(), Vec::new());
    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("local-dir".to_string(), "file:vendor/local-dir".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("package-lock.json");
    write(&path, &graph, &manifest).unwrap();

    let lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(lock["packages"]["vendor/local-dir"]["funding"], funding);
    assert_eq!(lock["packages"]["web"]["funding"], funding);

    let reparsed = parse(&path).unwrap();
    for pkg in reparsed
        .packages
        .values()
        .filter(|pkg| pkg.name == "local-dir" || pkg.name == "web")
    {
        assert_eq!(pkg.extra_meta.get(crate::EXTRA_NPM_FUNDING), Some(&funding));
    }
}

/// npm takes `funding` for the root and each workspace member from their
/// own package.json (skipping empty values), so a loaded manifest
/// overrides what the old lockfile recorded, including removing it. The
/// lockfile value only stands in for a member whose manifest is missing.
#[test]
fn test_write_takes_project_funding_from_manifests() {
    let stale = serde_json::json!({ "url": "https://example.com/stale" });
    let member_funding =
        serde_json::json!({ "type": "github", "url": "https://github.com/sponsors/docs" });
    let root_funding = serde_json::json!([
        { "type": "github", "url": "https://github.com/sponsors/root" },
        "https://example.com/root"
    ]);
    let mut graph = LockfileGraph::default();
    let mut root_deps = Vec::new();
    for member in ["web", "docs", "gone"] {
        let link = LocalSource::Link(PathBuf::from(member));
        let dep_path = link.dep_path(member);
        let mut pkg = LockedPackage {
            name: member.to_string(),
            version: "1.0.0".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(link),
            ..Default::default()
        };
        pkg.extra_meta
            .insert(crate::EXTRA_NPM_FUNDING.to_string(), stale.clone());
        graph.packages.insert(dep_path.clone(), pkg);
        graph.importers.insert(member.to_string(), Vec::new());
        root_deps.push(DirectDep {
            name: member.to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: None,
        });
    }
    graph.importers.insert(".".to_string(), root_deps);

    let tmp = tempfile::tempdir().unwrap();
    for (member, funding) in [("web", None), ("docs", Some(&member_funding))] {
        let dir = tmp.path().join(member);
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = serde_json::json!({ "name": member, "version": "1.0.0" });
        if let Some(funding) = funding {
            manifest["funding"] = funding.clone();
        }
        std::fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
    }
    let path = tmp.path().join("package-lock.json");
    let mut manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    manifest
        .extra
        .insert("funding".to_string(), root_funding.clone());
    write(&path, &graph, &manifest).unwrap();

    let lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let packages = &lock["packages"];
    assert_eq!(packages[""]["funding"], root_funding);
    assert!(packages["web"].get("funding").is_none());
    assert_eq!(packages["docs"]["funding"], member_funding);
    assert_eq!(packages["gone"]["funding"], stale);

    // npm drops an empty `funding`, like any other empty manifest value.
    manifest
        .extra
        .insert("funding".to_string(), serde_json::json!([]));
    write(&path, &graph, &manifest).unwrap();
    let lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(lock["packages"][""].get("funding").is_none());
}

proptest::proptest! {
    #[test]
    fn npm_peers_keep_runtime_edges_and_declared_shape(
        peer_flag in proptest::bool::ANY,
        declared_peer in proptest::bool::ANY,
        optional_peer in proptest::bool::ANY,
        peer_version in 1u8..20,
    ) {
        let version = format!("{peer_version}.0.0");
        let mut host = serde_json::json!({
            "version": "1.0.0",
            "peerDependencies": { "peer": "*" }
        });
        if declared_peer {
            host["optionalDependencies"] = serde_json::json!({ "peer": "*" });
        }
        if optional_peer {
            host["peerDependenciesMeta"] = serde_json::json!({ "peer": { "optional": true } });
        }
        let mut peer = serde_json::json!({ "version": version });
        if peer_flag {
            peer["peer"] = true.into();
        }
        if declared_peer || optional_peer {
            peer["optional"] = true.into();
        }
        let original = serde_json::json!({
            "lockfileVersion": 3,
            "packages": {
                "": { "dependencies": { "host": "1.0.0" } },
                "node_modules/host": host,
                "node_modules/peer": peer
            }
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), original.to_string()).unwrap();
        let graph = parse(file.path()).unwrap();
        let host = &graph.packages["host@1.0.0"];
        proptest::prop_assert_eq!(host.dependencies.get("peer"), Some(&version));
        proptest::prop_assert_eq!(host.optional_dependencies.contains_key("peer"), declared_peer || optional_peer);
        let manifest = aube_manifest::PackageJson {
            dependencies: [("host".into(), "1.0.0".into())].into(),
            ..Default::default()
        };
        write(file.path(), &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(file.path()).unwrap()).unwrap();
        proptest::prop_assert_eq!(&written["packages"], &original["packages"]);
    }
}

#[test]
fn npm_new_auto_installed_peers_are_flagged_without_becoming_declared_deps() {
    let mut graph = LockfileGraph::default();
    graph.importers.insert(
        ".".into(),
        vec![DirectDep {
            name: "host".into(),
            dep_path: "host@1.0.0".into(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".into()),
        }],
    );
    for (name, dependencies, peer_dependencies) in [
        (
            "host",
            [("peer".into(), "1.0.0".into())].into(),
            [("peer".into(), "*".into())].into(),
        ),
        (
            "peer",
            [("child".into(), "1.0.0".into())].into(),
            BTreeMap::new(),
        ),
        ("child", BTreeMap::new(), BTreeMap::new()),
    ] {
        graph.packages.insert(
            format!("{name}@1.0.0"),
            LockedPackage {
                name: name.into(),
                version: "1.0.0".into(),
                dep_path: format!("{name}@1.0.0"),
                declared_dependencies: if name == "peer" {
                    dependencies.clone()
                } else {
                    BTreeMap::new()
                },
                dependencies,
                peer_dependencies,
                ..Default::default()
            }
            .with_declared_dependencies_known(),
        );
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let manifest = aube_manifest::PackageJson {
        dependencies: [("host".into(), "1.0.0".into())].into(),
        ..Default::default()
    };
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    let packages = &written["packages"];
    assert_eq!(packages["node_modules/peer"]["peer"], true);
    assert_eq!(packages["node_modules/child"]["peer"], true);
    assert!(packages["node_modules/host"].get("dependencies").is_none());
    assert_eq!(
        packages["node_modules/host"]["peerDependencies"]["peer"],
        "*"
    );
}

#[test]
fn npm_preserves_nested_copies_until_their_resolution_changes() {
    let original = serde_json::json!({
        "lockfileVersion": 3,
        "packages": {
            "": { "dependencies": { "a": "1.0.0", "b": "1.0.0" } },
            "node_modules/a": { "version": "1.0.0", "dependencies": { "child": "*" } },
            "node_modules/b": { "version": "1.0.0", "dependencies": { "child": "*" } },
            "node_modules/a/node_modules/child": { "version": "1.0.0" },
            "node_modules/b/node_modules/child": { "version": "1.0.0" }
        }
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let mut graph = parse(file.path()).unwrap();
    let manifest = aube_manifest::PackageJson {
        dependencies: [("a".into(), "1.0.0".into()), ("b".into(), "1.0.0".into())].into(),
        ..Default::default()
    };
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(written["packages"], original["packages"]);

    graph
        .packages
        .get_mut("a@1.0.0")
        .unwrap()
        .dependencies
        .insert("child".into(), "2.0.0".into());
    graph.packages.insert(
        "child@2.0.0".into(),
        LockedPackage {
            name: "child".into(),
            version: "2.0.0".into(),
            dep_path: "child@2.0.0".into(),
            ..Default::default()
        },
    );
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert!(
        written["packages"]
            .get("node_modules/a/node_modules/child")
            .is_none()
    );
    assert_eq!(
        written["packages"]["node_modules/child"]["version"],
        "2.0.0"
    );
    assert_eq!(
        written["packages"]["node_modules/b/node_modules/child"]["version"],
        "1.0.0"
    );
}

#[test]
fn npm_clears_peer_flag_when_a_regular_dependency_starts_reaching_it() {
    let original = serde_json::json!({
        "lockfileVersion": 3,
        "packages": {
            "": { "dependencies": { "host": "1.0.0" } },
            "node_modules/host": { "version": "1.0.0", "peerDependencies": { "peer": "*" } },
            "node_modules/peer": { "version": "1.0.0", "peer": true }
        }
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let mut graph = parse(file.path()).unwrap();
    graph.importers.get_mut(".").unwrap().push(DirectDep {
        name: "peer".into(),
        dep_path: "peer@1.0.0".into(),
        dep_type: DepType::Production,
        specifier: Some("1.0.0".into()),
    });
    let manifest = aube_manifest::PackageJson {
        dependencies: [
            ("host".into(), "1.0.0".into()),
            ("peer".into(), "1.0.0".into()),
        ]
        .into(),
        ..Default::default()
    };
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert!(
        written["packages"]["node_modules/peer"]
            .get("peer")
            .is_none()
    );
}

proptest::proptest! {
    #[test]
    fn npm_bundled_entries_stay_with_their_owner(version in 1u8..20, dev in proptest::bool::ANY, optional in proptest::bool::ANY) {
        let version = format!("{version}.0.0");
        let mut owner = serde_json::json!({
            "version": "1.0.0", "bundleDependencies": ["child"],
            "dependencies": {"child": "*"}
        });
        let mut child = serde_json::json!({
            "version": version, "inBundle": true, "license": "MIT",
            "dependencies": {"grandchild": "*"}
        });
        let mut grandchild = serde_json::json!({"version": "1.0.0", "inBundle": true});
        if dev {
            owner["dev"] = true.into();
            child["dev"] = true.into();
            grandchild["dev"] = true.into();
        }
        if optional {
            owner.as_object_mut().unwrap().remove("dependencies");
            owner["optionalDependencies"] = serde_json::json!({"child": "*"});
            child["optional"] = true.into();
            grandchild["optional"] = true.into();
        }
        let section = if dev {"devDependencies"} else {"dependencies"};
        let original = serde_json::json!({"lockfileVersion": 3, "packages": {
            "": {section: {"owner": "1.0.0"}},
            "node_modules/owner": owner,
            "node_modules/owner/node_modules/child": child,
            "node_modules/owner/node_modules/grandchild": grandchild
        }});
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), original.to_string()).unwrap();
        let graph = parse(file.path()).unwrap();
        proptest::prop_assert_eq!(graph.packages.len(), 1);
        proptest::prop_assert!(graph.packages["owner@1.0.0"].dependencies.is_empty());
        proptest::prop_assert_eq!(&graph.packages["owner@1.0.0"].bundled_dependencies, &["child"]);
        let manifest: aube_manifest::PackageJson = serde_json::from_value(original["packages"][""].clone()).unwrap();
        write(file.path(), &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
        proptest::prop_assert_eq!(&written["packages"], &original["packages"]);

        let mut graph = graph;
        graph.importers.get_mut(".").unwrap()[0].dep_type = DepType::Production;
        let manifest = aube_manifest::PackageJson {
            dependencies: [("owner".into(), "1.0.0".into())].into(), ..Default::default()
        };
        write(file.path(), &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
        for name in ["child", "grandchild"] {
            let child = &written["packages"][format!("node_modules/owner/node_modules/{name}")];
            proptest::prop_assert!(child.get("dev").is_none());
            proptest::prop_assert_eq!(child["optional"].as_bool().unwrap_or(false), optional);
            proptest::prop_assert_eq!(child["inBundle"].as_bool(), Some(true));
        }
    }
}

#[test]
fn npm_fresh_bundled_dependencies_keep_declarations_without_registry_entries() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut graph = LockfileGraph::default();
    graph.importers.insert(
        ".".into(),
        vec![DirectDep {
            name: "owner".into(),
            dep_path: "owner@1.0.0".into(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".into()),
        }],
    );
    graph.packages.insert(
        "owner@1.0.0".into(),
        LockedPackage {
            name: "owner".into(),
            version: "1.0.0".into(),
            dep_path: "owner@1.0.0".into(),
            bundled_dependencies: vec!["child".into()],
            declared_dependencies: [("child".into(), "^2.0.0".into())].into(),
            ..Default::default()
        }
        .with_declared_dependencies_known(),
    );
    let manifest = aube_manifest::PackageJson {
        dependencies: [("owner".into(), "1.0.0".into())].into(),
        ..Default::default()
    };
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(
        written["packages"]["node_modules/owner"]["bundleDependencies"],
        serde_json::json!(["child"])
    );
    assert_eq!(
        written["packages"]["node_modules/owner"]["dependencies"]["child"],
        "^2.0.0"
    );
    assert_eq!(written["packages"].as_object().unwrap().len(), 2);
}

#[test]
fn npm_optional_peers_merge_dev_and_optional_reachability() {
    let original = serde_json::json!({"lockfileVersion": 3, "packages": {
        "": {"dependencies": {"host": "1.0.0"}, "devDependencies": {"peer": "1.0.0"}},
        "node_modules/host": {"version": "1.0.0", "peerDependencies": {"peer": "*"},
            "peerDependenciesMeta": {"peer": {"optional": true}}},
        "node_modules/peer": {"version": "1.0.0", "devOptional": true, "dependencies": {"child": "*"}},
        "node_modules/child": {"version": "1.0.0", "devOptional": true}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let mut graph = parse(file.path()).unwrap();
    let mut manifest: aube_manifest::PackageJson =
        serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(written["packages"], original["packages"]);

    // Freshly resolved peer edges need the same classification without the
    // reader's optional-edge map.
    graph
        .packages
        .get_mut("host@1.0.0")
        .unwrap()
        .optional_dependencies
        .clear();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(written["packages"], original["packages"]);

    // A required production root clears devOptional through its dependencies.
    graph
        .importers
        .get_mut(".")
        .unwrap()
        .iter_mut()
        .find(|dep| dep.name == "peer")
        .unwrap()
        .dep_type = DepType::Production;
    manifest.dev_dependencies.remove("peer");
    manifest.dependencies.insert("peer".into(), "1.0.0".into());
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    for name in ["peer", "child"] {
        let entry = &written["packages"][format!("node_modules/{name}")];
        for flag in ["dev", "optional", "devOptional"] {
            assert!(entry.get(flag).is_none(), "{name}: {flag}");
        }
    }
}

#[test]
fn npm_preserves_nested_dependencies_beside_their_consumer() {
    let original = serde_json::json!({"lockfileVersion": 3, "packages": {
        "": {"dependencies": {"a": "1.0.0", "child": "1.0.0"}},
        "node_modules/a": {"version": "1.0.0", "dependencies": {"consumer": "*"}},
        "node_modules/a/node_modules/consumer": {"version": "1.0.0", "dependencies": {"child": "2.0.0"}},
        "node_modules/a/node_modules/child": {"version": "2.0.0"},
        "node_modules/child": {"version": "1.0.0"}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let manifest: aube_manifest::PackageJson =
        serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(written["packages"], original["packages"]);
}

proptest::proptest! {
    #[test]
    fn npm_preserves_peer_flags_on_each_installed_copy(
        first_peer in proptest::bool::ANY,
        second_peer in proptest::bool::ANY,
        version in 1u8..20,
    ) {
        let version = format!("{version}.0.0");
        let mut original = serde_json::json!({"lockfileVersion": 3, "packages": {
            "": {"dependencies": {"a": "1.0.0", "b": "1.0.0"}},
            "node_modules/a": {"version": "1.0.0", "dependencies": {"child": "*"}},
            "node_modules/b": {"version": "1.0.0", "peerDependencies": {"child": "*"}},
            "node_modules/a/node_modules/child": {"version": version},
            "node_modules/b/node_modules/child": {"version": version}
        }});
        for (name, peer) in [("a", first_peer), ("b", second_peer)] {
            if peer {
                original["packages"][format!("node_modules/{name}/node_modules/child")]["peer"] = true.into();
            }
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), original.to_string()).unwrap();
        let graph = parse(file.path()).unwrap();
        let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
        write(file.path(), &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
        proptest::prop_assert_eq!(&written["packages"], &original["packages"]);
    }

    #[test]
    fn npm_preserves_installed_package_workspaces(object_form in proptest::bool::ANY) {
        let workspaces = if object_form {
            serde_json::json!({"packages": ["packages/*"], "nohoist": ["**/child"]})
        } else {
            serde_json::json!(["tests/*", "packages/*"])
        };
        let original = serde_json::json!({"lockfileVersion": 3, "packages": {
            "": {"dependencies": {"host": "1.0.0"}},
            "node_modules/host": {"version": "1.0.0", "workspaces": workspaces}
        }});
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), original.to_string()).unwrap();
        let graph = parse(file.path()).unwrap();
        let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
        let output = tempfile::NamedTempFile::new().unwrap();
        write(output.path(), &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(output.path()).unwrap()).unwrap();
        proptest::prop_assert_eq!(&written["packages"], &original["packages"]);
    }
}

#[test]
fn npm_reclassifies_only_the_copy_gaining_a_regular_incoming_edge() {
    let original = serde_json::json!({"lockfileVersion": 3, "packages": {
        "": {"dependencies": {"a": "1.0.0", "b": "1.0.0"}},
        "node_modules/a": {"version": "1.0.0", "peerDependencies": {"child": "*"}},
        "node_modules/b": {"version": "1.0.0", "peerDependencies": {"child": "*"}},
        "node_modules/a/node_modules/child": {"version": "1.0.0", "peer": true},
        "node_modules/b/node_modules/child": {"version": "1.0.0", "peer": true}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let mut graph = parse(file.path()).unwrap();
    graph
        .packages
        .get_mut("a@1.0.0")
        .unwrap()
        .declared_dependencies
        .insert("child".into(), "*".into());
    let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    assert!(
        written["packages"]["node_modules/a/node_modules/child"]
            .get("peer")
            .is_none()
    );
    assert_eq!(
        written["packages"]["node_modules/b/node_modules/child"]["peer"],
        true
    );
}

#[test]
fn npm_preserves_nested_optional_placements_with_an_unchanged_ancestor() {
    let original = serde_json::json!({"lockfileVersion": 3, "packages": {
        "": {"devDependencies": {"host": "1.0.0", "provider": "2.0.0"}},
        "node_modules/host": {"version": "1.0.0", "dev": true, "optionalDependencies": {"platform": "1.0.0"}},
        "node_modules/platform": {"version": "1.0.0", "dev": true, "optional": true, "cpu": ["wasm32"], "dependencies": {"provider": "1.0.0"}},
        "node_modules/platform/node_modules/child": {"version": "1.0.0", "dev": true, "optional": true},
        "node_modules/provider": {"version": "2.0.0", "dev": true, "dependencies": {"child": "2.0.0"}},
        "node_modules/child": {"version": "2.0.0", "dev": true}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    for (path, entry) in original["packages"].as_object().unwrap() {
        assert_eq!(&written["packages"][path], entry, "{path}");
    }
    assert_eq!(written["packages"], original["packages"]);
}

#[test]
fn review_round12_bundled_child_does_not_make_hoisted_peer_copy_ordinary() {
    let original = serde_json::json!({"lockfileVersion": 3, "requires": true, "packages": {
        "": {"dependencies": {"owner": "1.0.0", "peerhost": "1.0.0"}},
        "node_modules/b": {"version": "2.0.0", "peer": true},
        "node_modules/owner": {"version": "1.0.0", "bundleDependencies": ["b"], "dependencies": {"b": "1.0.0"}},
        "node_modules/owner/node_modules/b": {"version": "1.0.0", "inBundle": true},
        "node_modules/peerhost": {"version": "1.0.0", "peerDependencies": {"b": "2.0.0"}}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    assert_eq!(written["packages"], original["packages"], "{written:#}");
}

#[test]
fn npm_bundled_child_keeps_external_peer_reachable() {
    let original = serde_json::json!({"lockfileVersion": 3, "requires": true, "packages": {
        "": {"dependencies": {"owner": "1.0.0"}},
        "node_modules/owner": {"version": "1.0.0", "bundleDependencies": ["plugin"], "dependencies": {"plugin": "1.0.0"}},
        "node_modules/owner/node_modules/plugin": {"version": "1.0.0", "inBundle": true, "peerDependencies": {"react": "*", "tslib": "*"}, "peerDependenciesMeta": {"tslib": {"optional": true}}},
        "node_modules/owner/node_modules/tslib": {"version": "1.0.0", "inBundle": true},
        "node_modules/react": {"version": "19.0.0", "peer": true},
        "node_modules/tslib": {"version": "2.3.0", "peer": true}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let owner = &graph.packages["owner@1.0.0"];
    assert_eq!(
        owner.dependencies.get("react").map(String::as_str),
        Some("19.0.0")
    );
    assert!(!owner.dependencies.contains_key("tslib"));
    let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    assert_eq!(
        written["packages"]["node_modules/react"],
        original["packages"]["node_modules/react"]
    );
}

#[test]
fn npm_bundled_optional_peer_does_not_demote_required_owner_edge() {
    let original = serde_json::json!({"lockfileVersion": 3, "requires": true, "packages": {
        "": {"dependencies": {"owner": "1.0.0"}},
        "node_modules/owner": {"version": "1.0.0", "bundleDependencies": ["plugin"], "dependencies": {"plugin": "1.0.0", "react": "*"}},
        "node_modules/owner/node_modules/plugin": {"version": "1.0.0", "inBundle": true, "peerDependencies": {"react": "*"}, "peerDependenciesMeta": {"react": {"optional": true}}},
        "node_modules/react": {"version": "19.0.0"}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let owner = &graph.packages["owner@1.0.0"];
    assert_eq!(
        owner.dependencies.get("react").map(String::as_str),
        Some("19.0.0")
    );
    assert!(!owner.optional_dependencies.contains_key("react"));
}

#[test]
fn npm_required_bundled_peer_overrides_earlier_optional_bundled_peer() {
    let original = serde_json::json!({"lockfileVersion": 3, "requires": true, "packages": {
        "": {"dependencies": {"owner": "1.0.0"}},
        "node_modules/owner": {"version": "1.0.0", "bundleDependencies": ["a", "b"], "dependencies": {"a": "1.0.0", "b": "1.0.0"}},
        "node_modules/owner/node_modules/a": {"version": "1.0.0", "inBundle": true, "peerDependencies": {"react": "*"}, "peerDependenciesMeta": {"react": {"optional": true}}},
        "node_modules/owner/node_modules/b": {"version": "1.0.0", "inBundle": true, "peerDependencies": {"react": "*"}},
        "node_modules/react": {"version": "19.0.0", "peer": true}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let graph = parse(file.path()).unwrap();
    let owner = &graph.packages["owner@1.0.0"];
    assert!(owner.dependencies.contains_key("react"));
    assert!(!owner.optional_dependencies.contains_key("react"));
}

#[test]
fn npm_fresh_bundled_optional_dependencies_stay_optional() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut graph = LockfileGraph::default();
    graph.importers.insert(
        ".".into(),
        vec![DirectDep {
            name: "owner".into(),
            dep_path: "owner@1.0.0".into(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".into()),
        }],
    );
    graph.packages.insert(
        "owner@1.0.0".into(),
        LockedPackage {
            name: "owner".into(),
            version: "1.0.0".into(),
            dep_path: "owner@1.0.0".into(),
            bundled_dependencies: vec!["child".into()],
            declared_dependencies: [("child".into(), "^2.0.0".into())].into(),
            extra_meta: [(
                super::BUNDLED_OPTIONAL_KEY.to_string(),
                serde_json::json!({"child": "^2.0.0"}),
            )]
            .into(),
            ..Default::default()
        }
        .with_declared_dependencies_known(),
    );
    let manifest = aube_manifest::PackageJson {
        dependencies: [("owner".into(), "1.0.0".into())].into(),
        ..Default::default()
    };
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    let owner = &written["packages"]["node_modules/owner"];
    assert_eq!(owner["optionalDependencies"]["child"], "^2.0.0");
    assert!(owner["dependencies"].get("child").is_none());
}

proptest::proptest! {
    #[test]
    fn npm_retains_only_unchanged_optional_orphans(version in "[1-9]\\.[0-9]\\.[0-9]") {
        for change in ["none", "owner-removed", "owner-edge", "dependent-edge", "dependent-removed", "child-edge"] {
            let original = serde_json::json!({"lockfileVersion": 3, "packages": {
                "": {"optionalDependencies": {"owner": "1.0.0"}},
                "node_modules/owner": {"version": "1.0.0", "optional": true, "dependencies": {"dep": "1.0.0"}},
                "node_modules/dep": {"version": "1.0.0", "optional": true},
                "node_modules/owner/node_modules/child": {"version": version, "optional": true}
            }});
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), original.to_string()).unwrap();
            let mut graph = parse(file.path()).unwrap();
            // Resolution prunes the orphan; only the owner's imported metadata survives.
            let key = format!("child@{version}");
            if change == "child-edge" {
                graph.packages.get_mut(&key).unwrap().dependencies.insert("dep".into(), "1.0.0".into());
            } else {
                graph.packages.remove(&key);
            }
            match change {
                "owner-removed" => { graph.importers.get_mut(".").unwrap().clear(); },
                "owner-edge" => { graph.packages.get_mut("owner@1.0.0").unwrap().dependencies.clear(); },
                "dependent-removed" => { graph.packages.remove("dep@1.0.0"); },
                "dependent-edge" => { graph.packages.get_mut("dep@1.0.0").unwrap().declared_dependencies.insert("new".into(), "1.0.0".into()); },
                _ => {},
            }
            let manifest = serde_json::from_value(original["packages"][""].clone()).unwrap();
            write(file.path(), &graph, &manifest).unwrap();
            let written: serde_json::Value = serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
            proptest::prop_assert_eq!(written["packages"].get("node_modules/owner/node_modules/child").is_some(), change == "none", "{}", change);
        }
    }
}

proptest::proptest! {
    #[test]
    fn npm_preserves_root_engines(node in proptest::option::of(">=[0-9]{1,2}\\.[0-9]\\.[0-9]")) {
        let engines: BTreeMap<String, String> = node.into_iter().map(|version| ("node".into(), version)).collect();
        let manifest = aube_manifest::PackageJson { engines, ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package-lock.json");
        write(&file, &LockfileGraph::default(), &manifest).unwrap();
        let original = std::fs::read(&file).unwrap();
        let graph = parse(&file).unwrap();
        write(&file, &graph, &manifest).unwrap();
        proptest::prop_assert_eq!(std::fs::read(&file).unwrap(), original);
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let expected = if manifest.engines.is_empty() { serde_json::Value::Null } else { serde_json::json!(manifest.engines) };
        proptest::prop_assert_eq!(&written["packages"][""]["engines"], &expected);
    }
}

#[test]
fn review_round13_retained_orphan_does_not_shadow_newly_nested_dependent() {
    let original = serde_json::json!({"lockfileVersion": 3, "packages": {
        "": {"dependencies": {"mid": "1.0.0", "owner": "1.0.0"}},
        "node_modules/mid": {"version": "1.0.0", "dependencies": {"x": "2.0.0"}},
        "node_modules/owner": {"version": "1.0.0", "dependencies": {"mid": "1.0.0"}},
        "node_modules/owner/node_modules/x": {"version": "1.0.0", "optional": true},
        "node_modules/x": {"version": "2.0.0"}
    }});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), original.to_string()).unwrap();
    let mut graph = parse(file.path()).unwrap();
    assert!(graph.packages.contains_key("x@1.0.0"));
    graph.packages.remove("x@1.0.0");
    // Root upgrades its own mid to 3.0.0, pushing owner's mid@1 below owner.
    let mut mid3 = graph.packages["mid@1.0.0"].clone();
    mid3.version = "3.0.0".into();
    mid3.dep_path = "mid@3.0.0".into();
    mid3.dependencies.clear();
    mid3.declared_dependencies.clear();
    graph.packages.insert("mid@3.0.0".into(), mid3);
    for dep in graph.importers.get_mut(".").unwrap() {
        if dep.name == "mid" {
            dep.dep_path = "mid@3.0.0".into();
            dep.specifier = Some("3.0.0".into());
        }
    }
    let mut manifest: aube_manifest::PackageJson =
        serde_json::from_value(original["packages"][""].clone()).unwrap();
    manifest.dependencies.insert("mid".into(), "3.0.0".into());
    write(file.path(), &graph, &manifest).unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    assert_eq!(
        written["packages"]["node_modules/owner/node_modules/mid"]["version"],
        "1.0.0"
    );
    assert!(
        written["packages"]
            .get("node_modules/owner/node_modules/x")
            .is_none(),
        "retained x@1 shadows owner/node_modules/mid's x@2: {:#}",
        written["packages"]
    );
    assert_ne!(written["packages"]["node_modules/x"]["peer"], true);
    let reloaded = parse(file.path()).unwrap();
    assert_eq!(reloaded.packages["mid@1.0.0"].dependencies["x"], "2.0.0");
}

proptest::proptest! {
    #[test]
    fn npm_preserves_root_and_workspace_engine_forms(major in 18u8..30, form in 0u8..7) {
        let engines = match form {
            0 => serde_json::json!({"node": format!(">={major}")}),
            1 => serde_json::json!([format!("node >={major}")]),
            2 => serde_json::json!(format!("node >={major}")),
            3 => serde_json::json!([]),
            4 => serde_json::json!({}),
            5 => serde_json::json!(""),
            _ => serde_json::Value::Null,
        };
        let mut source = serde_json::json!({"name": "member", "version": "1.0.0", "engines": engines});
        let manifest: aube_manifest::PackageJson = serde_json::from_value(source.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let member = dir.path().join("packages/a");
        std::fs::create_dir_all(&member).unwrap();
        let member_file = member.join("package.json");
        std::fs::write(&member_file, source.to_string()).unwrap();
        let file = dir.path().join("package-lock.json");
        let mut graph = LockfileGraph::default();
        graph.importers.insert(".".into(), vec![]);
        graph.importers.insert("packages/a".into(), vec![]);
        write(&file, &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let expected = if form < 3 { engines } else { serde_json::Value::Null };
        for importer in ["", "packages/a"] {
            proptest::prop_assert_eq!(&written["packages"][importer]["engines"], &expected);
        }
        // Current manifests also remove stale metadata on the next write.
        let graph = parse(&file).unwrap();
        source.as_object_mut().unwrap().remove("engines");
        std::fs::write(&member_file, source.to_string()).unwrap();
        let manifest = serde_json::from_value(source).unwrap();
        write(&file, &graph, &manifest).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        for importer in ["", "packages/a"] {
            proptest::prop_assert!(written["packages"][importer].get("engines").is_none());
        }
    }
}

/// `deprecated` on a package-lock.json entry must survive a parse/write
/// cycle, and a freshly resolved package's recorded reason must be written.
#[test]
fn test_write_deprecated_round_trips_and_writes_new_entries() {
    let lock = r#"{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {
    "": {"name": "t", "version": "1.0.0", "dependencies": {"old": "1.0.0", "new": "1.0.0"}},
    "node_modules/old": {
      "version": "1.0.0", "resolved": "https://r/o.tgz", "integrity": "sha512-o",
      "deprecated": "use new instead"
    },
    "node_modules/new": {
      "version": "1.0.0", "resolved": "https://r/n.tgz", "integrity": "sha512-n"
    }
  }
}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("package-lock.json");
    std::fs::write(&path, lock).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("t".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("old".to_string(), "1.0.0".to_string()),
            ("new".to_string(), "1.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut graph = parse(&path).unwrap();
    graph
        .packages
        .get_mut("new@1.0.0")
        .unwrap()
        .extra_meta
        .insert("deprecated".to_string(), "fixture deprecation".into());
    write(&path, &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        json["packages"]["node_modules/old"]["deprecated"],
        "use new instead"
    );
    assert_eq!(
        json["packages"]["node_modules/new"]["deprecated"],
        "fixture deprecation"
    );
}

#[test]
fn test_write_absolute_directory_targets_relative_to_lockfile() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    for target in ["vendor/child", "../child"] {
        let directory = LocalSource::Directory(project.join(target));
        let directory_key = directory.dep_path("child");
        let spec = format!("file:{}", project.join(target).display());
        let graph = LockfileGraph {
            importers: BTreeMap::from([(
                ".".to_string(),
                vec![DirectDep {
                    name: "child".to_string(),
                    dep_path: directory_key.clone(),
                    dep_type: DepType::Production,
                    specifier: Some(spec.clone()),
                }],
            )]),
            packages: BTreeMap::from([(
                directory_key.clone(),
                LockedPackage {
                    name: "child".to_string(),
                    version: "1.0.0".to_string(),
                    dep_path: directory_key,
                    local_source: Some(directory),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let manifest = aube_manifest::PackageJson {
            dependencies: BTreeMap::from([("child".to_string(), spec.clone())]),
            ..Default::default()
        };
        let lock = project.join("package-lock.json");
        write(&lock, &graph, &manifest).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        let packages = value["packages"].as_object().unwrap();
        assert_eq!(packages[""]["dependencies"]["child"], spec);
        assert_eq!(packages["node_modules/child"]["resolved"], target);
        assert_eq!(packages["node_modules/child"]["link"], true);
        assert_eq!(packages[target]["version"], "1.0.0");
        assert!(packages.keys().all(|key| !Path::new(key).is_absolute()));
        let relative_lock = pathdiff::diff_paths(&lock, std::env::current_dir().unwrap()).unwrap();
        write(&relative_lock, &graph, &manifest).unwrap();
        let reread = parse(&lock).unwrap();
        let child = reread
            .packages
            .values()
            .find(|pkg| pkg.name == "child")
            .unwrap();
        assert_eq!(child.local_source, Some(LocalSource::Link(target.into())));
    }
}

fn workspace_members_lock(members: &[&str]) -> LockfileGraph {
    let mut packages = serde_json::Map::new();
    packages.insert(
        String::new(),
        serde_json::json!({"name": "root", "version": "1.0.0", "workspaces": ["packages/*"]}),
    );
    for member in members {
        packages.insert(
            format!("node_modules/{member}"),
            serde_json::json!({"resolved": format!("packages/{member}"), "link": true}),
        );
        packages.insert(
            format!("packages/{member}"),
            serde_json::json!({"version": "1.0.0"}),
        );
    }
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        serde_json::json!({"lockfileVersion": 3, "requires": true, "packages": packages})
            .to_string(),
    )
    .unwrap();
    parse(tmp.path()).unwrap()
}

fn member_manifest(json: &str) -> aube_manifest::PackageJson {
    serde_json::from_str(json).unwrap()
}

#[test]
fn test_drift_flags_new_dependency_free_workspace_member() {
    let graph = workspace_members_lock(&["a"]);
    let root = member_manifest(r#"{"name": "root", "workspaces": ["packages/*"]}"#);
    let check = |members: &[(&str, &str)]| {
        let mut manifests = vec![(".".to_string(), root.clone())];
        manifests.extend(
            members
                .iter()
                .map(|(path, json)| (path.to_string(), member_manifest(json))),
        );
        graph.check_drift_workspace_for_kind(
            &manifests,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
            LockfileKind::Npm,
        )
    };
    let a = ("packages/a", r#"{"name": "a", "version": "1.0.0"}"#);
    let b = ("packages/b", r#"{"name": "b", "version": "1.0.0"}"#);
    assert_eq!(check(&[a]), DriftStatus::Fresh);
    assert!(matches!(check(&[a, b]), DriftStatus::Stale { .. }));
}

/// `npm ci` links each member into the root `node_modules` by its
/// `name`. A member renamed in place keeps its importer path, so the
/// lockfile still records `packages/b` but links `b`, not `c`.
#[test]
fn test_drift_flags_workspace_member_renamed_in_place() {
    let graph = workspace_members_lock(&["a", "b"]);
    let root = member_manifest(r#"{"name": "root", "workspaces": ["packages/*"]}"#);
    let check = |b_name: &str| {
        graph.check_drift_workspace_for_kind(
            &[
                (".".to_string(), root.clone()),
                (
                    "packages/a".to_string(),
                    member_manifest(r#"{"name": "a", "version": "1.0.0"}"#),
                ),
                (
                    "packages/b".to_string(),
                    member_manifest(&format!(r#"{{"name": "{b_name}", "version": "1.0.0"}}"#)),
                ),
            ],
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
            LockfileKind::Npm,
        )
    };
    assert_eq!(check("b"), DriftStatus::Fresh);
    assert!(matches!(
        check("c"),
        DriftStatus::Stale { reason } if reason.contains("workspace member c")
    ));
}

#[test]
fn test_drift_flags_new_workspace_member_under_npm_shrinkwrap() {
    let graph = workspace_members_lock(&["a"]);
    let root = member_manifest(r#"{"name": "root", "workspaces": ["packages/*"]}"#);
    let a = member_manifest(r#"{"name": "a", "version": "1.0.0"}"#);
    let c = member_manifest(r#"{"name": "c", "version": "1.0.0"}"#);
    let check = |members: &[(&str, &aube_manifest::PackageJson)]| {
        let mut manifests = vec![(".".to_string(), root.clone())];
        manifests.extend(members.iter().map(|(p, m)| (p.to_string(), (*m).clone())));
        graph.check_drift_workspace_for_kind(
            &manifests,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
            LockfileKind::NpmShrinkwrap,
        )
    };
    assert_eq!(check(&[("packages/a", &a)]), DriftStatus::Fresh);
    assert!(matches!(
        check(&[("packages/a", &a), ("packages/c", &c)]),
        DriftStatus::Stale { .. }
    ));
}

#[test]
fn test_write_workspace_member_copies_os_cpu_and_omits_folder_name() {
    let graph = workspace_members_lock(&["a", "c"]);
    let root = member_manifest(r#"{"name": "root", "workspaces": ["packages/*"]}"#);
    let dir = tempfile::tempdir().unwrap();
    for (member, json) in [
        ("a", r#"{"name": "a", "version": "1.0.0"}"#),
        (
            "c",
            r#"{"name": "c", "version": "1.0.0", "os": ["linux", "darwin"], "cpu": "x64"}"#,
        ),
    ] {
        let member_dir = dir.path().join("packages").join(member);
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("package.json"), json).unwrap();
    }
    let path = dir.path().join("package-lock.json");
    write(&path, &graph, &root).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        written["packages"]["packages/a"],
        serde_json::json!({"version": "1.0.0"})
    );
    assert_eq!(
        written["packages"]["packages/c"],
        serde_json::json!({
            "version": "1.0.0",
            "os": ["linux", "darwin"],
            "cpu": ["x64"],
        })
    );
}

fn libc_graph() -> LockfileGraph {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "native@1.0.0".to_string(),
        LockedPackage {
            name: "native".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-native".to_string()),
            dep_path: "native@1.0.0".to_string(),
            libc: vec!["glibc".to_string()].into(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "native".to_string(),
            dep_path: "native@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".to_string()),
        }],
    );
    graph
}

fn written_libc(prior: Option<&str>, package_manager: Option<&str>) -> serde_json::Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("package-lock.json");
    if let Some(prior) = prior {
        std::fs::write(&path, prior).unwrap();
    }
    let mut manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("native".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    if let Some(pm) = package_manager {
        manifest
            .extra
            .insert("packageManager".to_string(), serde_json::json!(pm));
    }
    write(&path, &libc_graph(), &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    json["packages"]["node_modules/native"]
        .get("libc")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

const PRIOR_WITH_LIBC: &str = r#"{"lockfileVersion":3,"requires":true,"packages":{
    "":{"name":"root","version":"1.0.0"},
    "node_modules/other":{"version":"2.0.0","libc":["glibc"]}}}"#;
const PRIOR_WITHOUT_LIBC: &str = r#"{"lockfileVersion":3,"requires":true,"packages":{
    "":{"name":"root","version":"1.0.0"},
    "node_modules/native":{"version":"1.0.0"}}}"#;

#[test]
fn test_write_new_entry_libc_follows_existing_lockfile() {
    assert_eq!(
        written_libc(Some(PRIOR_WITH_LIBC), None),
        serde_json::json!(["glibc"])
    );
}

#[test]
fn test_write_new_entry_libc_follows_pinned_npm_version() {
    assert_eq!(
        written_libc(None, Some("npm@11.11.0")),
        serde_json::json!(["glibc"])
    );
    assert_eq!(
        written_libc(None, Some("npm@11.20.0+sha512.abc")),
        serde_json::json!(["glibc"])
    );
}

#[test]
fn test_write_new_entry_omits_libc_for_older_npm() {
    assert_eq!(
        written_libc(None, Some("npm@11.6.2")),
        serde_json::Value::Null
    );
    assert_eq!(written_libc(None, None), serde_json::Value::Null);
    assert_eq!(
        written_libc(None, Some("pnpm@11.20.0")),
        serde_json::Value::Null
    );
}

#[test]
fn test_write_existing_entry_keeps_its_libc_presence() {
    assert_eq!(
        written_libc(Some(PRIOR_WITHOUT_LIBC), Some("npm@11.20.0")),
        serde_json::Value::Null
    );
}

#[test]
fn test_write_new_entry_is_not_confused_with_alias_of_same_package() {
    let prior = r#"{"lockfileVersion":3,"requires":true,"packages":{
        "":{"name":"root","version":"1.0.0"},
        "node_modules/native-alias":{"name":"native","version":"1.0.0"}}}"#;
    assert_eq!(
        written_libc(Some(prior), Some("npm@11.20.0")),
        serde_json::json!(["glibc"])
    );
}

#[test]
fn test_parse_skips_extraneous_entries_outside_node_modules() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "test", "version": "1.0.0", "workspaces": ["packages/*"] },
                "../ext/b": { "version": "1.0.0", "extraneous": true },
                "node_modules/b": { "resolved": "packages/b", "link": true },
                "packages/b": { "version": "1.0.0" },
                "packages/gone": { "version": "1.0.0", "extraneous": true },
                "packages/gone/inner": { "name": "inner", "version": "1.0.0" },
                "packages/not_node_modules/gone": { "version": "1.0.0", "extraneous": true }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let mut names: Vec<_> = graph.packages.values().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["b"]);
}
