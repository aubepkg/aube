use crate::{DirectDep, LockedPackage};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Resolve a transitive dep name from the perspective of a package at
/// `pkg_install_path` using npm's nested-resolution walk: look first inside
/// the package's own `node_modules`, then walk up each ancestor's
/// `node_modules`, finally falling back to the root `node_modules`.
pub(super) fn resolve_nested<T>(
    pkg_install_path: &str,
    dep_name: &str,
    install_paths: &BTreeMap<String, T>,
) -> Option<String> {
    let mut base = pkg_install_path.to_string();
    loop {
        let candidate = if base.is_empty() {
            format!("node_modules/{dep_name}")
        } else {
            format!("{base}/node_modules/{dep_name}")
        };
        if install_paths.contains_key(&candidate) {
            return Some(candidate);
        }
        if base.is_empty() {
            return None;
        }
        // Walk up one level: strip the trailing "/node_modules/<pkg>" segment.
        if let Some(idx) = base.rfind("/node_modules/") {
            base.truncate(idx);
        } else {
            // We're at a top-level path like "node_modules/foo" — next step is root.
            base.clear();
        }
    }
}

// Peer flags belong to installed copies, which can have different incoming
// edges even when the resolver collapses them to the same name and version.
pub(super) fn non_peer_placements(packages: &serde_json::Value) -> BTreeSet<String> {
    reachable_placements(packages, false)
}

pub(super) fn reachable_placements(
    packages: &serde_json::Value,
    include_peers: bool,
) -> BTreeSet<String> {
    let entries: BTreeMap<_, _> = packages
        .as_object()
        .into_iter()
        .flatten()
        .map(|(path, entry)| (path.clone(), entry))
        .collect();
    let mut queue: VecDeque<_> = entries
        .keys()
        .filter(|path| !path.split('/').any(|part| part == "node_modules"))
        .cloned()
        .collect();
    let mut reached = BTreeSet::new();
    while let Some(path) = queue.pop_front() {
        if !reached.insert(path.clone()) {
            continue;
        }
        let Some(entry) = entries.get(&path) else {
            continue;
        };
        if entry["link"] == true {
            if let Some(target) = entry["resolved"].as_str() {
                queue.push_back(target.to_owned());
            }
            continue;
        }
        for section in ["dependencies", "devDependencies", "optionalDependencies"]
            .into_iter()
            .chain(include_peers.then_some("peerDependencies"))
        {
            for name in entry[section]
                .as_object()
                .into_iter()
                .flat_map(|deps| deps.keys())
            {
                if let Some(target) = resolve_nested(&path, name, &entries) {
                    queue.push_back(target);
                }
            }
        }
    }
    reached
}

// Ignore peer-context suffixes, which resolution may add without changing
// the package identities or dependency declarations imported from npm.
pub(super) fn dependency_signature(pkg: &LockedPackage) -> serde_json::Value {
    let dependencies: BTreeMap<_, _> = pkg
        .dependencies
        .iter()
        .map(|(name, value)| (name, child_canonical_key(name, value)))
        .collect();
    serde_json::json!({
        "dependencies": dependencies,
        "declared": pkg.declared_dependencies,
        "optional": pkg.optional_dependencies.keys().collect::<Vec<_>>(),
        "peers": pkg.peer_dependencies,
        "bundled": pkg.bundled_dependencies,
    })
}

pub(super) fn dependency_snapshot(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: Vec<String>,
) -> BTreeMap<String, serde_json::Value> {
    let mut snapshot = BTreeMap::new();
    let mut queue: VecDeque<_> = roots.into();
    while let Some(key) = queue.pop_front() {
        if snapshot.contains_key(&key) {
            continue;
        }
        let Some(pkg) = canonical.get(&key) else {
            continue;
        };
        snapshot.insert(key, dependency_signature(pkg));
        queue.extend(
            pkg.dependencies
                .iter()
                .map(|(name, value)| child_canonical_key(name, value)),
        );
    }
    snapshot
}

