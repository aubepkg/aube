//! Hoisted (`node-linker=hoisted`) layout.
//!
//! Unlike the isolated layout — which materializes every package under
//! a per-project `.aube/<dep_path>/` virtual store and builds Node's
//! module graph out of symlinks — the hoisted layout writes real
//! package directories straight into `node_modules/`, nesting
//! conflicting versions under the parent that requires them. This
//! matches npm / yarn-classic's flat tree and is what certain legacy
//! toolchains (React Native's Metro, some Jest plugins) require.
//!
//! Placement algorithm (npm-style, workspace-aware):
//!
//! 1. Start with a `TreeNode` for the workspace-root `node_modules`
//!    directory, with physical importers modeled as synthetic children.
//! 2. Rank competing versions for each package name across the full
//!    workspace. An explicit root dependency wins; otherwise the version
//!    used by the most distinct dependents and peer dependents is preferred.
//! 3. BFS from the importer's direct deps. For each `(requester, name,
//!    dep_path)` pair, walk up from the requester looking for the
//!    shallowest ancestor whose `children[name]` is either absent or
//!    points at the same `dep_path`. That ancestor becomes the
//!    placement site.
//! 4. If a matching entry already exists at that ancestor, reuse it
//!    (dedupe). Otherwise create a new child node and enqueue every
//!    transitive dep of the placed package with the new node as
//!    requester.
//! 5. Conflicting versions naturally nest: when walking up from the
//!    requester we stop as soon as we find a different `dep_path`
//!    under the same name, so the conflict forces the new entry to
//!    live below the blocker (typically inside the requester's own
//!    `node_modules/`).
//!
//! The planner operates purely on dep_path strings — the same keys
//! aube-lockfile uses — so peer-context dep_paths like
//! `react-router@6(react@18)` are treated as distinct and won't
//! collapse onto a plain `react-router@6` placement. The side effect
//! is that peer-variant conflicts nest deeper in hoisted mode than in
//! isolated mode, which is the correct-but-slightly-inefficient
//! fallback.
//!
//! The planner output (`PlacementPlan`) is consumed by the
//! materializer and also surfaced to the
//! install driver via `HoistedPlacements` so bin linking and
//! dependency lifecycle scripts can locate a package's on-disk
//! directory without recomputing the tree.

use crate::{Error, HoistingLimits, LinkStats, Linker, apply_multi_file_patch};
use aube_lockfile::{DirectDep, LocalSource, LockfileGraph};
use aube_store::PackageIndex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// Map from lockfile `dep_path` to the absolute on-disk directories
/// where that package ended up. Most entries have exactly one path;
/// packages whose name conflicts with a shallower version end up
/// duplicated across multiple parent `node_modules/` directories so
/// each gets its own on-disk copy.
#[derive(Debug, Default, Clone)]
pub struct HoistedPlacements {
    by_dep_path: BTreeMap<String, Vec<PathBuf>>,
}

impl HoistedPlacements {
    /// Restore an exact placement map recorded when the tree was linked.
    /// This avoids replaying the planner against a graph whose filtering or
    /// iteration order may differ from the materialized install.
    pub fn from_package_dirs(by_dep_path: BTreeMap<String, Vec<PathBuf>>) -> Self {
        Self { by_dep_path }
    }

    /// Recompute hoisted placement paths for an already-linked graph
    /// without touching disk. Used by commands like `aube rebuild`
    /// that need to find package directories after install, but must
    /// not relink node_modules. `modules_dir_name` must match the
    /// `modulesDir` setting the install used, or the computed paths
    /// won't match what's on disk.
    pub fn from_graph(
        root_dir: &Path,
        graph: &LockfileGraph,
        modules_dir_name: &str,
        hoisting_limits: HoistingLimits,
    ) -> Result<Self, Error> {
        let mut placements = Self::default();
        let mut importers = Vec::with_capacity(graph.importers.len());
        for (importer_path, deps) in &graph.importers {
            if !crate::is_physical_importer(importer_path) {
                continue;
            }
            let importer_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                aube_util::path::normalize_lexical(&root_dir.join(importer_path))
            };
            importers.push(HoistedWorkspaceImporter {
                modules_dir: importer_dir.join(modules_dir_name),
                dependencies: deps.clone(),
            });
        }
        let root_nm = root_dir.join(modules_dir_name);
        let plan = plan_workspace(&root_nm, &importers, graph, hoisting_limits)?;
        for node in &plan.nodes {
            let (Some(dep_path), Some(pkg_dir)) = (&node.dep_path, &node.pkg_dir) else {
                continue;
            };
            if pkg_dir.exists() {
                placements.record(dep_path, pkg_dir.clone());
            }
        }
        Ok(placements)
    }

    /// Shallowest placement for `dep_path`, or `None` if the dep is
    /// not in the hoisted tree (e.g. filtered by `--prod` /
    /// `--no-optional`). Used by the install driver as the canonical
    /// location for bin linking and lifecycle-script cwds.
    pub fn package_dir(&self, dep_path: &str) -> Option<&Path> {
        self.by_dep_path
            .get(dep_path)
            .and_then(|v| v.first())
            .map(|p| p.as_path())
    }

    /// Every placement site for `dep_path`. When a name conflicts
    /// with a shallower version the same dep_path may appear at
    /// multiple depths; lifecycle scripts run once per site so each
    /// copy has its native-build artifacts in place.
    pub fn all_package_dirs(&self, dep_path: &str) -> &[PathBuf] {
        self.by_dep_path
            .get(dep_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Iterate `(dep_path, placement_path)` pairs in BTree order.
    /// Primarily used by the top-level installer when it wants to
    /// walk every placed copy (e.g. the stale-directory sweep or the
    /// lifecycle-script dispatcher).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.by_dep_path
            .iter()
            .flat_map(|(k, v)| v.iter().map(move |p| (k.as_str(), p.as_path())))
    }

    pub(crate) fn record(&mut self, dep_path: &str, path: PathBuf) {
        self.by_dep_path
            .entry(dep_path.to_string())
            .or_default()
            .push(path);
    }
}

