use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) type PkgJsonCache = BTreeMap<String, Option<serde_json::Value>>;

/// Per-install cache of workspace-package `package.json` reads. Keyed
/// by the workspace dir on disk so a popular tooling package consumed
/// by many importers gets read and parsed once, not once per consumer.
pub(crate) type WsPkgJsonCache = BTreeMap<PathBuf, Option<serde_json::Value>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedBinEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
    Other,
}

/// What a linking pass does when the command name it is about to write
/// is already present in the target `.bin/`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BinConflict {
    /// Write the shim unconditionally. Used by the passes whose entries
    /// are authoritative for a `.bin/`: the importer's direct deps, the
    /// importer's own `bin`, and the isolated per-dep pass.
    Overwrite,
    /// Yield to a command another package claimed earlier *in this run*,
    /// so a deep dependency can't shadow one of the importer's own.
    ///
    /// A shim left behind by an earlier install is deliberately *not* a
    /// claim. `.bin/` is never pruned, so treating on-disk state as
    /// ownership would let a command whose owner was removed or replaced
    /// keep pointing at the old package forever. Overwriting it is how
    /// an incremental install reconciles the directory.
    YieldToClaimed,
    /// As [`BinConflict::YieldToClaimed`], but also yield to a command
    /// already on disk that still resolves. For passes that run *without*
    /// the importer passes — `aube rebuild` re-emits dependency shims
    /// against an already-linked tree — where this run's claims don't
    /// include the importers' direct deps and the tree on disk is the
    /// only record of who owns what. A command that no longer resolves
    /// is stale either way and gets rewritten.
    YieldToClaimedOrLive,
}

/// Exact shim files created during the pre-lifecycle linking pass, keyed by
/// their `.bin` directory and command name. Snapshots distinguish unchanged
/// Aube output from lifecycle-produced replacements on every platform.
#[derive(Debug, Default)]
pub(crate) struct ManagedBinLinks {
    entries: BTreeMap<PathBuf, BTreeMap<String, BTreeMap<PathBuf, ManagedBinEntry>>>,
    seen: BTreeMap<PathBuf, BTreeSet<String>>,
    capture: bool,
}

impl ManagedBinLinks {
    pub(crate) fn capturing() -> Self {
        Self {
            capture: true,
            ..Default::default()
        }
    }
}
pub(crate) type PreservedBinLinks = BTreeMap<PathBuf, BTreeSet<String>>;

pub(crate) struct LinkDepBinsInput<'a> {
    pub(crate) aube_dir: &'a Path,
    pub(crate) graph: &'a aube_lockfile::LockfileGraph,
    pub(crate) virtual_store_dir_max_length: usize,
    pub(crate) placements: Option<&'a aube_linker::HoistedPlacements>,
    pub(crate) shim_opts: aube_linker::BinShimOptions<'a>,
    pub(crate) cache: &'a mut PkgJsonCache,
    pub(crate) managed: &'a mut ManagedBinLinks,
    pub(crate) preserved: Option<&'a PreservedBinLinks>,
    /// Whether the importer passes (`link_bins` and friends) already ran
    /// against this `ManagedBinLinks` in this run. `install` links the
    /// importers' direct deps first, so their commands are claimed by
    /// the time the hoisted pass runs; `rebuild` calls this pass on its
    /// own and has to read ownership off the tree instead.
    pub(crate) importer_bins_linked: bool,
}

pub(super) struct LinkAllBinsInput<'a> {
    pub(super) project_dir: &'a Path,
    pub(super) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(super) modules_dir_name: &'a str,
    pub(super) aube_dir: &'a Path,
    pub(super) graph: &'a aube_lockfile::LockfileGraph,
    pub(super) virtual_store_dir_max_length: usize,
    pub(super) placements: Option<&'a aube_linker::HoistedPlacements>,
    pub(super) ws_dirs: &'a BTreeMap<String, PathBuf>,
    pub(super) manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(super) manifest: &'a aube_manifest::PackageJson,
    pub(super) node_linker: aube_linker::NodeLinker,
    pub(super) has_workspace: bool,
    pub(super) link_dependency_bins: bool,
    pub(super) capture_managed: bool,
    pub(super) preserved: Option<&'a PreservedBinLinks>,
}

/// Link bin entries from packages to node_modules/.bin/
/// Compute the on-disk directory a dep's materialized package lives
/// in. Matches the path `aube-linker` writes under
/// `node_modules/.aube/<escaped dep_path>/node_modules/<name>`.
///
/// `virtual_store_dir_max_length` must match the value the linker
/// was built with (see `install::run` for the single source of
/// truth) — otherwise long `dep_path`s that trigger the
/// truncate-and-hash fallback inside `dep_path_to_filename` will
/// encode to a different filename than the one the linker wrote,
/// and this function will return a path that doesn't exist.
pub(crate) fn materialized_pkg_dir(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> std::path::PathBuf {
    // In hoisted mode the package was materialized directly into
    // `node_modules/<...>/<name>/` and its path is recorded in
    // `placements`. Fall back to the isolated `.aube/<dep_path>`
    // convention when either the mode is isolated (`placements` is
    // `None`) or the hoisted planner didn't place this specific
    // dep_path (e.g. filtered by `--prod` / `--no-optional`).
    // `aube_dir` is the resolved `virtualStoreDir` — the install
    // driver threads it in via `commands::resolve_virtual_store_dir`
    // so a custom override lands on the same path the linker wrote
    // to.
    if let Some(placements) = placements
        && let Some(p) = placements.package_dir(dep_path)
    {
        return p.to_path_buf();
    }
    aube_dir
        .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .join("node_modules")
        .join(name)
}

/// Directory holding the dep's own `node_modules/` — i.e. the dir
/// that contains both `<name>` and its sibling symlinks. For scoped
/// packages (`@scope/name`) `package_dir` is two levels below that
/// `node_modules/`, so we strip the extra `@scope` hop. Used to
/// locate the per-dep `.bin/` for transitive lifecycle-script bins.
pub(crate) fn dep_modules_dir_for(package_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    if name.starts_with('@') {
        package_dir
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    } else {
        package_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    }
}

/// Read a dep's `package.json` from its materialized directory.
///
/// Earlier revisions of this file went through
/// `package_indices[dep_path]` and read
/// `stored.store_path.join("package.json")` from the CAS. That
/// stopped working once `fetch_packages_with_root` learned to skip
/// `load_index` for packages whose `.aube/<dep_path>` already exists
/// (the `AlreadyLinked` fast path) — the indices map is sparse on
/// warm installs, and every caller that reached for
/// `package_indices.get(..)?.get("package.json")` silently dropped
/// those deps via the `continue` or `?` on the missing key.
///
/// Read the hardlinked file at the materialized location instead:
/// same bytes, zero dependency on the sparse indices map, and
/// doesn't require a cache miss to surface when the virtual store is
/// intact.
///
/// Error policy: `Ok(None)` only when the file is legitimately
/// missing (e.g. a package that ships without a top-level
/// `package.json`, or hasn't been materialized yet). Every other
/// `std::io::Error` — permission denied, short reads, disk errors —
/// bubbles up as `Err` so the user sees a real failure instead of a
/// silently dropped bin link. Parse errors likewise propagate.
fn read_materialized_pkg_json(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    let pkg_json_path = pkg_dir.join("package.json");
    let content = match std::fs::read_to_string(&pkg_json_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(miette!(
                "failed to read package.json for {name} at {}: {e}",
                pkg_json_path.display()
            ));
        }
    };
    let value = aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
        .map_err(miette::Report::new)
        .wrap_err_with(|| format!("failed to parse package.json for {name}"))?;
    Ok(Some(value))
}