/// Extract a package name from an install path like `node_modules/foo`,
/// `node_modules/@scope/foo`, or `node_modules/foo/node_modules/bar`.
pub(super) fn package_name_from_install_path(install_path: &str) -> Option<String> {
    // Find the last "node_modules/" segment and return everything after it,
    // preserving a scope prefix (`@scope/pkg`).
    let nm_idx = install_path.rfind("node_modules/")?;
    let tail = &install_path[nm_idx + "node_modules/".len()..];

    if tail.is_empty() {
        return None;
    }

    if let Some(rest) = tail.strip_prefix('@') {
        // @scope/pkg
        let slash = rest.find('/')?;
        let scoped_end = slash + 1;
        let name_end = rest[scoped_end..]
            .find('/')
            .map(|i| scoped_end + i)
            .unwrap_or(rest.len());
        return Some(format!("@{}", &rest[..name_end]));
    }

    let end = tail.find('/').unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

pub(crate) fn dep_path_tail<'a>(name: &str, dep_path: &'a str) -> &'a str {
    dep_path
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('@'))
        .unwrap_or_else(|| {
            debug_assert!(
                false,
                "dep_path '{dep_path}' does not start with name '{name}'"
            );
            dep_path
        })
}

/// Render a segment list `["foo", "bar"]` as an npm-style install
/// path `node_modules/foo/node_modules/bar`. Empty list → empty
/// string (the root importer key).
pub(crate) fn segments_to_install_path(segs: &[String]) -> String {
    if segs.is_empty() {
        return String::new();
    }
    let mut out = String::from("node_modules/");
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            out.push_str("/node_modules/");
        }
        out.push_str(s);
    }
    out
}