/// One node in the placement tree. A node is either the importer
/// root (`pkg_dir == None`) or a placed package. `nm_dir` is the
/// `node_modules/` directory underneath this node where its children
/// live — for the importer that's `<importer>/node_modules`, for a
/// placed package it's `<parent.nm_dir>/<name>/node_modules`.
struct TreeNode {
    pkg_dir: Option<PathBuf>,
    nm_dir: PathBuf,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
    dep_path: Option<String>,
}

/// Arena-backed placement tree.
pub(crate) struct PlacementPlan {
    nodes: Vec<TreeNode>,
    root_idx: usize,
    importer_indices: Vec<usize>,
}

struct PlaceOutcome {
    node_idx: usize,
    created: bool,
}

#[derive(Default)]
struct PreferenceEntry {
    dependents: BTreeSet<String>,
    peer_dependents: BTreeSet<String>,
    discovery_order: usize,
}

impl PreferenceEntry {
    fn usages(&self) -> usize {
        self.dependents.len() + self.peer_dependents.len()
    }
}

impl PlacementPlan {
    fn new(importer_nm: PathBuf) -> Self {
        let root = TreeNode {
            pkg_dir: None,
            nm_dir: importer_nm,
            parent: None,
            children: BTreeMap::new(),
            dep_path: None,
        };
        Self {
            nodes: vec![root],
            root_idx: 0,
            importer_indices: vec![0],
        }
    }

    /// Add a workspace importer to the plan. The synthetic node has no
    /// package directory of its own; `parent` models whether Node's
    /// ancestor lookup can reach the workspace root.
    fn add_importer(&mut self, importer_nm: PathBuf, parent: Option<usize>) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: None,
            nm_dir: importer_nm,
            parent,
            children: BTreeMap::new(),
            dep_path: None,
        });
        self.importer_indices.push(idx);
        idx
    }

    /// Place `(name, dep_path)` under the ancestor chain rooted at
    /// `requester`. Returns the resulting node index and whether a
    /// fresh entry was created (so the caller knows whether to
    /// enqueue transitive deps).
    fn place(
        &mut self,
        requester: usize,
        floor: usize,
        name: &str,
        dep_path: &str,
    ) -> Result<PlaceOutcome, Error> {
        crate::validate_package_link_name(name)?;
        debug_assert!(is_ancestor_or_self(&self.nodes, floor, requester));
        // Reuse a matching package anywhere already visible through
        // Node's ancestor lookup, even if the hoist limit would
        // prevent placing a new package that high.
        let mut cursor = requester;
        loop {
            if let Some(&existing) = self.nodes[cursor].children.get(name) {
                if self.nodes[existing].dep_path.as_deref() == Some(dep_path) {
                    return Ok(PlaceOutcome {
                        node_idx: existing,
                        created: false,
                    });
                }
                // A nearer same-name package blocks Node from
                // resolving to any matching package above it.
                break;
            }
            match self.nodes[cursor].parent {
                Some(p) => cursor = p,
                None => break,
            }
        }

        // Walk up from the requester looking for the shallowest
        // allowed ancestor that doesn't already host a different
        // version of `name`.
        let mut cursor = requester;
        let mut candidate = requester;
        loop {
            if self.nodes[cursor].children.contains_key(name) {
                // Conflict: must stay at or below `candidate`.
                break;
            }
            candidate = cursor;
            if cursor == floor {
                break;
            }
            match self.nodes[cursor].parent {
                Some(p) => cursor = p,
                None => break,
            }
        }

        let parent_nm = self.nodes[candidate].nm_dir.clone();
        let pkg_dir = parent_nm.join(name);
        let nm_dir = pkg_dir.join("node_modules");
        let new_idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: Some(pkg_dir),
            nm_dir,
            parent: Some(candidate),
            children: BTreeMap::new(),
            dep_path: Some(dep_path.to_string()),
        });
        self.nodes[candidate]
            .children
            .insert(name.to_string(), new_idx);
        Ok(PlaceOutcome {
            node_idx: new_idx,
            created: true,
        })
    }

    fn should_defer_for_preference(
        &self,
        requester: usize,
        floor: usize,
        name: &str,
        dep_path: &str,
        preferred: &BTreeMap<String, String>,
    ) -> bool {
        if floor != self.root_idx
            || preferred
                .get(name)
                .is_none_or(|candidate| candidate == dep_path)
        {
            return false;
        }

        // Once a nearer same-name package exists, this request cannot claim
        // the shared root slot and delaying it provides no dedupe benefit.
        let mut cursor = requester;
        loop {
            if self.nodes[cursor].children.contains_key(name) {
                return false;
            }
            if cursor == floor {
                return true;
            }
            let Some(parent) = self.nodes[cursor].parent else {
                return false;
            };
            cursor = parent;
        }
    }

    /// Names placed directly in an importer's `node_modules/`. Drives
    /// the stale-entry sweep before the plan is materialized.
    fn importer_root_names(&self, importer_idx: usize) -> impl Iterator<Item = &str> {
        self.nodes[importer_idx].children.keys().map(String::as_str)
    }
}