#[allow(clippy::too_many_arguments)]
fn read_materialized_pkg_json_cached(
    cache: &mut PkgJsonCache,
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    if let Some(value) = cache.get(dep_path) {
        return Ok(value.clone());
    }
    let value = read_materialized_pkg_json(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )?;
    cache.insert(dep_path.to_string(), value.clone());
    Ok(value)
}

/// Create top-level + bundled bin symlinks for one dep. Extracted so
/// both the root-importer pass (`link_bins`) and the per-workspace
/// loop use the same code path.
#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins_for_dep(
    cache: &mut PkgJsonCache,
    aube_dir: &std::path::Path,
    bin_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    if let Some(pkg_json) = read_materialized_pkg_json_cached(
        cache,
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )? && let Some(bin) = pkg_json.get("bin")
    {
        link_bin_entries(
            bin_dir,
            &pkg_dir,
            Some(name),
            bin,
            shim_opts,
            managed,
            preserved,
            BinConflict::Overwrite,
        )?;
    }
    link_bundled_bins(
        bin_dir,
        &pkg_dir,
        graph,
        dep_path,
        shim_opts,
        managed,
        preserved,
        BinConflict::Overwrite,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    ws_dirs: Option<&BTreeMap<String, PathBuf>>,
    ws_cache: &mut WsPkgJsonCache,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let bin_dir = project_dir.join(modules_dir_name).join(".bin");
    std::fs::create_dir_all(&bin_dir).into_diagnostic()?;

    for dep in graph.root_deps() {
        if let Some(ws_dir) = ws_dirs.and_then(|m| m.get(&dep.name)) {
            link_bins_for_workspace_dep(
                ws_cache, &bin_dir, ws_dir, &dep.name, shim_opts, managed, preserved,
            )?;
        } else {
            link_bins_for_dep(
                cache,
                aube_dir,
                &bin_dir,
                graph,
                &dep.dep_path,
                &dep.name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
                managed,
                preserved,
            )?;
        }
    }

    Ok(())
}

/// Link bins declared by a `workspace:` dep into the importer's
/// `.bin/`. Workspace deps don't get a `.aube/<dep_path>/` materialization
/// (the linker symlinks them straight into the importer's `node_modules/`),
/// so `link_bins_for_dep` finds nothing on disk and silently skips. Read
/// the workspace package's own `package.json` and shim each bin entry,
/// matching pnpm's behavior of exposing workspace bins to dependent
/// packages' npm scripts.
///
/// `cache` deduplicates the read+parse across importers — without it,
/// a popular tooling package consumed by N workspace members gets its
/// `package.json` read N times during a single install.
pub(super) fn link_bins_for_workspace_dep(
    cache: &mut WsPkgJsonCache,
    bin_dir: &Path,
    ws_dir: &Path,
    name: &str,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let pkg_json = if let Some(cached) = cache.get(ws_dir) {
        cached.clone()
    } else {
        let pkg_json_path = ws_dir.join("package.json");
        let parsed = match std::fs::read_to_string(&pkg_json_path) {
            Ok(content) => Some(
                aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| {
                        format!("failed to parse package.json for workspace dep {name}")
                    })?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for workspace dep {name} at {}: {e}",
                    pkg_json_path.display()
                ));
            }
        };
        cache.insert(ws_dir.to_path_buf(), parsed.clone());
        parsed
    };
    if let Some(pkg_json) = pkg_json
        && let Some(bin) = pkg_json.get("bin")
    {
        link_bin_entries(
            bin_dir,
            ws_dir,
            Some(name),
            bin,
            shim_opts,
            managed,
            preserved,
            BinConflict::Overwrite,
        )?;
    }
    Ok(())
}

/// Write per-dep `.bin/` directories holding shims for each package's
/// *own* declared dependencies. Mirrors pnpm's post-link pass that
/// populates `node_modules/.pnpm/<dep_path>/node_modules/.bin/`.
///
/// Without this, a dep's lifecycle script (e.g. `unrs-resolver`'s
/// postinstall that calls `prebuild-install`) can't find transitive
/// binaries on PATH — the project-level `node_modules/.bin` only holds
/// shims for the root's *direct* deps. `run_dep_hook` prepends the
/// dep-local `.bin` (via `dep_modules_dir_for`) before the
/// project-level one so the dep's own transitive bins always win.
///
/// Hoisted trees have no per-dep `node_modules/` to hang shims off, so
/// they take the `link_hoisted_dep_bins` path instead.
pub(crate) fn link_dep_bins(input: LinkDepBinsInput<'_>) -> miette::Result<()> {
    let LinkDepBinsInput {
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        cache,
        managed,
        preserved,
        importer_bins_linked,
    } = input;
    if let Some(placements) = placements {
        return link_hoisted_dep_bins(
            aube_dir,
            graph,
            virtual_store_dir_max_length,
            placements,
            shim_opts,
            cache,
            managed,
            preserved,
            if importer_bins_linked {
                BinConflict::YieldToClaimed
            } else {
                BinConflict::YieldToClaimedOrLive
            },
        );
    }
    for (dep_path, pkg) in &graph.packages {
        if pkg.dependencies.is_empty() {
            continue;
        }
        let pkg_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !pkg_dir.exists() {
            // Filtered by optional / platform guards, or a staging
            // hiccup. Skipping avoids blowing up the whole install on
            // a dep that was never materialized.
            continue;
        }
        let dep_modules_dir = dep_modules_dir_for(&pkg_dir, &pkg.name);
        let bin_dir = dep_modules_dir.join(".bin");
        // Don't `create_dir_all(&bin_dir)` here — most deps have
        // no child that ships a `bin`, and an eager mkdir would leave
        // empty `.bin/` directories everywhere. `create_bin_link`
        // materializes the parent the first time a shim actually
        // lands, so deps whose children contribute zero shims stay
        // empty on disk.

        for (child_name, child_version) in &pkg.dependencies {
            // Mirror the linker's self-ref guard from
            // `materialize_into`: a package that depends on its own
            // dep_path is a graph artefact, not a real edge.
            let child_dep_path = format!("{child_name}@{child_version}");
            if child_dep_path == *dep_path && child_name == &pkg.name {
                continue;
            }
            // The sibling may have been filtered (optional on another
            // platform); `link_bins_for_dep` already returns Ok when
            // the target pkg_json is absent, so just call through.
            link_bins_for_dep(
                cache,
                aube_dir,
                &bin_dir,
                graph,
                &child_dep_path,
                child_name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
                managed,
                preserved,
            )?;
        }
    }
    Ok(())
}