/// Build a hoist + nest tree from a flat [`LockfileGraph`]-derived
/// `canonical` map. Returned keys are segment lists — an empty list
/// is the root importer; `["foo"]` is the hoisted top-level `foo`;
/// `["foo", "bar"]` is a nested `bar` living under `foo` when the
/// version conflict forced it off the top.
///
/// Shared by the npm and bun writers, which both model a hoisted
/// nested `node_modules` layout and differ only in how they render
/// the segment list as a lookup key. Yarn v1 has no nesting and
/// doesn't use this function.
///
/// Algorithm:
///   1. Place each root direct dep at `[name]`.
///   2. BFS: for each placed node, walk its declared deps. For every
///      child, search the ancestor chain for an existing entry —
///      nearest-ancestor first. If an ancestor already carries the
///      right version, the child resolves through that and needs no
///      new entry. If an ancestor has the *wrong* version (or we
///      reach the root empty-handed), try hoisting to `[child]`;
///      if that slot is occupied by a different version, nest at
///      `[...parent, child]`.
///   3. Cycles terminate because each segment-list is placed at most once.
///
/// `reserved_roots` names top-level slots held for something outside
/// `canonical` (a workspace member the root asks for): a package that
/// would hoist there nests under its parent instead, and the slot is
/// left out of the result. A root direct dep of the same name still
/// takes the slot.
pub(crate) fn build_hoist_tree(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: &[DirectDep],
    preferred_placements: Option<&BTreeMap<Vec<String>, String>>,
    reserved_roots: &BTreeSet<String>,
) -> BTreeMap<Vec<String>, String> {
    let mut placed: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut queue: VecDeque<(Vec<String>, String)> = VecDeque::new();
    // An empty key never equals a child's, so `ancestor_resolution` sees
    // a reserved slot as a different version.
    for name in reserved_roots {
        placed.insert(vec![name.clone()], String::new());
    }

    for dep in roots {
        let key = canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&key) {
            continue;
        }
        let segs = vec![dep.name.clone()];
        if placed
            .insert(segs.clone(), key.clone())
            .is_none_or(|previous| previous.is_empty())
        {
            queue.push_back((segs, key));
        }
    }

    // npm preserves an existing valid top-level placement when an install
    // changes an unrelated dependency. Seed those choices before traversing
    // transitives so the first BFS path to a multi-version package does not
    // arbitrarily replace the version npm had already hoisted. Direct roots
    // above remain authoritative, and the reachability gate prevents stale
    // lockfile entries from surviving after their last edge is removed.
    if let Some(preferred_placements) = preferred_placements {
        let reachable = reachable_canonical_keys(canonical, roots);
        for (segs, key) in preferred_placements {
            if segs.len() != 1 || !reachable.contains(key) {
                continue;
            }
            let root_slot = segs.clone();
            if placed.contains_key(&root_slot) {
                continue;
            }
            placed.insert(root_slot.clone(), key.clone());
            queue.push_back((root_slot, key.clone()));
        }
    }

    while let Some((parent_segs, parent_key)) = queue.pop_front() {
        let Some(pkg) = canonical.get(&parent_key).copied() else {
            continue;
        };
        // An existing dependency may live beside its consumer under an
        // ancestor, rather than directly inside the consumer's node_modules.
        if let Some(preferred) = preferred_placements {
            for (child_name, child_value) in &pkg.dependencies {
                let child_key = child_canonical_key(child_name, child_value);
                let root_slot = vec![child_name.clone()];
                let root_unchanged = preferred
                    .get(&root_slot)
                    .is_none_or(|key| placed.get(&root_slot) == Some(key));
                if !root_unchanged || !canonical.contains_key(&child_key) {
                    continue;
                }
                for depth in (1..=parent_segs.len()).rev() {
                    let mut nested = parent_segs[..depth].to_vec();
                    nested.push(child_name.clone());
                    if let Some(preferred_key) = preferred.get(&nested) {
                        // A placed descendant that resolves this name through a
                        // different (usually hoisted) copy would find the ancestor
                        // slot first under Node's lookup.
                        let shadows_descendant = placed.iter().any(|(path, key)| {
                            path.len() > depth
                                && path[..depth] == nested[..depth]
                                && canonical.get(key).is_some_and(|descendant| {
                                    descendant
                                        .dependencies
                                        .get(child_name)
                                        .is_some_and(|value| {
                                            child_canonical_key(child_name, value) != child_key
                                        })
                                })
                                && !(depth + 1..=path.len()).any(|len| {
                                    let mut slot = path[..len].to_vec();
                                    slot.push(child_name.clone());
                                    placed.contains_key(&slot)
                                })
                        });
                        if preferred_key == &child_key
                            && !placed.contains_key(&nested)
                            && !shadows_descendant
                        {
                            placed.insert(nested.clone(), child_key.clone());
                            queue.push_back((nested, child_key.clone()));
                        }
                        break;
                    }
                }
            }
        }
        let mut child_entries: Vec<(String, String)> = Vec::new();
        for (child_name, child_value) in &pkg.dependencies {
            let child_key = child_canonical_key(child_name, child_value);
            if !canonical.contains_key(&child_key) {
                continue;
            }
            child_entries.push((child_name.clone(), child_key));
        }

        for (child_name, child_key) in child_entries {
            match ancestor_resolution(&parent_segs, &child_name, &child_key, &placed) {
                AncestorHit::Match => continue,
                AncestorHit::Shadowed => {
                    // An intermediate ancestor carries a *different*
                    // version of `child_name`, which shadows anything
                    // at root. Node's runtime walk would stop at the
                    // ancestor and resolve the wrong version, so we
                    // must place a new entry directly inside the
                    // parent's own `node_modules` to short-circuit
                    // the shadow. Never fall through to the root-slot
                    // logic here, even if root happens to already
                    // carry the right version.
                    let mut nested = parent_segs.clone();
                    nested.push(child_name.clone());
                    if placed.insert(nested.clone(), child_key.clone()).is_none() {
                        queue.push_back((nested, child_key));
                    }
                }
                AncestorHit::Miss => {
                    // Ancestor chain is empty (including root). Hoist.
                    // Today the walk guarantees the root slot is empty
                    // when we get here, so `.is_none()` always holds —
                    // but match the `Shadowed` branch's insert-guard
                    // pattern exactly so a future change to when Miss
                    // is returned can't silently introduce duplicate
                    // queue entries or an unguarded overwrite.
                    let root_slot = vec![child_name.clone()];
                    if placed
                        .insert(root_slot.clone(), child_key.clone())
                        .is_none()
                    {
                        queue.push_back((root_slot, child_key));
                    }
                }
            }
        }
    }

    placed.retain(|_, key| !key.is_empty());
    placed
}

fn reachable_canonical_keys(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: &[DirectDep],
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();
    for dep in roots {
        let key = canonical_key_from_dep_path(&dep.dep_path);
        if canonical.contains_key(&key) && reachable.insert(key.clone()) {
            queue.push_back(key);
        }
    }
    while let Some(key) = queue.pop_front() {
        let Some(pkg) = canonical.get(&key).copied() else {
            continue;
        };
        for (child_name, child_value) in &pkg.dependencies {
            let child_key = child_canonical_key(child_name, child_value);
            if canonical.contains_key(&child_key) && reachable.insert(child_key.clone()) {
                queue.push_back(child_key);
            }
        }
    }
    reachable
}

