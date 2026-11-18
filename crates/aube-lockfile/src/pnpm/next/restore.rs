//! Rebuilding the peer-suffixed snapshot keys of a v13 lockfile, ported
//! from pnpm's `next_format/read`.
//!
//! 1. Each reference is matched to one snapshot of the package it names.
//!    A package with one snapshot needs no match. A reference that names
//!    peers picks the snapshot that binds them. Any other reference picks
//!    the snapshot whose bindings all agree with the referrer's scope and
//!    that binds the most names.
//! 2. Each peer binding is matched to its provider, the snapshot that the
//!    scope above the binding snapshot links under the peer's name. A
//!    snapshot's scope is that of the referrer that first reaches it, in a
//!    walk from the importers in order.
//! 3. Each snapshot's key is rebuilt from its providers with the
//!    resolver's peer-suffix rules.

use super::refs::{DepRef, ImporterVersion, Key, NamedBindings, NextRef, names_binding};
use super::{Entry, Importer, Package, Rules, SnapshotOut, tarjan};
use sha2::{Digest, Sha256};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub(super) struct Input<'a> {
    pub snapshots: Option<&'a BTreeMap<Key, Vec<Entry>>>,
    pub importers: &'a BTreeMap<String, Importer>,
    pub packages: &'a BTreeMap<Key, Package>,
    pub patched: &'a BTreeMap<String, String>,
    pub rules: &'a Rules,
}

/// The rebuilt v9 snapshots, and the keys the importers point at.
pub(super) struct Restored {
    pub snapshots: Vec<(Key, SnapshotOut)>,
    importer_keys: HashMap<String, Vec<(String, Key)>>,
    /// Each package's first snapshot and its count.
    by_package: HashMap<Key, (Sid, usize)>,
    /// Each snapshot's chosen children and the providers of its peers.
    linked: Vec<HashMap<String, Sid>>,
}

impl Restored {
    /// The v9 version of the importer dependency `alias` of `id`.
    pub fn importer_version(
        &self,
        id: &str,
        alias: &str,
        version: &ImporterVersion,
    ) -> ImporterVersion {
        self.importer_keys
            .get(id)
            .and_then(|children| {
                children
                    .binary_search_by(|(name, _)| compare_names(name, alias))
                    .ok()
                    .map(|at| &children[at].1)
            })
            .map_or_else(|| version.clone(), |key| version.with_key(key))
    }

    /// The key of the importer dependency `alias` of `id`.
    pub fn importer_key(&self, id: &str, alias: &str) -> Option<&Key> {
        let children = self.importer_keys.get(id)?;
        children
            .binary_search_by(|(name, _)| compare_names(name, alias))
            .ok()
            .map(|at| &children[at].1)
    }

    fn sid(&self, package: &Key, index: usize) -> Option<Sid> {
        let &(start, count) = self.by_package.get(package)?;
        (index < count).then_some(start + index)
    }

    /// The rebuilt key of the snapshot at `index` of `package`.
    pub fn snapshot_key(&self, package: &Key, index: usize) -> Option<&Key> {
        self.sid(package, index).map(|sid| &self.snapshots[sid].0)
    }

    /// The rebuilt key of what that snapshot links as `alias`: a child it
    /// chose, or the provider of a peer it binds.
    pub fn child_key(&self, package: &Key, index: usize, alias: &str) -> Option<&Key> {
        let sid = self.sid(package, index)?;
        self.linked[sid]
            .get(alias)
            .map(|&child| &self.snapshots[child].0)
    }
}