fn is_ancestor_or_self(nodes: &[TreeNode], ancestor: usize, mut node: usize) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        let Some(parent) = nodes[node].parent else {
            return false;
        };
        node = parent;
    }
}

/// Build a placement plan for a single importer.
pub(crate) fn plan_importer(
    importer_nm: &Path,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
    hoisting_limits: HoistingLimits,
) -> Result<PlacementPlan, Error> {
    let mut plan = PlacementPlan::new(importer_nm.to_path_buf());
    let mut queue: VecDeque<(usize, usize, String, String)> = VecDeque::new();

    seed_importer(&mut queue, plan.root_idx, plan.root_idx, root_deps, graph);
    complete_plan(&mut plan, queue, graph, hoisting_limits, &BTreeMap::new())?;

    Ok(plan)
}

/// One physical importer in a hoisted workspace plan.
pub(crate) struct HoistedWorkspaceImporter {
    pub(crate) modules_dir: PathBuf,
    pub(crate) dependencies: Vec<DirectDep>,
}

/// Build one placement plan for an entire workspace. Importers are
/// represented as synthetic children of the workspace root, which lets
/// `hoistingLimits=none` promote compatible packages to the shared root
/// while the other limits keep their importer-local boundaries.
fn plan_workspace(
    root_nm: &Path,
    importers: &[HoistedWorkspaceImporter],
    graph: &LockfileGraph,
    hoisting_limits: HoistingLimits,
) -> Result<PlacementPlan, Error> {
    let mut plan = PlacementPlan::new(root_nm.to_path_buf());
    let mut queue: VecDeque<(usize, usize, String, String)> = VecDeque::new();
    let workspace_root = root_nm.parent().unwrap_or(root_nm);

    for importer in importers {
        let root_reachable = importer
            .modules_dir
            .parent()
            .is_some_and(|importer_dir| importer_dir.starts_with(workspace_root));
        let importer_idx = if importer.modules_dir == root_nm {
            plan.root_idx
        } else {
            plan.add_importer(
                importer.modules_dir.clone(),
                root_reachable.then_some(plan.root_idx),
            )
        };
        let floor = match hoisting_limits {
            HoistingLimits::None if root_reachable => plan.root_idx,
            HoistingLimits::None => importer_idx,
            HoistingLimits::Workspaces | HoistingLimits::Dependencies => importer_idx,
        };
        seed_importer(
            &mut queue,
            importer_idx,
            floor,
            &importer.dependencies,
            graph,
        );
    }

    let preferred = if matches!(hoisting_limits, HoistingLimits::None) {
        build_workspace_preferences(root_nm, importers, graph)
    } else {
        BTreeMap::new()
    };
    complete_plan(&mut plan, queue, graph, hoisting_limits, &preferred)?;
    Ok(plan)
}