/// Three-way result of an ancestor-chain lookup. Differentiating
/// `Miss` (nothing anywhere — safe to hoist) from `Shadowed` (a
/// wrong-version ancestor blocks hoisting and forces a nested
/// placement) is load-bearing: conflating them caused a real bug
/// where an intermediate ancestor carrying the wrong version would
/// silently shadow a correct root entry at runtime.
enum AncestorHit {
    Match,
    Shadowed,
    Miss,
}

/// Walk the ancestor chain of `parent_segs` nearest-first looking
/// for an entry named `child_name`, and classify the first hit
/// against `child_key`. `Match` iff the nearest hit equals
/// `child_key`; `Shadowed` iff it's a different version; `Miss` iff
/// the entire chain (including root) is empty.
fn ancestor_resolution(
    parent_segs: &[String],
    child_name: &str,
    child_key: &str,
    placed: &BTreeMap<Vec<String>, String>,
) -> AncestorHit {
    // Candidate layering, nearest first:
    //   parent_segs + [child]
    //   parent_segs[..-1] + [child]
    //   ...
    //   [child]  (root)
    for i in (0..=parent_segs.len()).rev() {
        let mut candidate: Vec<String> = parent_segs[..i].to_vec();
        candidate.push(child_name.to_string());
        if let Some(existing) = placed.get(&candidate) {
            return if existing == child_key {
                AncestorHit::Match
            } else {
                AncestorHit::Shadowed
            };
        }
    }
    AncestorHit::Miss
}

/// Strip any peer-context suffix from a dep_path tail, returning just
/// the version. Both the normal `(peer@ver)…` form and the capped
/// `(<short-hash>)` form (pnpm's `createPeerDepGraphHash`) begin at the
/// first `(`, so cutting there handles either. Input
/// `"18.2.0(prop-types@15.8.1)"` → `"18.2.0"`.
fn version_from_tail(tail: &str) -> &str {
    tail.split_once('(').map(|(v, _)| v).unwrap_or(tail)
}

/// Compute the canonical `name@version` key for a child declared in
/// [`LockedPackage::dependencies`]. Tolerates both encodings seen in
/// practice: the documented "tail only" form (`"1.0.0"`) used by
/// `pnpm::parse` *and* the "full dep_path" form (`"bar@1.0.0"`)
/// currently emitted by [`parse`] above. Peer context suffixes are
/// stripped in both branches.
pub(crate) fn child_canonical_key(child_name: &str, value: &str) -> String {
    let no_peer = version_from_tail(value);
    let prefix = format!("{child_name}@");
    if no_peer.starts_with(&prefix) {
        no_peer.to_string()
    } else {
        format!("{prefix}{no_peer}")
    }
}

/// Render a child dep value back as a bare version string, regardless
/// of which encoding it was stored in. Used when writing out the
/// `dependencies` field of a nested package entry.
pub(crate) fn dep_value_as_version<'a>(child_name: &str, value: &'a str) -> &'a str {
    let no_peer = version_from_tail(value);
    let prefix = format!("{child_name}@");
    if let Some(rest) = no_peer.strip_prefix(&prefix) {
        rest
    } else {
        no_peer
    }
}

/// Extract `"name@version"` from a full dep_path, dropping any peer
/// context suffix. Strips the `(peer@ver)` tail *first* so the
/// `rfind('@')` that separates name from version can't land inside
/// the peer suffix — e.g. `"foo@1.0.0(react@18.2.0)"` must resolve
/// to `"foo@1.0.0"`, not `"foo@1.0.0(react@18.2.0)"` (which would
/// then miss the canonical map and silently drop the package from
/// the written lockfile).
pub(crate) fn canonical_key_from_dep_path(dep_path: &str) -> String {
    let trimmed = version_from_tail(dep_path);
    let (name, version) = match trimmed.rfind('@') {
        Some(0) | None => return trimmed.to_string(),
        Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
    };
    format!("{name}@{version}")
}
