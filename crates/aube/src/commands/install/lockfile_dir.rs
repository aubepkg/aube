use aube_lockfile::LocalSource;
use aube_util::path::normalize_lexical;
use std::path::{Path, PathBuf};

fn remap_lockfile_importers(
    graph: &mut aube_lockfile::LockfileGraph,
    lockfile_dir: &Path,
    importer_key: &str,
    kind: aube_lockfile::LockfileKind,
) {
    if importer_key == "." || records_project_relative_importers(kind) {
        return;
    }
    if records_paths_like_pnpm(kind) {
        let project = project_dir(lockfile_dir, importer_key);
        remap_importer_paths(graph, lockfile_dir, &project);
        rebase_local_sources_between(graph, lockfile_dir, &project);
    } else if let Some(deps) = graph.importers.remove(importer_key) {
        graph.importers.insert(".".into(), deps);
    }
}

fn records_paths_like_pnpm(kind: aube_lockfile::LockfileKind) -> bool {
    matches!(
        kind,
        aube_lockfile::LockfileKind::Aube | aube_lockfile::LockfileKind::Pnpm
    )
}

fn records_project_relative_importers(kind: aube_lockfile::LockfileKind) -> bool {
    matches!(
        kind,
        aube_lockfile::LockfileKind::Npm
            | aube_lockfile::LockfileKind::NpmShrinkwrap
            | aube_lockfile::LockfileKind::Bun
    )
}

fn remap_importer_paths(graph: &mut aube_lockfile::LockfileGraph, from: &Path, to: &Path) {
    fn remap<T>(map: &mut std::collections::BTreeMap<String, T>, from: &Path, to: &Path) {
        *map = std::mem::take(map)
            .into_iter()
            .map(|(key, value)| {
                let path = rebase_relative(Path::new(&key), from, to).unwrap_or_else(|| key.into());
                let key = if path.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    path.to_string_lossy().replace('\\', "/")
                };
                (key, value)
            })
            .collect();
    }
    remap(&mut graph.importers, from, to);
    remap(&mut graph.skipped_optional_dependencies, from, to);
    remap(&mut graph.workspace_extra_fields, from, to);
}

/// The project directory that owns `importer_key` in `lockfile_dir`.
fn project_dir(lockfile_dir: &Path, importer_key: &str) -> PathBuf {
    normalize_lexical(&lockfile_dir.join(importer_key))
}

/// `path`, relative to `from`, re-expressed relative to `to`. `None` for
/// an absolute path, which means the same thing from anywhere.
fn rebase_relative(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    pathdiff::diff_paths(normalize_lexical(&from.join(path)), to).map(|p| normalize_lexical(&p))
}

/// `source` with its path replaced, keeping its kind.
fn with_path(source: &LocalSource, path: PathBuf) -> Option<LocalSource> {
    Some(match source {
        LocalSource::Directory(_) => LocalSource::Directory(path),
        LocalSource::Tarball(_) => LocalSource::Tarball(path),
        LocalSource::Portal(_) => LocalSource::Portal(path),
        LocalSource::Exec(_) => LocalSource::Exec(path),
        LocalSource::Link(_) | LocalSource::Git(_) | LocalSource::RemoteTarball(_) => return None,
    })
}

/// Re-express every relative local path in `graph` from directory `from`
/// to directory `to`. In the graph a pnpm-format lockfile reads into or
/// is written from, local paths are relative to the lockfile directory:
/// the reader turns an importer's `link:` version (relative to the
/// importer, as pnpm writes it) into a lockfile-relative path, and the
/// writer turns it back. The rest of the install resolves paths against
/// the project, so reading rebases from the lockfile directory to the
/// project and writing rebases the other way.
fn rebase_local_sources_between(graph: &mut aube_lockfile::LockfileGraph, from: &Path, to: &Path) {
    graph.rebase_local_sources(|_, source| {
        let rebased = rebase_relative(source.path()?, from, to)?;
        match source {
            LocalSource::Link(_) => Some(LocalSource::Link(rebased)),
            _ => with_path(source, rebased),
        }
    });
}