fn build_workspace_preferences(
    root_nm: &Path,
    importers: &[HoistedWorkspaceImporter],
    graph: &LockfileGraph,
) -> BTreeMap<String, String> {
    let mut entries: BTreeMap<(String, String), PreferenceEntry> = BTreeMap::new();
    let mut root_direct = BTreeMap::new();
    let mut pending = VecDeque::new();
    let mut discovery_order = 0;
    let workspace_root = root_nm.parent().unwrap_or(root_nm);

    for (importer_idx, importer) in importers.iter().enumerate() {
        let root_reachable = importer
            .modules_dir
            .parent()
            .is_some_and(|importer_dir| importer_dir.starts_with(workspace_root));
        if !root_reachable {
            continue;
        }
        let dependent = format!("workspace:{importer_idx}");
        for dep in &importer.dependencies {
            let is_link = graph
                .packages
                .get(&dep.dep_path)
                .is_some_and(|pkg| matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))));
            // A non-root `link:` edge is pinned to its importer and can never
            // claim the shared root slot. A root direct link is already at the
            // root floor, so it remains an unconditional root preference.
            if is_link && importer.modules_dir != root_nm {
                continue;
            }
            if importer.modules_dir == root_nm {
                root_direct.insert(dep.name.clone(), dep.dep_path.clone());
            }
            let entry = entries
                .entry((dep.name.clone(), dep.dep_path.clone()))
                .or_insert_with(|| {
                    let entry = PreferenceEntry {
                        discovery_order,
                        ..PreferenceEntry::default()
                    };
                    discovery_order += 1;
                    entry
                });
            entry.dependents.insert(dependent.clone());
            if !is_link {
                pending.push_back(dep.dep_path.clone());
            }
        }
    }

    let mut expanded = BTreeSet::new();
    while let Some(parent_dep_path) = pending.pop_front() {
        if !expanded.insert(parent_dep_path.clone()) {
            continue;
        }
        let Some(pkg) = graph.packages.get(&parent_dep_path) else {
            continue;
        };
        if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
            continue;
        }
        for (dep_name, dep_tail) in &pkg.dependencies {
            let child_dep_path = aube_lockfile::shared_local_dep_path(dep_name, dep_tail)
                .unwrap_or_else(|| format!("{dep_name}@{dep_tail}"));
            if !graph.packages.contains_key(&child_dep_path) {
                continue;
            }
            let entry = entries
                .entry((dep_name.clone(), child_dep_path.clone()))
                .or_insert_with(|| {
                    let entry = PreferenceEntry {
                        discovery_order,
                        ..PreferenceEntry::default()
                    };
                    discovery_order += 1;
                    entry
                });
            if pkg.peer_dependencies.contains_key(dep_name) {
                entry.peer_dependents.insert(parent_dep_path.clone());
            } else {
                entry.dependents.insert(parent_dep_path.clone());
            }
            pending.push_back(child_dep_path);
        }
    }

    let mut candidates: BTreeMap<String, Vec<(String, usize, usize)>> = BTreeMap::new();
    for ((name, dep_path), entry) in entries {
        candidates
            .entry(name)
            .or_default()
            .push((dep_path, entry.usages(), entry.discovery_order));
    }

    let mut preferred = BTreeMap::new();
    for (name, mut versions) in candidates {
        if let Some(dep_path) = root_direct.remove(&name) {
            preferred.insert(name, dep_path);
            continue;
        }
        versions.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
        if let Some((dep_path, _, _)) = versions.into_iter().next() {
            preferred.insert(name, dep_path);
        }
    }
    preferred.extend(root_direct);
    preferred
}

fn seed_importer(
    queue: &mut VecDeque<(usize, usize, String, String)>,
    importer_idx: usize,
    floor: usize,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
) {
    // Seed the queue with the importer's direct deps in declaration order.
    // The workspace preference pass preserves that order as the stable
    // tiebreaker after usage counts.
    for dep in root_deps {
        let Some(pkg) = graph.packages.get(&dep.dep_path) else {
            continue;
        };
        // A direct `link:` is a live importer-relative edge, including the
        // locked representation of `workspace:` dependencies. Keep it in the
        // consuming importer's node_modules instead of sharing it at the
        // workspace root like an immutable registry package.
        let dep_floor = if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
            importer_idx
        } else {
            floor
        };
        queue.push_back((
            importer_idx,
            dep_floor,
            dep.name.clone(),
            dep.dep_path.clone(),
        ));
    }
}

fn complete_plan(
    plan: &mut PlacementPlan,
    mut queue: VecDeque<(usize, usize, String, String)>,
    graph: &LockfileGraph,
    hoisting_limits: HoistingLimits,
    preferred: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let mut consecutive_deferrals = 0;
    let mut force_next = false;
    while let Some((requester, floor, name, dep_path)) = queue.pop_front() {
        if !force_next
            && plan.should_defer_for_preference(requester, floor, &name, &dep_path, preferred)
        {
            queue.push_back((requester, floor, name, dep_path));
            consecutive_deferrals += 1;
            if consecutive_deferrals >= queue.len() {
                // The preferred candidate is unreachable until one of the
                // deferred packages lands (or is outside this root). Let the
                // stable first candidate through so planning still converges.
                force_next = true;
            }
            continue;
        }
        consecutive_deferrals = 0;
        force_next = false;
        let outcome = plan.place(requester, floor, &name, &dep_path)?;
        if !outcome.created {
            continue;
        }
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        // Skip transitives for `link:` deps — their target directory
        // holds its own node_modules and Node resolves through it
        // naturally. Materializing a copy would fight with a live
        // workspace package.
        if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
            continue;
        }
        let child_floor = match hoisting_limits {
            HoistingLimits::None => plan.root_idx,
            HoistingLimits::Workspaces => floor,
            HoistingLimits::Dependencies => outcome.node_idx,
        };
        for (dep_name, dep_tail) in &pkg.dependencies {
            // Git / remote-tarball deps are recorded by their resolved URL
            // spec but keyed under the short `name@git+<hash>` /
            // `name@url+<hash>` form, so the verbatim `name@tail` key would
            // miss `graph.packages` and silently drop the dep's subtree.
            let child_dep_path = aube_lockfile::shared_local_dep_path(dep_name, dep_tail)
                .unwrap_or_else(|| format!("{dep_name}@{dep_tail}"));
            if !graph.packages.contains_key(&child_dep_path) {
                continue;
            }
            queue.push_back((
                outcome.node_idx,
                child_floor,
                dep_name.clone(),
                child_dep_path,
            ));
        }
    }
    Ok(())
}