pub(super) fn restore(input: &Input<'_>) -> Restored {
    let Some(snapshots) = input.snapshots else {
        return Restored {
            snapshots: Vec::new(),
            importer_keys: HashMap::new(),
            by_package: HashMap::new(),
            linked: Vec::new(),
        };
    };
    let graph = Graph::new(snapshots, input.importers);
    let keys = graph.keys(input);
    let unbound = graph.unbound_peers(input.packages);
    let snapshots = (0..graph.keys.len())
        .map(|sid| {
            let below = graph.unbound_below(sid, &unbound, input.packages);
            (
                keys[sid].clone(),
                graph.snapshot_out(sid, &keys, input.packages, &below),
            )
        })
        .collect();
    let importer_keys = graph
        .importer_children
        .iter()
        .map(|(&id, children)| {
            let children = children
                .iter()
                .map(|&(alias, sid)| (alias.to_string(), keys[sid].clone()))
                .collect();
            (id.to_string(), children)
        })
        .collect();
    let linked = (0..graph.keys.len())
        .map(|sid| {
            let providers =
                graph.providers[sid]
                    .iter()
                    .filter_map(|(name, provider)| match provider {
                        Provider::Snapshot(provider) => Some((name.to_string(), *provider)),
                        Provider::Link(_) | Provider::Missing(_) => None,
                    });
            let children = graph.children[sid]
                .iter()
                .map(|&(name, child)| (name.to_string(), child));
            providers.chain(children).collect()
        })
        .collect();
    Restored {
        snapshots,
        importer_keys,
        by_package: graph
            .by_package
            .iter()
            .map(|(&key, &place)| (key.clone(), place))
            .collect(),
        linked,
    }
}

type Sid = usize;

#[derive(Clone, Copy)]
enum Referrer<'a> {
    Importer(&'a str),
    Snapshot(Sid),
}

enum Provider {
    Snapshot(Sid),
    Link(String),
    /// The lockfile has no snapshot of the provider.
    Missing(String),
}

struct Graph<'a> {
    keys: Vec<&'a Key>,
    entries: Vec<&'a Entry>,
    /// Each package's first snapshot and its count.
    by_package: HashMap<&'a Key, (Sid, usize)>,
    importers: &'a BTreeMap<String, Importer>,
    /// Each snapshot's chosen children, sorted by name.
    children: Vec<Vec<(&'a str, Sid)>>,
    importer_children: BTreeMap<&'a str, Vec<(&'a str, Sid)>>,
    /// The referrer that first reaches each snapshot: the scope its peers
    /// are bound in.
    parent: Vec<Option<Referrer<'a>>>,
    providers: Vec<Vec<(&'a str, Provider)>>,
}

impl<'a> Graph<'a> {
    fn new(
        snapshots: &'a BTreeMap<Key, Vec<Entry>>,
        importers: &'a BTreeMap<String, Importer>,
    ) -> Self {
        let mut graph = Graph {
            keys: Vec::new(),
            entries: Vec::new(),
            by_package: HashMap::with_capacity(snapshots.len()),
            importers,
            children: Vec::new(),
            importer_children: BTreeMap::new(),
            parent: Vec::new(),
            providers: Vec::new(),
        };
        for (key, list) in snapshots {
            graph.by_package.insert(key, (graph.keys.len(), list.len()));
            for entry in list {
                graph.keys.push(key);
                graph.entries.push(entry);
            }
        }
        graph.children = (0..graph.keys.len())
            .map(|sid| graph.choose_children(sid))
            .collect();
        graph.importer_children = importers
            .iter()
            .map(|(id, importer)| (id.as_str(), graph.choose_importer_children(id, importer)))
            .collect();
        graph.parent = graph.parents();
        graph.providers = (0..graph.keys.len())
            .map(|sid| {
                graph.entries[sid]
                    .bindings()
                    .map(|(name, reference)| (name.as_str(), graph.provider(sid, name, reference)))
                    .collect()
            })
            .collect();
        graph
    }

    // Choosing a snapshot for each reference.