/// Read a relocated lockfile, restoring pnpm importer keys and local sources
/// to project-relative paths. Native npm/Bun importer keys stay project-relative.
pub(super) fn parse_lockfile_dir_remapped_with_kind_and_options(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    manifest: &aube_manifest::PackageJson,
    options: aube_lockfile::ParseOptions,
    selected: Option<aube_lockfile::LockfileKind>,
) -> Result<(aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind), aube_lockfile::Error> {
    let (mut graph, kind) = aube_lockfile::parse_lockfile_with_project_root(
        lockfile_dir,
        &project_dir(lockfile_dir, importer_key),
        manifest,
        options,
        selected,
    )?;
    remap_lockfile_importers(&mut graph, lockfile_dir, importer_key, kind);
    Ok((graph, kind))
}

/// Validate physical importer ownership before any resolver can prune the graph.
pub(super) fn guard_against_foreign_importers(
    lockfile_dir: &Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
    kind: aube_lockfile::LockfileKind,
    workspace_importers: &[String],
    frozen: bool,
) -> Result<(), aube_lockfile::Error> {
    let pnpm = records_paths_like_pnpm(kind);
    let project_relative = records_project_relative_importers(kind);
    let project = resolve_physical_path(&lockfile_dir.join(importer_key))
        .map_err(|err| aube_lockfile::Error::Parse(lockfile_dir.to_path_buf(), err.to_string()))?;
    // Importer keys are lexical; only ownership checks follow symlinks.
    let lexical_project = project_dir(lockfile_dir, importer_key);
    let physical_lockfile_dir = resolve_physical_path(lockfile_dir)
        .map_err(|err| aube_lockfile::Error::Parse(lockfile_dir.to_path_buf(), err.to_string()))?;
    // Native formats record an implicit project root, not a physical owner.
    let foreign_root = project_relative && lockfile_dir.join("package.json").exists();
    let owned_path = |key: &str| {
        let base = if pnpm { lockfile_dir } else { &project };
        resolve_physical_path(&base.join(key)).is_ok_and(|path| {
            path.starts_with(&project) && (!pnpm || path != physical_lockfile_dir)
        })
    };
    // An older aube kept members project-relative while the root was
    // lock-relative; those keys are the current workspace's own members.
    let legacy_member = |key: &str| {
        pnpm && key != "."
            && workspace_importers.iter().any(|member| member == key)
            && resolve_physical_path(&project.join(key))
                .is_ok_and(|path| path.starts_with(&project))
    };
    let local_importers = if matches!(
        kind,
        aube_lockfile::LockfileKind::Npm | aube_lockfile::LockfileKind::NpmShrinkwrap
    ) {
        let roots: Vec<_> = graph
            .importers
            .keys()
            .filter(|key| owned_path(key))
            .map(String::as_str)
            .collect();
        graph.npm_local_importers(&roots)
    } else {
        Default::default()
    };
    // Mixed coordinates also describe a stale member that moved. Mutable
    // installs can repair either case; only explain it when frozen drift fails.
    if frozen
        && pnpm
        && graph.importers.contains_key(importer_key)
        && workspace_importers.iter().any(|key| {
            if key == "." || !graph.importers.contains_key(key) {
                return false;
            }
            let expected = rebase_relative(Path::new(key), &lexical_project, lockfile_dir)
                .unwrap_or_else(|| key.into())
                .to_string_lossy()
                .replace('\\', "/");
            let recorded = rebase_relative(Path::new(key), lockfile_dir, &lexical_project)
                .unwrap_or_else(|| key.into())
                .to_string_lossy()
                .replace('\\', "/");
            !graph.importers.contains_key(&expected)
                && project.join(key).join("package.json").is_file()
                && !workspace_importers.contains(&recorded)
        })
    {
        return Err(aube_lockfile::Error::Parse(
            lockfile_dir.to_path_buf(),
            format!(
                "an older aube wrote mixed workspace coordinates in this lockfile; move the lockfile aside and regenerate it with `{} --no-frozen-lockfile`",
                aube_util::cmd("install"),
            ),
        ));
    }
    let foreign: Vec<&str> = graph
        .importers
        .keys()
        .map(String::as_str)
        .filter(|key| {
            if pnpm || project_relative {
                !owned_path(key) && !legacy_member(key) && !local_importers.contains(*key)
            } else {
                *key != importer_key
            }
        })
        .collect();
    if foreign.is_empty() && !foreign_root {
        return Ok(());
    }
    Err(aube_lockfile::Error::Parse(
        lockfile_dir.to_path_buf(),
        format!(
            "lockfile already records importers from other projects ({}); \
             aube does not support multi-project shared lockfiles outside a workspace. \
             Use a workspace, or point each project at its own `--lockfile-dir`.",
            if foreign_root {
                ".".into()
            } else {
                foreign.join(", ")
            }
        ),
    ))
}