/// Materialize a planned tree onto disk for a single importer.
///
/// Called by `Linker::link_all` and `Linker::link_workspace` when the
/// linker is configured with `NodeLinker::Hoisted`. The importer's
/// existing `node_modules/` is swept of any top-level entries the
/// plan doesn't claim (direct deps from a previous install may have
/// changed); placed packages are then materialized in two passes —
/// local (`file:`/`link:`) first, then registry packages via the
/// standard reflink/hardlink/copy file-linker.
///
/// Every placed package is recorded in `placements` so the install
/// driver can later resolve `dep_path -> on-disk dir` for bin
/// linking and lifecycle scripts without recomputing the plan.
pub(crate) struct HoistedImporterDirs<'a> {
    pub(crate) root: &'a Path,
    pub(crate) importer: &'a Path,
}

pub(crate) fn link_hoisted_importer(
    linker: &Linker,
    dirs: HoistedImporterDirs<'_>,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    let root_dir = dirs.root;
    let importer_dir = dirs.importer;
    let nm = importer_dir.join(linker.modules_dir_name());
    let plan = plan_importer(&nm, root_deps, graph, linker.hoisting_limits)?;
    materialize_hoisted_plan(
        linker,
        root_dir,
        &plan,
        graph,
        package_indices,
        stats,
        placements,
    )
}

pub(crate) fn link_hoisted_workspace(
    linker: &Linker,
    root_dir: &Path,
    importers: &[HoistedWorkspaceImporter],
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    let root_nm = root_dir.join(linker.modules_dir_name());
    let plan = plan_workspace(&root_nm, importers, graph, linker.hoisting_limits)?;
    materialize_hoisted_plan(
        linker,
        root_dir,
        &plan,
        graph,
        package_indices,
        stats,
        placements,
    )
}