    fn choose_children(&self, sid: Sid) -> Vec<(&'a str, Sid)> {
        let entry = self.entries[sid];
        let mut children: Vec<(&'a str, Sid)> = entry
            .dependencies
            .iter()
            .chain(&entry.optional_dependencies)
            .filter_map(|(alias, reference)| {
                let key = reference.version().resolve(alias)?;
                let chosen = self.choose(&key, reference.peers(), Referrer::Snapshot(sid))?;
                Some((alias.as_str(), chosen))
            })
            .collect();
        children.sort_by(|left, right| compare_names(left.0, right.0));
        children
    }

    fn choose_importer_children(&self, id: &'a str, importer: &'a Importer) -> Vec<(&'a str, Sid)> {
        let mut children: Vec<(&'a str, Sid)> = importer
            .resolved()
            .filter_map(|(alias, dep)| {
                let key = dep.version.resolved_key(alias)?;
                let chosen = self.choose(&key, dep.peers.as_ref(), Referrer::Importer(id))?;
                Some((alias.as_str(), chosen))
            })
            .collect();
        children.sort_by(|left, right| compare_names(left.0, right.0));
        children
    }

    /// The snapshot `key` names from `referrer`, or `None` when the
    /// lockfile has none, as for a skipped optional dependency.
    fn choose(
        &self,
        key: &Key,
        peers: Option<&NamedBindings>,
        referrer: Referrer<'a>,
    ) -> Option<Sid> {
        let &(start, count) = self.by_package.get(&key.without_peer())?;
        if count == 1 {
            return Some(start);
        }
        if let Some(peers) = peers
            && let Some(sid) = self.snapshot_with_bindings(start, count, peers)
        {
            return Some(sid);
        }
        Some(self.snapshot_in_scope(start, count, referrer))
    }

    /// The snapshot that binds every named peer, with the fewest bindings.
    fn snapshot_with_bindings(
        &self,
        start: Sid,
        count: usize,
        peers: &NamedBindings,
    ) -> Option<Sid> {
        (start..start + count)
            .filter(|&sid| {
                peers.iter().all(|(name, wanted)| {
                    self.entries[sid]
                        .bindings()
                        .any(|(bound, reference)| bound == name && names_binding(wanted, reference))
                })
            })
            .min_by_key(|&sid| self.entries[sid].bindings().count())
    }

    /// The snapshot none of whose bindings disagrees with the referrer's
    /// scope, agreeing on the most names, then binding the fewest names the
    /// scope lacks. The first snapshot when every one disagrees.
    fn snapshot_in_scope(&self, start: Sid, count: usize, referrer: Referrer<'a>) -> Sid {
        (start..start + count)
            .filter_map(|sid| Some((sid, self.agreement(sid, referrer)?)))
            .min_by_key(|&(sid, (agreeing, absent))| (Reverse(agreeing), absent, sid))
            .map_or(start, |(sid, _)| sid)
    }

    /// How many bindings of `sid` the referrer's scope agrees with and
    /// lacks, or `None` when it disagrees with one.
    fn agreement(&self, sid: Sid, referrer: Referrer<'a>) -> Option<(usize, usize)> {
        let (mut agreeing, mut absent) = (0, 0);
        for (name, reference) in self.entries[sid].bindings() {
            match self.binding_in_scope(referrer, name, reference) {
                Some(true) => agreeing += 1,
                None => absent += 1,
                Some(false) => return None,
            }
        }
        Some((agreeing, absent))
    }

    /// Whether the referrer's scope agrees with a binding, or `None` when
    /// it links nothing under the name.
    fn binding_in_scope(
        &self,
        referrer: Referrer<'a>,
        name: &str,
        reference: &NextRef,
    ) -> Option<bool> {
        match referrer {
            Referrer::Snapshot(sid) => match self.entries[sid].reference_in_scope(name) {
                Some(scoped) => Some(same_target(name, scoped, reference)),
                None if self.is_itself(sid, name, reference) => Some(true),
                None => None,
            },
            Referrer::Importer(id) => self.importers[id]
                .resolved()
                .find(|(alias, _)| *alias == name)
                .map(|(_, dep)| importer_target_is(name, &dep.version, reference)),
        }
    }

    /// Whether `reference` names `sid` itself: a package is in the scope
    /// of its own dependencies, which can have it as a peer.
    fn is_itself(&self, sid: Sid, name: &str, reference: &NextRef) -> bool {
        self.keys[sid].name == name
            && reference
                .version()
                .resolve(name)
                .map(|key| key.without_peer())
                .as_ref()
                == Some(self.keys[sid])
    }

    // The scope of each snapshot and the providers it binds.

    /// The first referrer of each snapshot, in a walk from the importers
    /// in order. A snapshot no importer reaches starts a walk of its own.
    fn parents(&self) -> Vec<Option<Referrer<'a>>> {
        let mut walk = Walk {
            graph: self,
            parent: vec![None; self.keys.len()],
            visited: vec![false; self.keys.len()],
            queue: Vec::new(),
        };
        for (&id, children) in &self.importer_children {
            for &(_, child) in children {
                walk.reach(child, Referrer::Importer(id));
            }
            walk.drain();
        }
        let mut unreached: Vec<Sid> = (0..self.keys.len())
            .filter(|&sid| walk.parent[sid].is_none())
            .collect();
        unreached.sort_by_cached_key(|&sid| {
            (
                self.keys[sid].to_string(),
                sid - self.by_package[self.keys[sid]].0,
            )
        });
        for sid in unreached {
            walk.queue.push(sid);
            walk.drain();
        }
        walk.parent
    }

    fn provider(&self, sid: Sid, name: &str, reference: &NextRef) -> Provider {
        if let Some(path) = reference.version().as_link_target() {
            return Provider::Link(path.to_string());
        }
        let Some(key) = reference.version().resolve(name) else {
            return Provider::Missing(reference.version().to_string());
        };
        let Some(&(start, count)) = self.by_package.get(&key.without_peer()) else {
            return Provider::Missing(format!("{}@{}", key.name, key.suffix.without_peer()));
        };
        if count == 1 {
            return Provider::Snapshot(start);
        }
        if let Some(peers) = reference.peers()
            && let Some(found) = self.snapshot_with_bindings(start, count, peers)
        {
            return Provider::Snapshot(found);
        }
        Provider::Snapshot(
            self.linked_above(sid, name, start, count)
                .unwrap_or_else(|| self.snapshot_in_scope(start, count, Referrer::Snapshot(sid))),
        )
    }

    /// The snapshot among `start..start + count` linked as `name` in the
    /// scope `sid` was reached in: under its parent, its parent's parent,
    /// and so on up to an importer.
    fn linked_above(&self, sid: Sid, name: &str, start: Sid, count: usize) -> Option<Sid> {
        let mut referrer = self.parent[sid];
        let mut steps = 0;
        while let Some(current) = referrer {
            let linked = match current {
                Referrer::Snapshot(parent) => child_named(&self.children[parent], name)
                    .or_else(|| (self.keys[parent].name == name).then_some(parent)),
                Referrer::Importer(id) => child_named(&self.importer_children[id], name),
            };
            if let Some(found) = linked.filter(|found| (start..start + count).contains(found)) {
                return Some(found);
            }
            referrer = match current {
                Referrer::Snapshot(parent) => self.parent[parent],
                Referrer::Importer(_) => None,
            };
            steps += 1;
            if steps > self.keys.len() {
                return None;
            }
        }
        None
    }

    // Keys.

    fn keys(&self, input: &Input<'_>) -> Vec<Key> {
        let patches = Patches::new(input.patched, input.packages);
        if input.rules.dedupe {
            return (0..self.keys.len())
                .map(|sid| {
                    self.key_of(sid, input.rules, &patches, |_, provider| {
                        pair_of(self.keys[provider])
                    })
                })
                .collect();
        }
        let order = self.providers_first();
        let cyclic_names = self.cyclic_peer_names();
        let collapses = |sid: Sid, provider: Sid, name: &str| {
            let component = order.component_of[sid];
            (component.is_some() && component == order.component_of[provider])
                || cyclic_names.contains(name)
        };
        let mut keys: Vec<Option<Key>> = (0..self.keys.len())
            .map(|sid| {
                self.providers[sid]
                    .is_empty()
                    .then(|| self.base_key(sid, &patches))
            })
            .collect();
        // A component comes after the components of its providers, so their
        // keys are ready. A provider in the same component collapses.
        for component in &order.components {
            for &sid in component {
                if self.providers[sid].is_empty() {
                    continue;
                }
                let key = self.key_of(sid, input.rules, &patches, |name, provider| {
                    match &keys[provider] {
                        Some(key) if !collapses(sid, provider, name) => {
                            PeerId::DepPath(key.to_string())
                        }
                        _ => pair_of(self.keys[provider]),
                    }
                });
                keys[sid] = Some(key);
            }
        }
        keys.into_iter()
            .zip(&self.keys)
            .map(|(key, base)| key.unwrap_or_else(|| (*base).clone()))
            .collect()
    }

    /// `name@version` of `sid`, with its patch group: the key of a snapshot
    /// that binds no peers.
    fn base_key(&self, sid: Sid, patches: &Patches<'_>) -> Key {
        let key = self.keys[sid];
        Key::new(&key.name, key.suffix.with_peer(patches.group(key)))
    }

    fn key_of(
        &self,
        sid: Sid,
        rules: &Rules,
        patches: &Patches<'_>,
        segment: impl Fn(&str, Sid) -> PeerId,
    ) -> Key {
        if self.providers[sid].is_empty() {
            return self.base_key(sid, patches);
        }
        let ids: Vec<PeerId> = self.providers[sid]
            .iter()
            .map(|(name, provider)| match provider {
                Provider::Link(path) => {
                    PeerId::Pair(name.to_string(), link_path_to_peer_version(path))
                }
                Provider::Missing(pair) => PeerId::DepPath(pair.clone()),
                Provider::Snapshot(provider) => segment(name, *provider),
            })
            .collect();
        let key = self.keys[sid];
        let mut peer = patches.group(key);
        peer.push_str(&peer_dep_graph_hash(&ids, rules.max_length));
        Key::new(&key.name, key.suffix.with_peer(peer))
    }

    /// Snapshots grouped by peer cycle, each group after the groups of its
    /// providers. Only snapshots with bindings form cycles.
    fn providers_first(&self) -> tarjan::Components {
        tarjan::components(
            self.keys.len(),
            |sid| self.provider_snapshots(sid).collect(),
            |sid| !self.providers[sid].is_empty(),
        )
    }

    fn provider_snapshots(&self, sid: Sid) -> impl Iterator<Item = Sid> + '_ {
        self.providers[sid]
            .iter()
            .filter_map(|(_, provider)| match provider {
                Provider::Snapshot(provider) => Some(*provider),
                Provider::Link(_) | Provider::Missing(_) => None,
            })
    }