// Resolve each existing component before interpreting a later `..`. Missing
// suffixes stay lexical so removed members can still be pruned.
fn resolve_physical_path(path: &Path) -> std::io::Result<PathBuf> {
    fn walk(path: &Path, symlinks: usize) -> std::io::Result<PathBuf> {
        if symlinks > 40 {
            return Err(std::io::Error::other("too many importer symlinks"));
        }
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::Normal(_) => {
                    resolved.push(component.as_os_str());
                    match std::fs::symlink_metadata(&resolved) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            let target = std::fs::read_link(&resolved)?;
                            resolved.pop();
                            resolved = walk(&resolved.join(target), symlinks + 1)?;
                        }
                        Ok(_) => resolved = std::fs::canonicalize(&resolved)?,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err),
                    }
                }
                _ => resolved.push(component.as_os_str()),
            }
        }
        Ok(resolved)
    }
    if path.is_absolute() {
        walk(path, 0)
    } else {
        walk(&std::env::current_dir()?.join(path), 0)
    }
}

/// Write a relocated lockfile using its format's importer and local-path
/// conventions.
pub(super) fn write_lockfile_dir_remapped(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: aube_lockfile::LockfileKind,
) -> Result<std::path::PathBuf, aube_lockfile::Error> {
    if importer_key == "." {
        return aube_lockfile::write_lockfile_as(lockfile_dir, graph, manifest, kind);
    }
    if !graph.importers.contains_key(".") {
        return Err(aube_lockfile::Error::Parse(
            lockfile_dir.to_path_buf(),
            format!(
                "in-memory lockfile graph missing `.` importer; cannot write under key `{importer_key}`"
            ),
        ));
    }
    let project = project_dir(lockfile_dir, importer_key);
    let mut remapped = graph.clone();
    if records_paths_like_pnpm(kind) {
        remap_importer_paths(&mut remapped, &project, lockfile_dir);
        rebase_local_sources_between(&mut remapped, &project, lockfile_dir);
        aube_lockfile::write_lockfile_as(lockfile_dir, &remapped, manifest, kind)
    } else if records_project_relative_importers(kind) {
        aube_lockfile::write_lockfile_as_with_project_root(
            lockfile_dir,
            &project,
            &remapped,
            manifest,
            kind,
        )
    } else {
        let deps = remapped.importers.remove(".").unwrap();
        remapped.importers.insert(importer_key.into(), deps);
        aube_lockfile::write_lockfile_as(lockfile_dir, &remapped, manifest, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join("lfd")
    }

    #[test]
    fn relocated_importer_guard_uses_paths_including_stale_members() {
        use aube_lockfile::{LockfileGraph, LockfileKind};
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        for locks in [project.join("locks"), dir.path().join("locks")] {
            std::fs::create_dir_all(&locks).unwrap();
            let root_key = rebase_relative(Path::new("."), &project, &locks).unwrap();
            let root_key = root_key.to_str().unwrap();
            let members = vec![".".into()];
            for kind in [
                LockfileKind::Aube,
                LockfileKind::Pnpm,
                LockfileKind::Npm,
                LockfileKind::NpmShrinkwrap,
                LockfileKind::Bun,
            ] {
                let pnpm = records_paths_like_pnpm(kind);
                let mut graph = LockfileGraph::default();
                for key in [".", "apps/removed", "apps/renamed", "excluded/app"] {
                    let key = if pnpm {
                        rebase_relative(Path::new(key), &project, &locks)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/")
                    } else {
                        key.into()
                    };
                    graph.importers.insert(key, vec![]);
                }
                assert!(
                    guard_against_foreign_importers(
                        &locks, root_key, &graph, kind, &members, false
                    )
                    .is_ok()
                );
                let foreign = if pnpm {
                    rebase_relative(Path::new("../other"), &project, &locks)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                } else {
                    "../other".into()
                };
                graph.importers.insert(foreign.clone(), vec![]);
                assert!(
                    guard_against_foreign_importers(
                        &locks, root_key, &graph, kind, &members, false
                    )
                    .is_err()
                );
                graph.importers.remove(&foreign);
                if pnpm {
                    graph.importers.insert(".".into(), vec![]);
                } else {
                    std::fs::write(locks.join("package.json"), "{}").unwrap();
                }
                assert!(
                    guard_against_foreign_importers(
                        &locks, root_key, &graph, kind, &members, false
                    )
                    .is_err()
                );
                if !pnpm {
                    std::fs::remove_file(locks.join("package.json")).unwrap();
                }
            }
        }
    }

    #[test]
    fn relocated_mixed_workspace_coordinates_have_migration_guidance() {
        use aube_lockfile::LockfileKind;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let locks = project.join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::create_dir_all(project.join("apps/app")).unwrap();
        std::fs::write(
            project.join("apps/app/package.json"),
            r#"{"name":"app","dependencies":{"is-number":"7.0.0"}}"#,
        )
        .unwrap();
        // Retained upstream 2.8.2 fixture: root is lock-relative, member is not.
        std::fs::write(
            locks.join("aube-lock.yaml"),
            include_str!("../../../tests/fixtures/relocated-mixed-workspace.yaml"),
        )
        .unwrap();
        let manifest = aube_manifest::PackageJson::default();
        let graph = aube_lockfile::parse_lockfile(&locks, &manifest).unwrap();
        for kind in [LockfileKind::Aube, LockfileKind::Pnpm] {
            for frozen in [false, true] {
                let result = guard_against_foreign_importers(
                    &locks,
                    "..",
                    &graph,
                    kind,
                    &[".".into(), "apps/app".into()],
                    frozen,
                );
                if frozen {
                    let message = result.unwrap_err().to_string();
                    assert!(message.contains("older aube wrote mixed workspace coordinates"));
                    assert!(message.contains("move the lockfile aside and regenerate"));
                } else {
                    result.unwrap();
                }
            }
        }
        // Mixed-looking keys can still be fresh when both members are current
        // and the missing member has no dependencies.
        std::fs::write(project.join("package.json"), r#"{"name":"root"}"#).unwrap();
        std::fs::write(project.join("apps/app/package.json"), r#"{"name":"app"}"#).unwrap();
        std::fs::create_dir_all(locks.join("apps/app")).unwrap();
        std::fs::write(locks.join("apps/app/package.json"), r#"{"name":"other"}"#).unwrap();
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert("..".into(), vec![]);
        graph.importers.insert("apps/app".into(), vec![]);
        guard_against_foreign_importers(
            &locks,
            "..",
            &graph,
            LockfileKind::Aube,
            &[".".into(), "apps/app".into(), "locks/apps/app".into()],
            true,
        )
        .unwrap();
    }

    #[test]
    fn relocated_mixed_workspace_coordinates_migrate_with_sibling_lock_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let locks = dir.path().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::create_dir_all(project.join("apps/app")).unwrap();
        std::fs::write(project.join("apps/app/package.json"), r#"{"name":"app"}"#).unwrap();
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert("../project".into(), vec![]);
        graph.importers.insert("apps/app".into(), vec![]);
        let members = [".".into(), "apps/app".into()];
        let guard = |graph: &aube_lockfile::LockfileGraph, frozen| {
            guard_against_foreign_importers(
                &locks,
                "../project",
                graph,
                aube_lockfile::LockfileKind::Aube,
                &members,
                frozen,
            )
        };
        guard(&graph, false).unwrap();
        assert!(
            guard(&graph, true)
                .unwrap_err()
                .to_string()
                .contains("older aube wrote mixed workspace coordinates")
        );
        graph.importers.insert("other".into(), vec![]);
        assert!(guard(&graph, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn relocated_physical_paths_resolve_symlinks_before_parent_components() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let outside = dir.path().join("outside/target");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, project.join("alias")).unwrap();
        symlink(project.join("missing"), project.join("internal")).unwrap();
        symlink(outside.join("missing"), project.join("external")).unwrap();
        symlink(&project, dir.path().join("workspace")).unwrap();
        for (path, expected) in [
            (
                project.join("alias/../ghost"),
                dir.path().join("outside/ghost"),
            ),
            (
                project.join("internal/stale"),
                project.join("missing/stale"),
            ),
            (
                project.join("external/stale"),
                outside.join("missing/stale"),
            ),
            (
                dir.path().join("workspace/alias/../ghost"),
                dir.path().join("outside/ghost"),
            ),
            (
                project.join("missing/../alias/../ghost"),
                dir.path().join("outside/ghost"),
            ),
        ] {
            assert_eq!(resolve_physical_path(&path).unwrap(), expected);
        }
        assert_eq!(
            resolve_physical_path(&dir.path().join("workspace")).unwrap(),
            project
        );
    }

    #[test]
    fn relocated_importer_paths_roundtrip_with_workspace_metadata() {
        let project = root().join("project");
        let locks = project.join("locks");
        let mut graph = aube_lockfile::LockfileGraph::default();
        for key in [".", "apps/app", "../sibling"] {
            graph.importers.insert(key.into(), vec![]);
            graph
                .skipped_optional_dependencies
                .insert(key.into(), Default::default());
            graph
                .workspace_extra_fields
                .insert(key.into(), Default::default());
        }
        remap_importer_paths(&mut graph, &project, &locks);
        assert_eq!(
            graph
                .importers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["..", "../../sibling", "../apps/app"]
        );
        assert_eq!(
            graph.importers.keys().collect::<Vec<_>>(),
            graph
                .skipped_optional_dependencies
                .keys()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.importers.keys().collect::<Vec<_>>(),
            graph.workspace_extra_fields.keys().collect::<Vec<_>>()
        );
        remap_importer_paths(&mut graph, &locks, &project);
        assert_eq!(
            graph
                .importers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [".", "../sibling", "apps/app"]
        );
    }

    #[test]
    fn rebases_between_the_project_and_a_lockfile_dir_below_or_above_it() {
        let project = root().join("proj");
        let below = project.join(".lock");
        assert_eq!(
            rebase_relative(Path::new("./vendor/foo"), &project, &below),
            Some(PathBuf::from("../vendor/foo"))
        );
        assert_eq!(
            rebase_relative(Path::new("../vendor/foo"), &below, &project),
            Some(PathBuf::from("vendor/foo"))
        );
        assert_eq!(
            rebase_relative(Path::new("vendor/foo"), &project, &root()),
            Some(PathBuf::from("proj/vendor/foo"))
        );
        assert_eq!(rebase_relative(&root(), &project, &below), None);
    }

    #[test]
    fn reading_rebases_every_local_path_from_the_lockfile_dir_to_the_project() {
        let project = root().join("proj");
        let lockfile_dir = project.join(".lock");
        // What the reader makes of importer `..` in `.lock`: `version:
        // link:other` (an override's target, while the specifier names
        // `vendor/bar`) joined to the importer, and `directory:
        // ../vendor/foo` as written. `vendor/foo`'s own `link:` to
        // `vendor/child` is stored lockfile-relative.
        let mut graph = aube_lockfile::LockfileGraph::default();
        for (name, path, specifier, direct) in [
            ("bar", "../other", Some("link:./vendor/bar"), true),
            ("foo", "../vendor/foo", Some("file:./vendor/foo"), true),
            ("child", "../vendor/child", None, false),
        ] {
            let source = if name == "foo" {
                LocalSource::Directory(PathBuf::from(path))
            } else {
                LocalSource::Link(PathBuf::from(path))
            };
            let dep_path = source.dep_path(name);
            if direct {
                graph.importers.entry(".".to_string()).or_default().push(
                    aube_lockfile::DirectDep {
                        name: name.to_string(),
                        dep_path: dep_path.clone(),
                        dep_type: aube_lockfile::DepType::Production,
                        specifier: specifier.map(str::to_string),
                    },
                );
            }
            graph.packages.insert(
                dep_path.clone(),
                aube_lockfile::LockedPackage {
                    name: name.to_string(),
                    dep_path,
                    local_source: Some(source),
                    ..Default::default()
                },
            );
        }

        rebase_local_sources_between(&mut graph, &lockfile_dir, &project);

        let sources: Vec<_> = graph
            .root_deps()
            .iter()
            .map(|dep| graph.packages[&dep.dep_path].local_source.clone())
            .collect();
        // The recorded target wins over the specifier.
        assert_eq!(
            sources,
            [
                Some(LocalSource::Link(PathBuf::from("other"))),
                Some(LocalSource::Directory(PathBuf::from("vendor/foo"))),
            ]
        );
        let child = LocalSource::Link(PathBuf::from("vendor/child"));
        assert_eq!(
            graph.packages[&child.dep_path("child")].local_source,
            Some(child)
        );
    }
}