/// Hoisted counterpart of [`link_dep_bins`].
///
/// The hoisted layout writes real package directories into
/// `node_modules/`, nesting only where a version conflict forces it, so
/// there is no `.aube/<dep_path>/node_modules/.bin/` to hang per-dep
/// shims off. npm solves the same problem by linking each package's
/// *own* bins into the `.bin/` of the `node_modules/` directory that
/// package sits in — which is exactly the directory `run_dep_hook`
/// prepends to `PATH` (see `dep_modules_dir_for`). This pass does the
/// same, for every placement site of every package in the tree.
///
/// Without it only the importers' *direct* deps reached `node_modules/.bin`,
/// so a dep whose install script shells out to one of its own dependencies
/// died with `command not found` — `bcrypt` calling `node-pre-gyp` from
/// `@mapbox/node-pre-gyp` is the reported case (Discussion #1543), and
/// `prebuild-install` / `napi-postinstall` fail the same way.
///
/// On a collision the command stays with whoever claimed it first: the
/// importer passes run before this one and use `BinConflict::Overwrite`,
/// so a direct dependency always beats a transitive package shipping the
/// same name, and among transitives the graph order decides. A shim left
/// over from an earlier install is not an owner — `.bin/` is never
/// pruned, so rewriting it is what keeps a command pointing at the
/// package that owns it today. See [`BinConflict`] for the `rebuild`
/// variant, which has no importer pass to defer to.
#[allow(clippy::too_many_arguments)]
fn link_hoisted_dep_bins(
    aube_dir: &Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: &aube_linker::HoistedPlacements,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
    conflict: BinConflict,
) -> miette::Result<()> {
    for (dep_path, pkg) in &graph.packages {
        // Every copy of a package gets its own `.bin/` entry: a name
        // conflict duplicates the package under the dependents that
        // forced the nesting, and each copy is reachable only from its
        // own subtree.
        let placed_dirs = placements.all_package_dirs(dep_path);
        if placed_dirs.is_empty() {
            // Filtered by `--prod` / `--no-optional` / a platform guard,
            // so nothing was materialized to link against.
            continue;
        }
        // All copies share one set of bytes, so read and parse the
        // `package.json` once per dep_path rather than once per site.
        let pkg_json = read_materialized_pkg_json_cached(
            cache,
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            Some(placements),
        )?;
        for pkg_dir in placed_dirs {
            let bin_dir = dep_modules_dir_for(pkg_dir, &pkg.name).join(".bin");
            if let Some(pkg_json) = &pkg_json
                && let Some(bin) = pkg_json.get("bin")
            {
                link_bin_entries(
                    &bin_dir,
                    pkg_dir,
                    Some(&pkg.name),
                    bin,
                    shim_opts,
                    managed,
                    preserved,
                    conflict,
                )?;
            }
            // A bundled dep lives at `<pkg_dir>/node_modules/<name>`,
            // which no `.bin/` on the lifecycle `PATH` covers. Expose it
            // alongside its host so the host's own install script can
            // still invoke it, matching what the isolated pass does for
            // a bundling child.
            link_bundled_bins(
                &bin_dir, pkg_dir, graph, dep_path, shim_opts, managed, preserved, conflict,
            )?;
        }
    }
    Ok(())
}