    /// Peer names on a cycle of the graph from a package's name to the
    /// names of the peers it binds.
    fn cyclic_peer_names(&self) -> HashSet<&'a str> {
        let mut names: Vec<&'a str> = Vec::new();
        let mut index: HashMap<&'a str, usize> = HashMap::new();
        let mut edges: Vec<BTreeSet<usize>> = Vec::new();
        let mut intern = |name: &'a str, edges: &mut Vec<BTreeSet<usize>>| {
            *index.entry(name).or_insert_with(|| {
                names.push(name);
                edges.push(BTreeSet::new());
                names.len() - 1
            })
        };
        for sid in 0..self.keys.len() {
            if self.providers[sid].is_empty() {
                continue;
            }
            let from = intern(&self.keys[sid].name, &mut edges);
            for &(name, _) in &self.providers[sid] {
                let to = intern(name, &mut edges);
                edges[from].insert(to);
            }
        }
        let components = tarjan::components(
            edges.len(),
            |node| edges[node].iter().copied().collect(),
            |_| true,
        );
        components
            .components
            .iter()
            .filter(|component| match component.as_slice() {
                [only] => edges[*only].contains(only),
                _ => true,
            })
            .flatten()
            .map(|&node| names[node])
            .collect()
    }

    // Unbound peers.

    /// For each snapshot, the peers it and the packages below it declare
    /// and nothing binds. A dependency cycle shares one set.
    fn unbound_peers(&self, packages: &'a BTreeMap<Key, Package>) -> Vec<BTreeSet<&'a str>> {
        let own: Vec<Vec<&'a str>> = (0..self.keys.len())
            .map(|sid| self.own_unbound_peers(sid, packages))
            .collect();
        let order = tarjan::components(
            self.keys.len(),
            |sid| self.children[sid].iter().map(|&(_, child)| child).collect(),
            |_| true,
        );
        let mut below: Vec<BTreeSet<&'a str>> = vec![BTreeSet::new(); self.keys.len()];
        for component in &order.components {
            let mut set = BTreeSet::new();
            for &member in component {
                set.extend(own[member].iter().copied());
                for &(_, child) in &self.children[member] {
                    if order.component_of[child] != order.component_of[member] {
                        set.extend(below[child].iter().copied());
                    }
                }
            }
            for &member in component {
                below[member].clone_from(&set);
            }
        }
        below
    }

    /// The peers the package declares that the snapshot neither binds nor
    /// installs as a dependency of its own.
    fn own_unbound_peers(&self, sid: Sid, packages: &'a BTreeMap<Key, Package>) -> Vec<&'a str> {
        let Some(package) = packages.get(self.keys[sid]) else {
            return Vec::new();
        };
        let entry = self.entries[sid];
        package
            .peer_ranges
            .keys()
            .filter(|name| {
                !(entry.own.contains_key(*name)
                    || entry.dependencies.contains_key(*name)
                    || entry.optional_dependencies.contains_key(*name))
            })
            .map(String::as_str)
            .collect()
    }

    /// The unbound peers below `sid`, less the peers it declares itself,
    /// which its dependents list instead.
    fn unbound_below(
        &self,
        sid: Sid,
        unbound: &[BTreeSet<&'a str>],
        packages: &BTreeMap<Key, Package>,
    ) -> Vec<String> {
        let declared = packages
            .get(self.keys[sid])
            .map(|package| &package.peer_ranges);
        unbound[sid]
            .iter()
            .filter(|name| !declared.is_some_and(|declared| declared.contains_key(**name)))
            .map(|name| name.to_string())
            .collect()
    }

    // Output.

    fn snapshot_out(
        &self,
        sid: Sid,
        keys: &[Key],
        packages: &BTreeMap<Key, Package>,
        unbound_below: &[String],
    ) -> SnapshotOut {
        let entry = self.entries[sid];
        let mut dependencies = self.restored_children(sid, &entry.dependencies, keys);
        let mut optional_dependencies =
            self.restored_children(sid, &entry.optional_dependencies, keys);
        let optional_peers = packages
            .get(self.keys[sid])
            .map(|package| &package.optional_peers);
        for (name, reference) in &entry.own {
            let target = if optional_peers.is_some_and(|peers| peers.contains(name)) {
                &mut optional_dependencies
            } else {
                &mut dependencies
            };
            let provider = self.providers[sid]
                .iter()
                .find(|(bound, _)| bound == name)
                .map(|(_, provider)| provider);
            let restored = match provider {
                Some(Provider::Snapshot(provider)) => {
                    reference.version().with_key(&keys[*provider])
                }
                _ => reference.version().clone(),
            };
            target.insert(name.clone(), restored.to_string());
        }
        let mut transitive: Vec<String> = entry.below.keys().cloned().collect();
        match &entry.unbound {
            Some(unbound) => transitive.extend(unbound.iter().cloned()),
            None => transitive.extend(unbound_below.iter().cloned()),
        }
        transitive.sort();
        transitive.dedup();
        SnapshotOut {
            dependencies,
            optional_dependencies,
            transitive_peer_dependencies: transitive,
            optional: entry.optional,
        }
    }

    /// `references` with each one pointing at the snapshot chosen for it.
    fn restored_children(
        &self,
        sid: Sid,
        references: &BTreeMap<String, NextRef>,
        keys: &[Key],
    ) -> BTreeMap<String, String> {
        references
            .iter()
            .map(|(alias, reference)| {
                let restored: DepRef = match child_named(&self.children[sid], alias) {
                    Some(child) => reference.version().with_key(&keys[child]),
                    None => reference.version().clone(),
                };
                (alias.clone(), restored.to_string())
            })
            .collect()
    }
}

/// A walk from the importers that records the first referrer of each
/// snapshot.
struct Walk<'g, 'a> {
    graph: &'g Graph<'a>,
    parent: Vec<Option<Referrer<'a>>>,
    visited: Vec<bool>,
    queue: Vec<Sid>,
}

impl<'a> Walk<'_, 'a> {
    fn reach(&mut self, sid: Sid, referrer: Referrer<'a>) {
        self.parent[sid].get_or_insert(referrer);
        self.queue.push(sid);
    }

    fn drain(&mut self) {
        let graph = self.graph;
        while let Some(sid) = self.queue.pop() {
            if std::mem::replace(&mut self.visited[sid], true) {
                continue;
            }
            for &(_, child) in &graph.children[sid] {
                if !self.visited[child] {
                    self.reach(child, Referrer::Snapshot(sid));
                }
            }
        }
    }
}

fn same_target(name: &str, left: &NextRef, right: &NextRef) -> bool {
    match (
        left.version().as_link_target(),
        right.version().as_link_target(),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => {
            let target = |reference: &NextRef| {
                reference
                    .version()
                    .resolve(name)
                    .map(|key| key.without_peer())
            };
            target(left) == target(right)
        }
        _ => false,
    }
}

fn importer_target_is(name: &str, version: &ImporterVersion, reference: &NextRef) -> bool {
    match (
        version.as_link_target(),
        reference.version().as_link_target(),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => {
            let own = version.resolved_key(name).map(|key| key.without_peer());
            own.is_some()
                && own
                    == reference
                        .version()
                        .resolve(name)
                        .map(|key| key.without_peer())
        }
        _ => false,
    }
}

/// pnpm's order of package names: unscoped names first, then by scope,
/// then by the bare name.
fn compare_names(left: &str, right: &str) -> Ordering {
    let split = |name: &str| -> (Option<String>, String) {
        match name.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
            Some((scope, bare)) => (Some(scope.to_string()), bare.to_string()),
            None => (None, name.to_string()),
        }
    };
    split(left).cmp(&split(right))
}

