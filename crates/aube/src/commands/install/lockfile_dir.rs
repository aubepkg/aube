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
        rebase_local_sources_into_project(
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

/// Make a lockfile's local paths relative to the project, which the rest
/// of the install resolves them against. Like pnpm, the lockfile records
/// `file:` directories and tarballs relative to the lockfile directory and
/// `link:` targets relative to the project. The reader may already have
/// rebased a `link:` path it took for importer-relative, so derive those
/// from the project's own specifier instead.
fn rebase_local_sources_into_project(
    graph: &mut aube_lockfile::LockfileGraph,
    lockfile_dir: &Path,
    project_dir: &Path,
) {
    let links: std::collections::HashMap<String, LocalSource> = graph
        .root_deps()
        .iter()
        .filter_map(|dep| {
            let source = LocalSource::parse(dep.specifier.as_deref()?, project_dir)?;
            matches!(source, LocalSource::Link(_)).then(|| (dep.dep_path.clone(), source))
        })
        .collect();
    graph.rebase_local_sources(|dep_path, source| match source {
        LocalSource::Link(_) => links.get(dep_path).filter(|link| *link != source).cloned(),
        _ => with_path(
            source,
            rebase_relative(source.path()?, lockfile_dir, project_dir)?,
        ),
    });
}

/// The inverse of [`rebase_local_sources_into_project`] for writing: every
/// local path becomes relative to the lockfile directory, as the writer
/// expects. It records `file:` directories and tarballs that way and
/// re-expresses each `link:` target relative to its importer, the project.
fn rebase_local_sources_into_lockfile_dir(
    graph: &mut aube_lockfile::LockfileGraph,
    lockfile_dir: &Path,
    project_dir: &Path,
) {
    graph.rebase_local_sources(|_, source| {
        let rebased = rebase_relative(source.path()?, project_dir, lockfile_dir)?;
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
        rebase_local_sources_into_lockfile_dir(
            &mut remapped,
            lockfile_dir,
            &project_dir(lockfile_dir, importer_key),
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
    fn reading_rederives_link_targets_from_the_project_specifier() {
        let project = root().join("proj");
        let lockfile_dir = project.join(".lock");
        // What the reader makes of `version: link:./vendor/bar` under
        // importer `..`: it takes the path for importer-relative.
        let read_link = LocalSource::Link(PathBuf::from("../vendor/bar"));
        let read_dir = LocalSource::Directory(PathBuf::from("../vendor/foo"));
        let mut graph = aube_lockfile::LockfileGraph::default();
        for (name, source, specifier) in [
            ("bar", &read_link, "link:./vendor/bar"),
            ("foo", &read_dir, "file:./vendor/foo"),
        ] {
            let dep_path = source.dep_path(name);
            graph
                .importers
                .entry(".".to_string())
                .or_default()
                .push(aube_lockfile::DirectDep {
                    name: name.to_string(),
                    dep_path: dep_path.clone(),
                    dep_type: aube_lockfile::DepType::Production,
                    specifier: Some(specifier.to_string()),
                });
            graph.packages.insert(
                dep_path.clone(),
                aube_lockfile::LockedPackage {
                    name: name.to_string(),
                    dep_path,
                    local_source: Some(source.clone()),
                    ..Default::default()
                },
            );
        }

        rebase_local_sources_into_project(&mut graph, &lockfile_dir, &project);

        let sources: Vec<_> = graph
            .root_deps()
            .iter()
            .map(|dep| graph.packages[&dep.dep_path].local_source.clone())
            .collect();
        assert_eq!(
            sources,
            [
                Some(LocalSource::Link(PathBuf::from("./vendor/bar"))),
                Some(LocalSource::Directory(PathBuf::from("vendor/foo"))),
            ]
        );
    }
}
