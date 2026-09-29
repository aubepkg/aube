use aube_lockfile::LocalSource;
use aube_util::path::normalize_lexical;
use std::path::{Path, PathBuf};

fn remap_lockfile_importer(
    graph: &mut aube_lockfile::LockfileGraph,
    lockfile_dir: &Path,
    importer_key: &str,
    kind: aube_lockfile::LockfileKind,
) {
    if importer_key != "."
        && let Some(deps) = graph.importers.remove(importer_key)
    {
        graph.importers.insert(".".to_string(), deps);
        if !records_paths_like_pnpm(kind) {
            return;
        }
        rebase_local_sources_between(
            graph,
            lockfile_dir,
            &project_dir(lockfile_dir, importer_key),
        );
    }
}

/// Whether `kind` stores local paths the way pnpm does, which the rebasing
/// below follows. Other formats keep one root importer, so a relocated
/// lockfile of theirs never reaches the rebasing on read.
fn records_paths_like_pnpm(kind: aube_lockfile::LockfileKind) -> bool {
    matches!(
        kind,
        aube_lockfile::LockfileKind::Aube | aube_lockfile::LockfileKind::Pnpm
    )
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

/// Read a lockfile from `lockfile_dir`, preserve the detected kind,
/// and remap its importer key for the current project from the
/// project's relative-path key to `"."`. No-op when
/// `importer_key == "."`.
pub(super) fn parse_lockfile_dir_remapped_with_kind_and_options(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    manifest: &aube_manifest::PackageJson,
    options: aube_lockfile::ParseOptions,
    selected: Option<aube_lockfile::LockfileKind>,
) -> Result<(aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind), aube_lockfile::Error> {
    let (mut graph, kind) = aube_lockfile::parse_lockfile_with_kind_and_options_selecting(
        lockfile_dir,
        manifest,
        options,
        selected,
    )?;
    remap_lockfile_importer(&mut graph, lockfile_dir, importer_key, kind);
    Ok((graph, kind))
}

/// Refuse to operate on a `--lockfile-dir` lockfile that already
/// records other importers besides the current project. This PR
/// scopes `--lockfile-dir` to single-project relocation; multi-
/// project shared lockfiles need workspace coordination (resolve
/// every importer's deps in one pass, prune packages by union of all
/// importers) which is out of scope. Without this guard, a second
/// project pointed at the same dir would silently orphan-strip the
/// first project's package entries on the next install. Loud-fail
/// here so the user can move to a workspace setup or pick a
/// different `lockfileDir`.
pub(super) fn guard_against_foreign_importers(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<(), aube_lockfile::Error> {
    // Caller gates on `importer_key != "."`, so any `"."` entry on
    // disk is itself a project that ran `aube install` directly in
    // `lockfile_dir` without `--lockfile-dir`. That entry would be
    // dropped on write, so it counts as foreign.
    let foreign: Vec<&str> = graph
        .importers
        .keys()
        .map(String::as_str)
        .filter(|k| *k != importer_key)
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    Err(aube_lockfile::Error::Parse(
        lockfile_dir.to_path_buf(),
        format!(
            "lockfile already records importers from other projects ({}); \
             aube does not yet support multi-project shared lockfiles outside a workspace. \
             Use a `pnpm-workspace.yaml` workspace, or point each project at its own `--lockfile-dir`.",
            foreign.join(", ")
        ),
    ))
}

/// Write `graph` to `lockfile_dir`, remapping the project's `"."`
/// importer key to its relative-path key from `lockfile_dir`.
/// No-op remap when `importer_key == "."`.
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
    let mut remapped = graph.clone();
    let deps = remapped.importers.remove(".").ok_or_else(|| {
        aube_lockfile::Error::Parse(
            lockfile_dir.to_path_buf(),
            format!(
                "in-memory lockfile graph missing `.` importer; cannot write under key `{importer_key}`"
            ),
        )
    })?;
    remapped.importers.insert(importer_key.to_string(), deps);
    if records_paths_like_pnpm(kind) {
        rebase_local_sources_between(
            &mut remapped,
            &project_dir(lockfile_dir, importer_key),
            lockfile_dir,
        );
    }
    aube_lockfile::write_lockfile_as(lockfile_dir, &remapped, manifest, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join("lfd")
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