fn child_named(children: &[(&str, Sid)], name: &str) -> Option<Sid> {
    children
        .binary_search_by(|(alias, _)| compare_names(alias, name))
        .ok()
        .map(|at| children[at].1)
}

/// One peer of a snapshot, as its key's peer suffix writes it.
enum PeerId {
    /// `name@version`.
    Pair(String, String),
    /// A full key.
    DepPath(String),
}

/// `name@version` of a provider, as the resolver writes a collapsed peer.
fn pair_of(key: &Key) -> PeerId {
    PeerId::Pair(key.name.clone(), key.suffix.without_peer().to_string())
}

/// pnpm's `createPeerDepGraphHash`: the sorted peers as `(a)(b)`, or a
/// hash of them when that is longer than `max_length`.
fn peer_dep_graph_hash(ids: &[PeerId], max_length: usize) -> String {
    let mut rendered: Vec<String> = ids
        .iter()
        .map(|id| match id {
            PeerId::Pair(name, version) => format!("{name}@{version}"),
            PeerId::DepPath(path) => path.strip_prefix('/').unwrap_or(path).to_string(),
        })
        .collect();
    rendered.sort();
    let body = rendered.join(")(");
    if body.len() > max_length {
        let digest = Sha256::digest(body.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("({})", &hex[..32])
    } else {
        format!("({body})")
    }
}

/// pnpm's `linkPathToPeerVersion`: a link path made safe for a peer
/// suffix.
fn link_path_to_peer_version(path: &str) -> String {
    let trimmed = path.trim_start_matches('.');
    let mut out = String::with_capacity(trimmed.len());
    let mut last_was_plus = true;
    for ch in trimmed.chars() {
        if ch.is_control() || "\"*+/:<>?\\|".contains(ch) {
            if !last_was_plus {
                out.push('+');
                last_was_plus = true;
            }
        } else {
            out.push(ch);
            last_was_plus = false;
        }
    }
    let out = out.trim_end_matches(['+', '.']);
    if out.is_empty() && !path.is_empty() {
        "+".to_string()
    } else {
        out.to_string()
    }
}

/// The patch groups of `patchedDependencies`, matched to packages the way
/// pnpm's resolver matches them.
struct Patches<'a> {
    /// Package name → its exact versions, ranges and catch-all hash.
    groups: Option<HashMap<&'a str, PatchGroup<'a>>>,
    packages: &'a BTreeMap<Key, Package>,
}