fn materialize_hoisted_plan(
    linker: &Linker,
    root_dir: &Path,
    plan: &PlacementPlan,
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    for &importer_idx in &plan.importer_indices {
        let nm = &plan.nodes[importer_idx].nm_dir;
        crate::mkdirp(nm)?;
        let keep_root: std::collections::HashSet<&str> =
            plan.importer_root_names(importer_idx).collect();
        crate::sweep_stale_top_level_entries(nm, &keep_root, None);
    }

    // Materialize every package node. Order doesn't matter for
    // correctness (each package's files are written into its own
    // directory) but we iterate by index so the BFS order surfaces
    // in progress/debug logs.
    for idx in 0..plan.nodes.len() {
        // Borrow scoping: take a clone of the fields we need out of
        // the node before calling methods that re-borrow `linker`
        // with `&mut stats`. The arena is read-only from here on.
        let node = &plan.nodes[idx];
        let (Some(dep_path), Some(pkg_dir)) = (&node.dep_path, &node.pkg_dir) else {
            continue;
        };
        let dep_path = dep_path.clone();
        let pkg_dir = pkg_dir.clone();
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };

        // `link:` deps: symlink the package dir straight at the
        // target. `link:` packages were excluded from the dependency
        // plan above because their target owns its deps. `portal:`
        // packages stay on the materialized-package path so their
        // graph-visible deps are linked like Yarn expects.
        // `rebase_local` in the resolver (and preserved-lockfile
        // import) stores local paths relative to the project root.
        if let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref() {
            if let Some(parent) = pkg_dir.parent() {
                crate::mkdirp(parent)?;
            }
            crate::try_remove_entry(&pkg_dir);
            let abs_target = root_dir.join(rel);
            let link_parent = pkg_dir.parent().unwrap_or(root_dir);
            let rel_target = pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
            crate::sys::create_dir_link(&rel_target, &pkg_dir)
                .map_err(|e| Error::Io(pkg_dir.clone(), e))?;
            placements.record(&dep_path, pkg_dir);
            // Don't bump `top_level_linked` here: the post-loop
            // `children.len()` add below already counts every root
            // child including `link:` direct deps. Incrementing in
            // both places would double-count.
            continue;
        }

        // Registry (or `file:`) package — needs a PackageIndex to
        // find the store-backed file set. `package_indices` is sparse
        // on warm installs, so lazy-load from the store on miss.
        let owned_index;
        let index = match package_indices.get(&dep_path) {
            Some(i) => i,
            None => {
                // `registry_name()` is the lookup key for npm-aliased
                // packages (`"h3-v2": "npm:h3@..."`), which saved the
                // index under the real package name at fetch time.
                // Integrity is part of the cache key so a same-name
                // dep resolved from a non-registry source (git, remote
                // tarball, file:) can't pick up a registry-sourced
                // cache entry and get a different file list than its
                // own tarball actually contains.
                let loaded = linker
                    .store
                    .load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
                    .ok_or_else(|| Error::MissingPackageIndex(dep_path.clone()))?;
                owned_index = loaded;
                &owned_index
            }
        };

        // Wipe any previous contents at this path so a re-run after
        // changing versions doesn't leave stale files behind, then
        // batch-create every intermediate parent directory the index
        // will write into.
        crate::try_remove_entry(&pkg_dir);
        let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
        parents.insert(pkg_dir.clone());
        // Validate every key once here. The file-linking loop below
        // walks the same immutable index, so skipping the check
        // there is safe.
        for rel_path in index.keys() {
            crate::validate_index_key(rel_path)?;
            let target = pkg_dir.join(rel_path);
            if let Some(parent) = target.parent() {
                parents.insert(parent.to_path_buf());
            }
        }
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        for (rel_path, stored) in index {
            // Key already validated in the parent-collection loop
            // above. The index is immutable between the two loops.
            let target = pkg_dir.join(rel_path);
            if let Err(e) = linker.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    crate::invalidate_stale_index_for_package(&linker.store, pkg);
                }
                return Err(e);
            }
            stats.files_linked += 1;
            if stored.executable {
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        if let Some((patch_key, patch_text)) = pkg.lookup_patch(&linker.patches) {
            apply_multi_file_patch(&pkg_dir, patch_text)
                .map_err(|msg| Error::Patch(patch_key, msg))?;
        }

        stats.packages_linked += 1;
        placements.record(&dep_path, pkg_dir);
    }

    stats.top_level_linked += plan
        .importer_indices
        .iter()
        .map(|&idx| plan.nodes[idx].children.len())
        .sum::<usize>();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, LockedPackage};

    fn dep(name: &str, dep_path: &str) -> DirectDep {
        DirectDep {
            name: name.to_string(),
            dep_path: dep_path.to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }
    }

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            dependencies: deps
                .iter()
                .map(|(dep_name, tail)| ((*dep_name).to_string(), (*tail).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn package_dir(plan: &PlacementPlan, dep_path: &str) -> PathBuf {
        plan.nodes
            .iter()
            .find(|node| node.dep_path.as_deref() == Some(dep_path))
            .and_then(|node| node.pkg_dir.clone())
            .unwrap_or_else(|| panic!("{dep_path} was not placed"))
    }

    fn package_dirs(plan: &PlacementPlan, dep_path: &str) -> Vec<PathBuf> {
        plan.nodes
            .iter()
            .filter(|node| node.dep_path.as_deref() == Some(dep_path))
            .filter_map(|node| node.pkg_dir.clone())
            .collect()
    }

    #[test]
    fn workspace_limit_controls_cross_importer_hoisting() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/app/node_modules"),
                dependencies: vec![dep("shared", "shared@1.0.0")],
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/lib/node_modules"),
                dependencies: vec![dep("shared", "shared@1.0.0")],
            },
        ];

        let unlimited = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();
        assert_eq!(
            package_dirs(&unlimited, "shared@1.0.0"),
            vec![root_nm.join("shared")]
        );

        let workspace_limited =
            plan_workspace(&root_nm, &importers, &graph, HoistingLimits::Workspaces).unwrap();
        assert_eq!(
            package_dirs(&workspace_limited, "shared@1.0.0"),
            vec![
                PathBuf::from("/project/packages/app/node_modules/shared"),
                PathBuf::from("/project/packages/lib/node_modules/shared"),
            ]
        );
    }

    #[test]
    fn workspace_prefers_version_used_by_more_dependents_and_peers() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("react@19.2.3".into(), pkg("react", "19.2.3", &[]));
        graph
            .packages
            .insert("react@19.2.6".into(), pkg("react", "19.2.6", &[]));
        let zustand_dep_path = "zustand@5.0.11(react@19.2.6)";
        let mut zustand = pkg("zustand", "5.0.11", &[("react", "19.2.6")]);
        zustand.dep_path = zustand_dep_path.to_string();
        zustand
            .peer_dependencies
            .insert("react".to_string(), ">=18.0.0".to_string());
        graph.packages.insert(zustand_dep_path.into(), zustand);
        let importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/mobile/node_modules"),
                dependencies: vec![dep("react", "react@19.2.3")],
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/web/node_modules"),
                dependencies: vec![
                    dep("react", "react@19.2.6"),
                    dep("zustand", zustand_dep_path),
                ],
            },
        ];

        let plan = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();

        assert_eq!(
            package_dirs(&plan, "react@19.2.6"),
            vec![root_nm.join("react")]
        );
        assert_eq!(
            package_dirs(&plan, "react@19.2.3"),
            vec![PathBuf::from("/project/packages/mobile/node_modules/react")]
        );
        assert_eq!(
            package_dir(&plan, zustand_dep_path),
            root_nm.join("zustand")
        );
    }

    #[test]
    fn workspace_root_direct_dependency_overrides_usage_preference() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("react@19.2.3".into(), pkg("react", "19.2.3", &[]));
        graph
            .packages
            .insert("react@19.2.6".into(), pkg("react", "19.2.6", &[]));
        graph.packages.insert(
            "consumer@1.0.0".into(),
            pkg("consumer", "1.0.0", &[("react", "19.2.6")]),
        );
        let importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: root_nm.clone(),
                dependencies: vec![dep("react", "react@19.2.3")],
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/web/node_modules"),
                dependencies: vec![
                    dep("react", "react@19.2.6"),
                    dep("consumer", "consumer@1.0.0"),
                ],
            },
        ];

        let plan = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();

        assert_eq!(
            package_dirs(&plan, "react@19.2.3"),
            vec![root_nm.join("react")]
        );
        assert_eq!(package_dirs(&plan, "react@19.2.6").len(), 2);
    }

    #[test]
    fn workspace_defers_direct_conflict_until_preferred_transitive_is_discovered() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        for consumer in ["consumer-a", "consumer-b"] {
            graph.packages.insert(
                format!("{consumer}@1.0.0"),
                pkg(consumer, "1.0.0", &[("shared", "2.0.0")]),
            );
        }
        let importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/first/node_modules"),
                dependencies: vec![dep("shared", "shared@1.0.0")],
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/second/node_modules"),
                dependencies: vec![
                    dep("consumer-a", "consumer-a@1.0.0"),
                    dep("consumer-b", "consumer-b@1.0.0"),
                ],
            },
        ];

        let plan = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();

        assert_eq!(
            package_dirs(&plan, "shared@2.0.0"),
            vec![root_nm.join("shared")]
        );
        assert_eq!(
            package_dirs(&plan, "shared@1.0.0"),
            vec![PathBuf::from("/project/packages/first/node_modules/shared")]
        );
    }

    #[test]
    fn workspace_preferences_exclude_root_ineligible_candidates() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        graph
            .packages
            .insert("shared@3.0.0".into(), pkg("shared", "3.0.0", &[]));
        let link_dep_path = "shared@link:../shared";
        let mut linked = pkg("shared", "0.0.0", &[]);
        linked.local_source = Some(LocalSource::Link(PathBuf::from("packages/shared")));
        graph.packages.insert(link_dep_path.into(), linked);

        let mut importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/web/node_modules"),
                dependencies: vec![dep("shared", "shared@3.0.0")],
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/project/packages/lib/node_modules"),
                dependencies: vec![dep("shared", "shared@3.0.0")],
            },
        ];
        for idx in 0..3 {
            importers.push(HoistedWorkspaceImporter {
                modules_dir: PathBuf::from(format!("/sibling-{idx}/node_modules")),
                dependencies: vec![dep("shared", "shared@2.0.0")],
            });
        }
        for idx in 0..4 {
            importers.push(HoistedWorkspaceImporter {
                modules_dir: PathBuf::from(format!("/project/packages/linked-{idx}/node_modules")),
                dependencies: vec![dep("shared", link_dep_path)],
            });
        }

        let preferred = build_workspace_preferences(&root_nm, &importers, &graph);

        assert_eq!(
            preferred.get("shared").map(String::as_str),
            Some("shared@3.0.0")
        );
    }

    #[test]
    fn preference_fallback_places_first_candidate_when_preferred_is_unavailable() {
        let root_nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let root_deps = vec![dep("shared", "shared@1.0.0")];
        let mut plan = PlacementPlan::new(root_nm.clone());
        let mut queue = VecDeque::new();
        seed_importer(&mut queue, plan.root_idx, plan.root_idx, &root_deps, &graph);

        complete_plan(
            &mut plan,
            queue,
            &graph,
            HoistingLimits::None,
            &BTreeMap::from([("shared".to_string(), "shared@2.0.0".to_string())]),
        )
        .unwrap();

        assert_eq!(
            package_dirs(&plan, "shared@1.0.0"),
            vec![root_nm.join("shared")]
        );
    }

    #[test]
    fn workspace_plan_keeps_direct_links_in_the_consuming_importer() {
        let root_nm = PathBuf::from("/project/node_modules");
        let importer_nm = PathBuf::from("/project/packages/app/node_modules");
        let mut linked = pkg("linked", "0.0.0", &[]);
        linked.local_source = Some(LocalSource::Link(PathBuf::from("packages/linked")));
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("linked@link:../linked".into(), linked);
        let importers = vec![HoistedWorkspaceImporter {
            modules_dir: importer_nm.clone(),
            dependencies: vec![dep("linked", "linked@link:../linked")],
        }];

        let plan = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();

        assert_eq!(
            package_dir(&plan, "linked@link:../linked"),
            importer_nm.join("linked")
        );
    }

    #[test]
    fn dependencies_limit_keeps_transitives_under_their_direct_dep() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("left-pad", "1.0.0")]),
        );
        graph.packages.insert(
            "left-pad@1.0.0".into(),
            pkg("left-pad", "1.0.0", &[("repeat", "1.0.0")]),
        );
        graph
            .packages
            .insert("repeat@1.0.0".into(), pkg("repeat", "1.0.0", &[]));
        let root_deps = vec![dep("app", "app@1.0.0")];

        let unlimited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::None).unwrap();
        assert_eq!(
            package_dir(&unlimited, "left-pad@1.0.0"),
            nm.join("left-pad")
        );
        assert_eq!(package_dir(&unlimited, "repeat@1.0.0"), nm.join("repeat"));

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();
        assert_eq!(
            package_dir(&limited, "left-pad@1.0.0"),
            nm.join("app/node_modules/left-pad")
        );
        assert_eq!(
            package_dir(&limited, "repeat@1.0.0"),
            nm.join("app/node_modules/left-pad/node_modules/repeat")
        );
    }

    #[test]
    fn dependencies_limit_reuses_matching_direct_dependency_above_floor() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let root_deps = vec![dep("shared", "shared@1.0.0"), dep("app", "app@1.0.0")];

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();

        assert_eq!(package_dir(&limited, "shared@1.0.0"), nm.join("shared"));
        assert_eq!(
            limited
                .nodes
                .iter()
                .filter(|node| node.dep_path.as_deref() == Some("shared@1.0.0"))
                .count(),
            1
        );
    }

    #[test]
    fn dependencies_limit_does_not_reuse_above_version_blocker() {
        let nm = PathBuf::from("/project/node_modules");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("shared", "2.0.0"), ("tool", "1.0.0")]),
        );
        graph.packages.insert(
            "tool@1.0.0".into(),
            pkg("tool", "1.0.0", &[("shared", "1.0.0")]),
        );
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        graph
            .packages
            .insert("shared@2.0.0".into(), pkg("shared", "2.0.0", &[]));
        let root_deps = vec![dep("shared", "shared@1.0.0"), dep("app", "app@1.0.0")];

        let limited = plan_importer(&nm, &root_deps, &graph, HoistingLimits::Dependencies).unwrap();

        let shared_v1_dirs: Vec<_> = limited
            .nodes
            .iter()
            .filter(|node| node.dep_path.as_deref() == Some("shared@1.0.0"))
            .filter_map(|node| node.pkg_dir.as_ref())
            .collect();
        assert_eq!(shared_v1_dirs.len(), 2);
        assert!(shared_v1_dirs.contains(&&nm.join("shared")));
        assert!(shared_v1_dirs.contains(&&nm.join("app/node_modules/tool/node_modules/shared")));
    }

    #[test]
    fn from_graph_respects_dependencies_limit() {
        let root = tempfile::tempdir().unwrap();
        let nm = root.path().join("node_modules");
        let app_dir = nm.join("app");
        let left_pad_dir = app_dir.join("node_modules/left-pad");
        std::fs::create_dir_all(&left_pad_dir).unwrap();

        let mut graph = LockfileGraph::default();
        graph
            .importers
            .insert(".".into(), vec![dep("app", "app@1.0.0")]);
        graph.packages.insert(
            "app@1.0.0".into(),
            pkg("app", "1.0.0", &[("left-pad", "1.0.0")]),
        );
        graph
            .packages
            .insert("left-pad@1.0.0".into(), pkg("left-pad", "1.0.0", &[]));

        let placements = HoistedPlacements::from_graph(
            root.path(),
            &graph,
            "node_modules",
            HoistingLimits::Dependencies,
        )
        .unwrap();

        assert_eq!(
            placements.package_dir("left-pad@1.0.0"),
            Some(left_pad_dir.as_path())
        );
    }

    #[test]
    fn from_graph_reconstructs_shared_workspace_root_placement() {
        let root = tempfile::tempdir().unwrap();
        let shared_dir = root.path().join("node_modules/shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        let mut graph = LockfileGraph::default();
        graph
            .importers
            .insert("packages/app".into(), vec![dep("shared", "shared@1.0.0")]);
        graph
            .importers
            .insert("packages/lib".into(), vec![dep("shared", "shared@1.0.0")]);
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));

        let placements = HoistedPlacements::from_graph(
            root.path(),
            &graph,
            "node_modules",
            HoistingLimits::None,
        )
        .unwrap();

        assert_eq!(
            placements.package_dir("shared@1.0.0"),
            Some(shared_dir.as_path())
        );
        assert_eq!(placements.all_package_dirs("shared@1.0.0").len(), 1);
    }

    #[test]
    fn parent_relative_importer_cannot_reuse_workspace_root_placement() {
        let root_nm = PathBuf::from("/workspace/node_modules");
        let mut graph = LockfileGraph::default();
        graph
            .packages
            .insert("shared@1.0.0".into(), pkg("shared", "1.0.0", &[]));
        let deps = vec![dep("shared", "shared@1.0.0")];
        let importers = vec![
            HoistedWorkspaceImporter {
                modules_dir: root_nm.clone(),
                dependencies: deps.clone(),
            },
            HoistedWorkspaceImporter {
                modules_dir: PathBuf::from("/sibling/node_modules"),
                dependencies: deps,
            },
        ];

        let plan = plan_workspace(&root_nm, &importers, &graph, HoistingLimits::None).unwrap();
        let dirs: Vec<_> = plan
            .nodes
            .iter()
            .filter(|node| node.dep_path.as_deref() == Some("shared@1.0.0"))
            .filter_map(|node| node.pkg_dir.as_ref())
            .collect();

        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&&root_nm.join("shared")));
        assert!(dirs.contains(&&PathBuf::from("/sibling/node_modules/shared")));
    }
}