/// Link every bin surface exposed by an install.
///
/// This runs before dependency lifecycle scripts so builds can invoke their
/// dependencies, then again after approved builds. The second pass refreshes
/// packages whose lifecycle replaces a bin target.
pub(super) fn link_all_bins(input: LinkAllBinsInput<'_>) -> miette::Result<ManagedBinLinks> {
    let LinkAllBinsInput {
        project_dir,
        settings_ctx,
        modules_dir_name,
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        ws_dirs,
        manifests,
        manifest,
        node_linker,
        has_workspace,
        link_dependency_bins,
        capture_managed,
        preserved,
    } = input;

    let extend_node_path = aube_settings::resolved::extend_node_path(settings_ctx);
    let isolated = !matches!(node_linker, aube_linker::NodeLinker::Hoisted);
    let prefer_symlinked_executables =
        aube_settings::resolved::prefer_symlinked_executables(settings_ctx)
            .or(isolated.then_some(false));
    let hidden_modules_dir = aube_dir.join("node_modules");
    let shim_opts = aube_linker::BinShimOptions {
        extend_node_path,
        prefer_symlinked_executables,
        hidden_modules_dir: isolated.then_some(hidden_modules_dir.as_path()),
    };

    let mut pkg_json_cache = PkgJsonCache::new();
    let mut ws_pkg_json_cache = WsPkgJsonCache::new();
    let mut managed = if capture_managed {
        ManagedBinLinks::capturing()
    } else {
        ManagedBinLinks::default()
    };
    let ws_dirs_for_bins = has_workspace.then_some(ws_dirs);
    link_bins(
        project_dir,
        modules_dir_name,
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        &mut pkg_json_cache,
        ws_dirs_for_bins,
        &mut ws_pkg_json_cache,
        &mut managed,
        preserved,
    )?;

    // Root self-bins override dependency bins with the same name. Force a
    // wrapper because generated output may not exist yet or be executable.
    if let Some(bin) = manifest.extra.get("bin") {
        let root_bin_dir = project_dir.join(modules_dir_name).join(".bin");
        let self_shim_opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..shim_opts
        };
        link_bin_entries(
            &root_bin_dir,
            project_dir,
            manifest.name.as_deref(),
            bin,
            self_shim_opts,
            &mut managed,
            preserved,
            BinConflict::Overwrite,
        )?;
    }

    if has_workspace {
        for (importer_path, deps) in &graph.importers {
            if importer_path == "." || !aube_linker::is_physical_importer(importer_path) {
                continue;
            }
            let pkg_dir = project_dir.join(importer_path);
            let bin_dir = pkg_dir.join(modules_dir_name).join(".bin");
            std::fs::create_dir_all(&bin_dir).into_diagnostic()?;
            for dep in deps {
                if let Some(ws_dir) = ws_dirs.get(&dep.name) {
                    link_bins_for_workspace_dep(
                        &mut ws_pkg_json_cache,
                        &bin_dir,
                        ws_dir,
                        &dep.name,
                        shim_opts,
                        &mut managed,
                        preserved,
                    )?;
                } else {
                    link_bins_for_dep(
                        &mut pkg_json_cache,
                        aube_dir,
                        &bin_dir,
                        graph,
                        &dep.dep_path,
                        &dep.name,
                        virtual_store_dir_max_length,
                        placements,
                        shim_opts,
                        &mut managed,
                        preserved,
                    )?;
                }
            }
            if let Some((_, member_manifest)) =
                manifests.iter().find(|(path, _)| path == importer_path)
                && let Some(bin) = member_manifest.extra.get("bin")
            {
                let self_shim_opts = aube_linker::BinShimOptions {
                    prefer_symlinked_executables: Some(false),
                    ..shim_opts
                };
                link_bin_entries(
                    &bin_dir,
                    &pkg_dir,
                    member_manifest.name.as_deref(),
                    bin,
                    self_shim_opts,
                    &mut managed,
                    preserved,
                    BinConflict::Overwrite,
                )?;
            }
        }
    }

    if link_dependency_bins {
        link_dep_bins(LinkDepBinsInput {
            aube_dir,
            graph,
            virtual_store_dir_max_length,
            placements,
            shim_opts,
            cache: &mut pkg_json_cache,
            managed: &mut managed,
            preserved,
            importer_bins_linked: true,
        })?;
    }
    Ok(managed)
}

/// Remove only shims that still match entries created by the pre-build pass.
/// Lifecycle-produced files or retargeted symlinks are left untouched.
pub(crate) fn remove_managed_bin_links(
    managed: &ManagedBinLinks,
) -> miette::Result<PreservedBinLinks> {
    let mut preserved = PreservedBinLinks::new();
    for (bin_dir, entries) in &managed.entries {
        for (name, expected_files) in entries {
            let mut matching = Vec::new();
            let mut replaced = false;
            for (path, expected) in expected_files {
                match read_managed_bin_entry(path)? {
                    Some(current) if current == *expected => matching.push(path),
                    Some(_) | None => replaced = true,
                }
            }
            if replaced {
                preserved
                    .entry(bin_dir.clone())
                    .or_default()
                    .insert(name.clone());
            } else {
                // A command can be a family of launchers on Windows
                // (`name`, `name.cmd`, and `name.ps1`). If a lifecycle
                // script replaces any member, keep the unchanged siblings
                // too: the relink pass preserves the whole command, and
                // deleting only its matching members would make it
                // unavailable from some shells.
                for path in matching {
                    std::fs::remove_file(path).into_diagnostic()?;
                }
            }
        }
    }
    Ok(preserved)
}

/// Remove preserved command families that are no longer declared by the
/// post-lifecycle package manifests. Commands encountered by the relink pass
/// stay preserved, including any intentionally replaced or deleted launcher.
pub(crate) fn remove_unclaimed_preserved_bin_links(
    managed: &ManagedBinLinks,
    preserved: &PreservedBinLinks,
    relinked: &ManagedBinLinks,
) -> miette::Result<()> {
    for (bin_dir, names) in preserved {
        for name in names {
            if relinked
                .seen
                .get(bin_dir)
                .is_some_and(|seen| seen.contains(name))
            {
                continue;
            }
            let Some(expected_files) = managed
                .entries
                .get(bin_dir)
                .and_then(|entries| entries.get(name))
            else {
                continue;
            };
            for path in expected_files.keys() {
                match std::fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        std::fs::remove_dir_all(path).into_diagnostic()?;
                    }
                    Ok(_) => std::fs::remove_file(path).into_diagnostic()?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).into_diagnostic(),
                }
            }
        }
    }
    Ok(())
}

fn read_managed_bin_entry(path: &Path) -> miette::Result<Option<ManagedBinEntry>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).into_diagnostic(),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::read_link(path)
            .map(ManagedBinEntry::Symlink)
            .map(Some)
            .into_diagnostic();
    }
    if metadata.is_file() {
        return std::fs::read(path)
            .map(ManagedBinEntry::File)
            .map(Some)
            .into_diagnostic();
    }
    Ok(Some(ManagedBinEntry::Other))
}

/// Whether any launcher for `name` in `bin_dir` still *resolves*.
///
/// Follows symlinks on purpose: under `nodeLinker=hoisted` a `.bin/`
/// entry defaults to a symlink into the package directory, so a package
/// that left the tree leaves a dangling link behind. Treating that as a
/// live command would let it block whichever package owns the name now.
///
/// A wrapper-script launcher (Windows, or `preferSymlinkedExecutables=false`)
/// is an ordinary file, so this can only report that it exists — a stale
/// one still reads as live. That is the conservative direction, and the
/// install path doesn't rely on this check at all.
fn bin_command_resolves(bin_dir: &Path, name: &str) -> bool {
    bin_link_paths(bin_dir, name)
        .iter()
        .any(|path| std::fs::metadata(path).is_ok())
}

fn bin_link_paths(bin_dir: &Path, name: &str) -> Vec<PathBuf> {
    let link = bin_dir.join(name);
    #[cfg(windows)]
    return vec![
        link,
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.ps1")),
    ];
    #[cfg(not(windows))]
    vec![link]
}