#[derive(Default)]
struct PatchGroup<'a> {
    exact: HashMap<&'a str, &'a str>,
    ranges: Vec<(node_semver::Range, &'a str)>,
    all: Option<&'a str>,
}

impl<'a> Patches<'a> {
    fn new(patched: &'a BTreeMap<String, String>, packages: &'a BTreeMap<Key, Package>) -> Self {
        let mut groups: HashMap<&str, PatchGroup<'_>> = HashMap::new();
        let mut valid = true;
        for (key, hash) in patched {
            let Some(at) = super::refs::version_separator_index(key) else {
                groups.entry(key).or_default().all = Some(hash);
                continue;
            };
            let (name, spec) = (&key[..at], &key[at + 1..]);
            let group = groups.entry(name).or_default();
            if node_semver::Version::parse(spec).is_ok() {
                group.exact.insert(spec, hash);
            } else if let Ok(range) = node_semver::Range::parse(spec) {
                group.ranges.push((range, hash));
            } else {
                valid = false;
            }
        }
        Patches {
            groups: (valid && !groups.is_empty()).then_some(groups),
            packages,
        }
    }

    /// `(patch_hash=…)` for a patched package, or nothing.
    fn group(&self, key: &Key) -> String {
        let Some(groups) = &self.groups else {
            return String::new();
        };
        let package_key = key.without_peer();
        let version = self
            .packages
            .get(&package_key)
            .and_then(|package| package.version.clone())
            .unwrap_or_else(|| package_key.suffix.version.clone());
        let Some(group) = groups.get(key.name.as_str()) else {
            return String::new();
        };
        let hash = group.exact.get(version.as_str()).copied().or_else(|| {
            let parsed = node_semver::Version::parse(&version).ok();
            let matching: Vec<&str> = group
                .ranges
                .iter()
                .filter(|(range, _)| {
                    parsed
                        .as_ref()
                        .is_some_and(|version| range.satisfies(version))
                })
                .map(|&(_, hash)| hash)
                .collect();
            match matching.as_slice() {
                [] => group.all,
                [only] => Some(*only),
                _ => None,
            }
        });
        hash.map_or_else(String::new, |hash| format!("(patch_hash={hash})"))
    }
}