/// Hoist bins declared by a package's `bundledDependencies` into
/// `bin_dir`. The bundled children live under
/// `<pkg_dir>/node_modules/<bundled>/` straight from the tarball — the
/// resolver never walks them, so they don't show up in the regular
/// packument-driven bin-linking pass and need this companion hoist.
/// Matches pnpm's post-bin-linking pass for `hasBundledDependencies`.
/// Used by both the root importer (`link_bins`) and the per-workspace
/// loop so a workspace package depending on a parent with bundled deps
/// sees the children's bins in its own `node_modules/.bin`.
#[allow(clippy::too_many_arguments)]
fn link_bundled_bins(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
    conflict: BinConflict,
) -> miette::Result<()> {
    let Some(locked) = graph.get_package(dep_path) else {
        return Ok(());
    };
    for bundled in &locked.bundled_dependencies {
        let bundled_dir = pkg_dir.join("node_modules").join(bundled);
        let bundled_pkg_json_path = bundled_dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(&bundled_pkg_json_path) else {
            continue;
        };
        let Ok(bundled_pkg_json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(bin) = bundled_pkg_json.get("bin") else {
            continue;
        };
        link_bin_entries(
            bin_dir,
            &bundled_dir,
            Some(bundled),
            bin,
            shim_opts,
            managed,
            preserved,
            conflict,
        )?;
    }
    Ok(())
}

/// Shim each entry of a package.json `bin` field into `bin_dir`,
/// resolving relative targets against `pkg_dir`. Shared by the
/// dep-bin pass (`link_bins_for_dep`), bundled-deps pass
/// (`link_bundled_bins`), and importer self-bin pass (root + each
/// workspace member, discussion #228).
///
/// String-form `bin: "./x.js"` uses the basename of `pkg_name` as the
/// shim name (scope `@a/b` → `b`); the entry is silently skipped when
/// `pkg_name` is `None`. Object-form `bin: { foo: "./f" }` uses each
/// key as-is. Entries whose name or target fail
/// [`aube_linker::validate_bin_name`] / [`aube_linker::validate_bin_target`]
/// are dropped without error, matching the pnpm/npm "silently ignore
/// invalid bin" behavior.
#[allow(clippy::too_many_arguments)]
pub(super) fn link_bin_entries(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    pkg_name: Option<&str>,
    bin: &serde_json::Value,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
    conflict: BinConflict,
) -> miette::Result<()> {
    match bin {
        serde_json::Value::String(bin_path) => {
            let Some(name) = pkg_name else {
                return Ok(());
            };
            let bin_name = name.split('/').next_back().unwrap_or(name);
            if aube_linker::validate_bin_name(bin_name).is_ok()
                && aube_linker::validate_bin_target(bin_path).is_ok()
            {
                create_bin_link(
                    bin_dir,
                    bin_name,
                    &pkg_dir.join(bin_path),
                    shim_opts,
                    managed,
                    preserved,
                    conflict,
                )?;
            }
        }
        serde_json::Value::Object(bins) => {
            for (bin_name, path) in bins {
                if let Some(path_str) = path.as_str()
                    && aube_linker::validate_bin_name(bin_name).is_ok()
                    && aube_linker::validate_bin_target(path_str).is_ok()
                {
                    create_bin_link(
                        bin_dir,
                        bin_name,
                        &pkg_dir.join(path_str),
                        shim_opts,
                        managed,
                        preserved,
                        conflict,
                    )?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn create_bin_link(
    bin_dir: &std::path::Path,
    name: &str,
    target: &std::path::Path,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
    conflict: BinConflict,
) -> miette::Result<()> {
    // Whether an earlier pass in this run already put this command here.
    // Read before the insert below, which would otherwise make every
    // command look self-claimed.
    let already_claimed = managed
        .seen
        .get(bin_dir)
        .is_some_and(|names| names.contains(name));
    // Record the command before any early return: the relink pass uses
    // `seen` to tell "still claimed by some package" from "the package
    // that owned this went away", and a command we deliberately leave
    // alone is still claimed.
    managed
        .seen
        .entry(bin_dir.to_path_buf())
        .or_default()
        .insert(name.to_string());
    if let Some(preserved) = preserved
        && preserved
            .get(bin_dir)
            .is_some_and(|names| names.contains(name))
    {
        return Ok(());
    }
    let yield_to_existing = match conflict {
        BinConflict::Overwrite => false,
        BinConflict::YieldToClaimed => already_claimed,
        BinConflict::YieldToClaimedOrLive => already_claimed || bin_command_resolves(bin_dir, name),
    };
    if yield_to_existing {
        return Ok(());
    }
    // `link_dep_bins` skips eager `create_dir_all` on per-dep `.bin/`.
    // Deps whose children ship no bins stay empty on disk. First shim
    // write materializes the dir on demand.
    //
    // Windows `CreateDirectoryW` returns `ERROR_ALREADY_EXISTS` (os 183)
    // when the leaf sits behind a junction in the path, even when the
    // leaf is absent. The isolated layout's `.aube/<dep_path>` is a
    // junction into the global virtual store, so every `.bin/` under it
    // hits the quirk. Workaround: canonicalize the parent
    // (`crate::dirs::canonicalize` already strips the `\\?\` verbatim
    // prefix, which would otherwise trip CreateDirectoryW's own os-123
    // quirk, while keeping real `\\?\UNC\…` share paths intact), then
    // create everything down to `link_path.parent()` on that plain-drive
    // root. The leaf inode is shared with the surface side, so
    // `create_bin_shim` later writes through the surface path into the
    // same directory. Including the `link_path.parent()` here covers
    // scoped bin names (`@scope/foo`): we have to pre-create
    // `<bin_dir>/@scope/` on the canonical side too, because
    // `create_bin_shim`'s own `create_dir_all` would otherwise trip the
    // same quirk on the surface side and the shim's `@scope/foo.cmd`
    // write would fail with `NotFound`. No-op on Unix.
    //
    // Pass the *surface* `bin_dir` (not the canonicalized form) to
    // `create_bin_shim`: the shim's relative target is anchored on
    // `link_parent`, and the canonical form lives on a different
    // subtree (the GVS, e.g. `…\aube\virtual-store\…`) than the
    // surface invocation path (`…\.aube\<dep_path>\node_modules\.bin\`).
    // `pathdiff` would then find only `C:\Users\…\AppData\Local\` as a
    // common prefix and emit a long `..\..\..\…` traversal back down
    // through the surface tree, producing the duplicated install-root
    // path Node surfaces as `Cannot find module
    // '…\pnpm\global-aube\<hash>\pnpm\global-aube\<hash>\…'`
    // (Discussion #654).
    #[cfg(windows)]
    let mkdir_root_owned = bin_dir.parent().and_then(|parent| {
        let leaf = bin_dir.file_name()?;
        let canon = crate::dirs::canonicalize(parent).ok()?;
        Some(canon.join(leaf))
    });
    #[cfg(windows)]
    let mkdir_root: &std::path::Path = mkdir_root_owned.as_deref().unwrap_or(bin_dir);
    #[cfg(not(windows))]
    let mkdir_root = bin_dir;
    let mkdir_link_path = mkdir_root.join(name);
    let mkdir_target = mkdir_link_path.parent().unwrap_or(mkdir_root);
    if let Err(e) = std::fs::create_dir_all(mkdir_target) {
        let tolerated = e.kind() == std::io::ErrorKind::AlreadyExists && mkdir_target.is_dir();
        if !tolerated {
            return Err(e)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin directory {}", bin_dir.display()));
        }
    }
    aube_linker::create_bin_shim(bin_dir, name, target, shim_opts)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to link bin `{name}` at {} -> {}",
                bin_dir.join(name).display(),
                target.display()
            )
        })?;
    if !managed.capture {
        return Ok(());
    }
    let mut files = BTreeMap::new();
    for path in bin_link_paths(bin_dir, name) {
        if let Some(entry) = read_managed_bin_entry(&path)? {
            files.insert(path, entry);
        }
    }
    managed
        .entries
        .entry(bin_dir.to_path_buf())
        .or_default()
        .insert(name.to_string(), files);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};

    fn locked(name: &str, version: &str, bin: BTreeMap<String, String>) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            bin,
            ..Default::default()
        }
    }

    /// The hoisted transitive pass links every package's bins into the
    /// `.bin/` next to it, including the project root's. A transitive
    /// package that happens to ship a command an importer's direct
    /// dependency already claimed must not take it over, so that pass
    /// yields instead of overwriting (Discussion #1543).
    #[test]
    fn create_bin_link_yield_leaves_a_claimed_command_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let direct = pkg_dir.join("direct.js");
        let transitive = pkg_dir.join("transitive.js");
        std::fs::write(&direct, "#!/usr/bin/env node\nconsole.log('direct')\n").unwrap();
        std::fs::write(
            &transitive,
            "#!/usr/bin/env node\nconsole.log('transitive')\n",
        )
        .unwrap();

        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &direct,
            opts,
            &mut managed,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();
        let claimed = std::fs::read_to_string(bin_dir.join("tool")).unwrap();

        create_bin_link(
            &bin_dir,
            "tool",
            &transitive,
            opts,
            &mut managed,
            None,
            BinConflict::YieldToClaimedOrLive,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("tool")).unwrap(),
            claimed,
            "a transitive package must not retarget a command the direct dep claimed"
        );

        // A command nobody claimed yet still gets linked.
        create_bin_link(
            &bin_dir,
            "other",
            &transitive,
            opts,
            &mut managed,
            None,
            BinConflict::YieldToClaimedOrLive,
        )
        .unwrap();
        assert!(bin_dir.join("other").exists());

        // And the authoritative passes still overwrite.
        create_bin_link(
            &bin_dir,
            "tool",
            &transitive,
            opts,
            &mut managed,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();
        assert_ne!(
            std::fs::read_to_string(bin_dir.join("tool")).unwrap(),
            claimed
        );
    }

    /// A yielding pass returns early, but the command is still claimed by
    /// a package in the tree. The relink cleanup keys off `seen` to
    /// decide whether a *preserved* command (one a lifecycle script
    /// replaced) still has an owner, so the early return has to record
    /// the command first or the refresh pass would delete a shim the
    /// build deliberately produced.
    #[test]
    fn create_bin_link_yield_still_claims_a_preserved_command() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let target = pkg_dir.join("cli.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();

        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::capturing();
        create_bin_link(
            &bin_dir,
            "tool",
            &target,
            opts,
            &mut managed,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();

        // A lifecycle script swaps the shim for a native launcher.
        let shim = bin_dir.join("tool");
        std::fs::write(&shim, "#!/bin/sh\nexec ./tool.real\n").unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        assert!(
            shim.exists(),
            "a replaced launcher is preserved, not removed"
        );

        let mut relinked = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &target,
            opts,
            &mut relinked,
            Some(&preserved),
            BinConflict::YieldToClaimedOrLive,
        )
        .unwrap();
        remove_unclaimed_preserved_bin_links(&managed, &preserved, &relinked).unwrap();

        assert!(
            shim.exists(),
            "the lifecycle-produced launcher must survive the refresh pass"
        );
    }

    /// `.bin/` is never pruned, so an incremental hoisted install can
    /// meet a shim whose owning package has left the tree. Yielding to
    /// it would pin the command to the removed package forever; the
    /// transitive pass has to rewrite it to whoever owns the name now.
    #[cfg(unix)]
    #[test]
    fn yield_rewrites_a_command_whose_owner_left_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let old_owner = dir.path().join("node_modules/old-owner");
        let new_owner = dir.path().join("node_modules/new-owner");
        std::fs::create_dir_all(&old_owner).unwrap();
        std::fs::create_dir_all(&new_owner).unwrap();
        let old_target = old_owner.join("cli.js");
        let new_target = new_owner.join("cli.js");
        std::fs::write(&old_target, "#!/usr/bin/env node\n").unwrap();
        std::fs::write(&new_target, "#!/usr/bin/env node\n").unwrap();

        // Symlinked launchers are the hoisted default on POSIX.
        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(true),
            ..Default::default()
        };
        let mut previous_install = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &old_target,
            opts,
            &mut previous_install,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();

        // The next install drops `old-owner` from the tree. Nothing
        // prunes `.bin/`, so its launcher is left dangling.
        std::fs::remove_dir_all(&old_owner).unwrap();
        let shim = bin_dir.join("tool");
        assert!(
            shim.symlink_metadata().is_ok(),
            "the launcher is still there"
        );
        assert!(!shim.exists(), "but it no longer resolves");

        let mut managed = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &new_target,
            opts,
            &mut managed,
            None,
            BinConflict::YieldToClaimedOrLive,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(&shim).unwrap(),
            new_target,
            "a stale launcher must not block the package that owns the command now"
        );
    }

    /// The install path claims the importers' commands before the
    /// transitive pass runs, so `YieldToClaimed` needs no help from disk
    /// state — and must ignore it, or a leftover from the previous
    /// install would survive as above.
    #[test]
    fn yield_to_claimed_ignores_shims_from_a_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let stale = pkg_dir.join("stale.js");
        let current = pkg_dir.join("current.js");
        std::fs::write(&stale, "#!/usr/bin/env node\n").unwrap();
        std::fs::write(&current, "#!/usr/bin/env node\n").unwrap();

        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        // A previous run left a launcher behind; this run has not
        // claimed the command.
        let mut previous_run = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &stale,
            opts,
            &mut previous_run,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();
        let stale_shim = std::fs::read_to_string(bin_dir.join("tool")).unwrap();

        let mut this_run = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            &current,
            opts,
            &mut this_run,
            None,
            BinConflict::YieldToClaimed,
        )
        .unwrap();
        assert_ne!(
            std::fs::read_to_string(bin_dir.join("tool")).unwrap(),
            stale_shim,
            "an unclaimed command is reconciled to the current owner"
        );

        // Claimed this run by an importer's direct dep: the transitive
        // pass leaves it alone.
        create_bin_link(
            &bin_dir,
            "tool",
            &stale,
            opts,
            &mut this_run,
            None,
            BinConflict::YieldToClaimed,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(bin_dir.join("tool"))
                .unwrap()
                .contains("current.js"),
            "a command claimed this run must survive the transitive pass"
        );
    }

    #[test]
    fn managed_bin_cleanup_removes_owned_shims_and_preserves_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let removed_target = pkg_dir.join("removed.js");
        let replaced_target = pkg_dir.join("replaced.js");
        std::fs::write(&removed_target, "#!/usr/bin/env node\n").unwrap();
        std::fs::write(&replaced_target, "#!/usr/bin/env node\n").unwrap();

        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::capturing();
        create_bin_link(
            &bin_dir,
            "removed",
            &removed_target,
            opts,
            &mut managed,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();
        create_bin_link(
            &bin_dir,
            "replaced",
            &replaced_target,
            opts,
            &mut managed,
            None,
            BinConflict::Overwrite,
        )
        .unwrap();

        std::fs::write(bin_dir.join("replaced"), "#!/bin/sh\necho custom\n").unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        create_bin_link(
            &bin_dir,
            "replaced",
            &replaced_target,
            opts,
            &mut ManagedBinLinks::default(),
            Some(&preserved),
            BinConflict::Overwrite,
        )
        .unwrap();

        assert!(!bin_dir.join("removed").exists());
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("replaced")).unwrap(),
            "#!/bin/sh\necho custom\n"
        );
    }

    #[test]
    fn managed_bin_cleanup_preserves_siblings_of_a_replaced_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let launcher = bin_dir.join("tool");
        let sibling = bin_dir.join("tool.cmd");
        std::fs::write(&launcher, "generated launcher\n").unwrap();
        std::fs::write(&sibling, "generated sibling\n").unwrap();

        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            launcher.clone(),
            read_managed_bin_entry(&launcher).unwrap().unwrap(),
        );
        expected_files.insert(
            sibling.clone(),
            read_managed_bin_entry(&sibling).unwrap().unwrap(),
        );
        let mut commands = BTreeMap::new();
        commands.insert("tool".to_string(), expected_files);
        let mut managed = ManagedBinLinks::capturing();
        managed.entries.insert(bin_dir.clone(), commands);

        std::fs::write(&launcher, "lifecycle replacement\n").unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();

        assert!(preserved[&bin_dir].contains("tool"));
        assert_eq!(
            std::fs::read_to_string(launcher).unwrap(),
            "lifecycle replacement\n"
        );
        assert_eq!(
            std::fs::read_to_string(sibling).unwrap(),
            "generated sibling\n"
        );
    }

    #[test]
    fn managed_bin_cleanup_preserves_siblings_of_a_deleted_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let launcher = bin_dir.join("tool");
        let sibling = bin_dir.join("tool.cmd");
        std::fs::write(&launcher, "generated launcher\n").unwrap();
        std::fs::write(&sibling, "generated sibling\n").unwrap();

        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            launcher.clone(),
            read_managed_bin_entry(&launcher).unwrap().unwrap(),
        );
        expected_files.insert(
            sibling.clone(),
            read_managed_bin_entry(&sibling).unwrap().unwrap(),
        );
        let mut commands = BTreeMap::new();
        commands.insert("tool".to_string(), expected_files);
        let mut managed = ManagedBinLinks::capturing();
        managed.entries.insert(bin_dir.clone(), commands);

        std::fs::remove_file(&launcher).unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        let mut relinked = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            dir.path().join("target.js").as_path(),
            Default::default(),
            &mut relinked,
            Some(&preserved),
            BinConflict::Overwrite,
        )
        .unwrap();
        remove_unclaimed_preserved_bin_links(&managed, &preserved, &relinked).unwrap();

        assert!(preserved[&bin_dir].contains("tool"));
        assert!(!launcher.exists());
        assert_eq!(
            std::fs::read_to_string(sibling).unwrap(),
            "generated sibling\n"
        );
    }

    #[test]
    fn post_lifecycle_relink_removes_deleted_bin_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "removes-bin@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "removes-bin", 120, None);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("cli.js"), "#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"removes-bin","version":"1.0.0","bin":{"removed-bin":"cli.js"}}"#,
        )
        .unwrap();

        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("removes-bin", "1.0.0", BTreeMap::new()),
        );
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "removes-bin".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::capturing();
        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            opts,
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut managed,
            None,
        )
        .unwrap();
        let shim = project_dir.join("node_modules/.bin/removed-bin");
        assert!(shim.exists());

        // Simulate an approved dependency lifecycle script removing its bin
        // declaration before the post-build refresh.
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"removes-bin","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::remove_file(&shim).unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        let mut relinked = ManagedBinLinks::default();
        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            opts,
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut relinked,
            Some(&preserved),
        )
        .unwrap();
        remove_unclaimed_preserved_bin_links(&managed, &preserved, &relinked).unwrap();

        assert!(!shim.exists());
    }

    #[test]
    fn link_bins_reads_manifest_when_lockfile_metadata_is_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "vitepress@1.6.4";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "vitepress", 120, None);
        std::fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"vitepress","bin":{"vitepress":"bin/vitepress.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("bin/vitepress.js"), "#!/usr/bin/env node\n").unwrap();

        let mut semver_bin = BTreeMap::new();
        semver_bin.insert("semver".to_string(), "bin/semver.js".to_string());

        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("vitepress", "1.6.4", BTreeMap::new()),
        );
        packages.insert(
            "semver@7.7.4".to_string(),
            locked("semver", "7.7.4", semver_bin),
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "vitepress".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^1.5.0".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        assert!(project_dir.join("node_modules/.bin/vitepress").exists());
    }

    /// Regression for Discussion #654. The isolated layout puts
    /// `.aube/<dep_path>` as an NTFS junction into the global virtual
    /// store, and per-dep `.bin/` lives under that junction. The
    /// previous `create_bin_link` body canonicalized the bin-dir parent
    /// (workaround for `CreateDirectoryW`'s ERROR_ALREADY_EXISTS quirk)
    /// and *also* handed that canonical path to `create_bin_shim`. The
    /// generated `.cmd` then anchored its relative target on the GVS
    /// subtree, but `%~dp0` at runtime is the surface invocation path —
    /// so the combined path re-descended through the install root and
    /// Node surfaced `Cannot find module
    /// '…\pnpm\global-aube\<hash>\pnpm\global-aube\<hash>\…'`. The fix
    /// keeps the canonical mkdir but routes the shim writer through
    /// the surface `bin_dir`, so `pathdiff` sees a short common prefix
    /// and emits the expected `..\..\..\…` form.
    #[cfg(windows)]
    #[test]
    fn create_bin_link_surface_relative_path_when_dep_dir_is_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();

        // Stand-in for the GVS: a separate subtree the dep_path junction
        // points at.
        let gvs = project.join("gvs");
        let gvs_dep = gvs.join("node-liblzma@2.2.0/node_modules");
        std::fs::create_dir_all(&gvs_dep).unwrap();
        aube_linker::create_dir_link(
            &gvs.join("node-liblzma@2.2.0"),
            &aube_dir.join("node-liblzma@2.2.0"),
        )
        .unwrap();

        // Sibling `.aube/` entry housing the bin we want to shim into
        // the junction's `.bin/`. Lives on the surface tree, not under
        // the junction.
        let target_pkg = aube_dir.join("prebuild-install@7.1.3/node_modules/prebuild-install");
        std::fs::create_dir_all(&target_pkg).unwrap();
        let target = target_pkg.join("bin.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();

        // Surface bin dir: traverses the junction. Pre-fix, the canonical
        // form lived under `gvs/…`, which is precisely the mismatch this
        // test pins down.
        let bin_dir = aube_dir.join("node-liblzma@2.2.0/node_modules/.bin");

        create_bin_link(
            &bin_dir,
            "prebuild-install",
            &target,
            aube_linker::BinShimOptions::default(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        let cmd = std::fs::read_to_string(bin_dir.join("prebuild-install.cmd")).unwrap();
        // Three uplevels out of `.bin/`: `.bin` → `node_modules` →
        // `node-liblzma@2.2.0` → `.aube`, then descend into the sibling
        // `prebuild-install@7.1.3` entry.
        let expected = r"..\..\..\prebuild-install@7.1.3\node_modules\prebuild-install\bin.js";
        assert!(
            cmd.contains(expected),
            ".cmd shim should embed surface-tree relative path `{expected}`; got:\n{cmd}"
        );
        // Belt-and-braces: the pre-fix bug embedded a path that re-descended
        // through the project root after a long `..\` chain. Reject any
        // absolute-style fragment or a relative path that escapes far enough
        // to climb above `.aube/`.
        assert!(
            !cmd.contains(r"..\..\..\..\"),
            ".cmd shim should not climb above the `.aube/` root; got:\n{cmd}"
        );
    }

    /// Companion to the case above: scoped bin name (`@scope/foo`)
    /// behind the same junction. The pre-fix code routed shim writes
    /// through the canonical bin dir, so `create_bin_shim`'s internal
    /// `create_dir_all(<bin>\@scope)` ran on the GVS subtree where no
    /// junction is in the path — it just worked. With the fix, the
    /// shim writer sees the *surface* path and would hit the same
    /// "leaf behind junction" `ERROR_ALREADY_EXISTS` quirk on the
    /// `@scope/` mkdir. The fix's other half is pre-creating
    /// `link_path.parent()` on the canonical side; this test pins
    /// that behavior — without it, `@scope/foo.cmd` would fail to
    /// write through the junction with `NotFound`.
    #[cfg(windows)]
    #[test]
    fn create_bin_link_creates_scoped_parent_when_dep_dir_is_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();

        let gvs = project.join("gvs");
        let gvs_dep = gvs.join("node-liblzma@2.2.0/node_modules");
        std::fs::create_dir_all(&gvs_dep).unwrap();
        aube_linker::create_dir_link(
            &gvs.join("node-liblzma@2.2.0"),
            &aube_dir.join("node-liblzma@2.2.0"),
        )
        .unwrap();

        // Scoped sibling: target lives at
        // `.aube/@scope+tool@1.0.0/node_modules/@scope/tool/cli.js` on
        // the surface tree (the linker escapes `/` as `+` in the
        // dep_path filename).
        let target_pkg = aube_dir.join("@scope+tool@1.0.0/node_modules/@scope/tool");
        std::fs::create_dir_all(&target_pkg).unwrap();
        let target = target_pkg.join("cli.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();

        let bin_dir = aube_dir.join("node-liblzma@2.2.0/node_modules/.bin");

        create_bin_link(
            &bin_dir,
            "@scope/tool",
            &target,
            aube_linker::BinShimOptions::default(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        // `@scope/` must exist as an actual directory on the surface
        // side (visible via the junction) so the shim file landed.
        assert!(
            bin_dir.join("@scope").is_dir(),
            "scoped parent `@scope/` should be pre-created through the junction"
        );
        let cmd = std::fs::read_to_string(bin_dir.join("@scope/tool.cmd")).unwrap();
        // Four uplevels out of `.bin/@scope/`: `@scope` → `.bin` →
        // `node_modules` → `node-liblzma@2.2.0` → `.aube`, then descend
        // into the sibling scoped entry.
        let expected = r"..\..\..\..\@scope+tool@1.0.0\node_modules\@scope\tool\cli.js";
        assert!(
            cmd.contains(expected),
            ".cmd shim should embed surface-tree relative path `{expected}`; got:\n{cmd}"
        );
        assert!(
            !cmd.contains(r"..\..\..\..\..\"),
            ".cmd shim should not climb above the `.aube/` root; got:\n{cmd}"
        );
    }
}
