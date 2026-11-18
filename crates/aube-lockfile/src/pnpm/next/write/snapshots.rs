//! The lockfile's snapshots grouped by package, and the v13 sections built
//! from them. Ported from pnpm's `write/snapshots.rs`,
//! `write/snapshots/naming.rs` and `write/importers.rs`.

use super::super::Entry;
use super::super::refs::{DepRef, ImporterVersion, Key, NamedBindings, NextRef, VerPeer};
use super::super::restore::Restored;
use super::super::tarjan;
use super::providers::{
    Constraint, ProviderFinder, all_groups_named, own_peer_names, provider_segments,
    transitive_named_in_key,
};
use super::{DepIn, Explicit, ImporterIn, Lockfile, SnapshotIn};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// How deep a reference's `with` may nest.
const MAX_NESTED_REFERENCES: usize = 32;

/// How many bindings the `with` of one reference may hold in all.
const MAX_BINDINGS: usize = 4096;

/// One snapshot of the lockfile, split into the v13 maps.
struct Snapshot<'a> {
    key: &'a Key,
    entry: &'a SnapshotIn,
    own_peers: BTreeSet<&'a str>,
    transitive: Vec<(String, DepRef)>,
}

impl Snapshot<'_> {
    fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.own_peers
            .iter()
            .copied()
            .chain(self.transitive.iter().map(|(name, _)| name.as_str()))
    }

    /// The peer bindings of this snapshot: its own peers, then those bound
    /// below it.
    fn bindings(&self) -> impl Iterator<Item = (&str, &DepRef)> {
        self.entry
            .children()
            .filter(|(alias, _)| self.own_peers.contains(alias.as_str()))
            .map(|(alias, reference)| (alias.as_str(), reference))
            .chain(
                self.transitive
                    .iter()
                    .map(|(name, reference)| (name.as_str(), reference)),
            )
    }

    /// `reference` as v13 writes it: its version, naming the snapshot by
    /// its bindings when the reference is marked explicit.
    fn written(
        &self,
        name: &str,
        reference: &DepRef,
        explicit: &Explicit,
        snapshots: &Snapshots<'_>,
    ) -> NextRef {
        let named = explicit
            .has_snapshot_reference(self.key, name)
            .then(|| reference.resolve(name))
            .flatten()
            .and_then(|target| snapshots.naming_bindings(&target));
        NextRef::of(reference.without_groups(), named)
    }

    fn next_entry(&self, explicit: &Explicit, snapshots: &Snapshots<'_>) -> Entry {
        let written =
            |name: &str, reference: &DepRef| self.written(name, reference, explicit, snapshots);
        let mut entry = Entry {
            optional: self.entry.optional,
            ..Entry::default()
        };
        for (map, target) in [
            (&self.entry.dependencies, &mut entry.dependencies),
            (
                &self.entry.optional_dependencies,
                &mut entry.optional_dependencies,
            ),
        ] {
            for (alias, reference) in map {
                let target = if self.own_peers.contains(alias.as_str()) {
                    &mut entry.own
                } else {
                    &mut *target
                };
                target.insert(alias.clone(), written(alias, reference));
            }
        }
        entry.below = self
            .transitive
            .iter()
            .map(|(name, reference)| (name.clone(), written(name, reference)))
            .collect();
        entry.unbound = explicit
            .unbound_peer_names
            .contains(self.key)
            .then(|| self.unbound_peer_names());
        entry
    }

    /// The transitive peers no provider below binds.
    fn unbound_peer_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entry
            .transitive
            .iter()
            .flatten()
            .filter(|name| !self.transitive.iter().any(|(bound, _)| bound == *name))
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// The references of this snapshot that read back to another snapshot:
    /// all its bindings when its own key read back wrong, and every child
    /// that read back to another key.
    fn mismatches(&self, restored: &Restored, package: &Key, index: usize) -> Vec<String> {
        let wrong_key = restored.snapshot_key(package, index) != Some(self.key);
        let bindings = self.binding_names().filter(|_| wrong_key);
        let changed = self.entry.children().filter_map(|(alias, reference)| {
            let expected = reference.resolve(alias)?;
            (restored.child_key(package, index, alias) != Some(&expected)).then_some(alias.as_str())
        });
        bindings.chain(changed).map(str::to_string).collect()
    }
}

/// A snapshot's transitive peers: those its key names, and those left for
/// [`ProviderFinder`].
struct Transitive<'a> {
    named: Vec<(String, DepRef)>,
    unnamed: Vec<&'a str>,
}

impl<'a> Transitive<'a> {
    fn of(key: &Key, entry: &'a SnapshotIn, own_peers: &BTreeSet<&str>) -> Self {
        let (named, mut unnamed) = match entry.transitive {
            Some(_) => transitive_named_in_key(key, entry),
            None => (Vec::new(), Vec::new()),
        };
        if !unnamed.is_empty() && all_groups_named(key, entry, own_peers, named.len()) {
            // The key names every provider, so the rest have none.
            unnamed.clear();
        }
        Transitive { named, unnamed }
    }

    /// Finds the providers of the unnamed peers below `key`. A peer whose
    /// provider the key does not name is missing as far as the key goes.
    fn find_unnamed(&mut self, finder: &mut ProviderFinder<'a, '_>, key: &'a Key) {
        if self.unnamed.is_empty() {
            return;
        }
        let segments = provider_segments(&key.suffix.peer);
        for name in std::mem::take(&mut self.unnamed) {
            let constraint = Constraint::new(name, segments.as_deref());
            if let Some(reference) = finder.provider_below(key, name, &constraint) {
                self.named.push((name.to_string(), reference.clone()));
            }
        }
    }
}

/// The peers the snapshots of one package bind differently or not all
/// bind, and the versions bound with several peer contexts.
#[derive(Default)]
struct Variation {
    names: BTreeSet<String>,
    nested: BTreeSet<(String, DepRef)>,
}

/// Bounds the `with` of one reference.
struct Budget {
    visiting: BTreeSet<Key>,
    remaining: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            visiting: BTreeSet::new(),
            remaining: MAX_BINDINGS,
        }
    }
}

impl Budget {
    fn enter(&mut self, target: &Key) -> Result<(), String> {
        if self.visiting.len() >= MAX_NESTED_REFERENCES {
            return Err(format!(
                "peer bindings exceed {MAX_NESTED_REFERENCES} nested references"
            ));
        }
        self.visiting.insert(target.clone());
        Ok(())
    }

    fn take_binding(&mut self) -> Result<(), String> {
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            format!("a peer reference expands to more than {MAX_BINDINGS} bindings")
        })?;
        Ok(())
    }
}

/// The v13 projects and package snapshots of one round.
pub(super) struct Sections {
    pub importers: BTreeMap<String, ImporterOut>,
    /// Each package's snapshots, in the order the writer lists them.
    pub snapshots: Vec<(Key, Vec<Entry>)>,
}

pub(super) struct ImporterOut {
    pub groups: [Option<BTreeMap<String, DepOut>>; 3],
    pub skipped: Option<Value>,
}

impl ImporterOut {
    pub fn dependencies(&self) -> impl Iterator<Item = (&String, &DepOut)> {
        self.groups.iter().flatten().flatten()
    }
}

/// An importer dependency with its version's groups dropped.
pub(super) struct DepOut {
    pub specifier: String,
    pub version: ImporterVersion,
    pub peers: Option<NamedBindings>,
}

pub(super) struct Snapshots<'a> {
    by_package: Vec<(Key, Vec<Snapshot<'a>>)>,
    /// Where each snapshot key is in `by_package`.
    index: HashMap<&'a Key, (usize, usize)>,
    varying: Vec<Variation>,
    named: HashMap<&'a Key, Option<NamedBindings>>,
    components: HashMap<&'a Key, usize>,
}

impl<'a> Snapshots<'a> {
    pub(super) fn of(lockfile: &'a Lockfile<'_>) -> Result<Self, String> {
        let snapshots = &lockfile.snapshots;
        let own_peers: BTreeMap<&'a Key, BTreeSet<&'a str>> = snapshots
            .iter()
            .map(|(key, entry)| {
                let metadata = lockfile.packages.get(&key.without_peer()).copied();
                (key, own_peer_names(key, entry, metadata))
            })
            .collect();
        let mut finder = ProviderFinder::new(snapshots, &own_peers);
        let mut grouped: BTreeMap<Key, Vec<Snapshot<'a>>> = BTreeMap::new();
        for (key, entry) in snapshots {
            let own = own_peers.get(key).cloned().unwrap_or_default();
            let mut transitive = Transitive::of(key, entry, &own);
            transitive.find_unnamed(&mut finder, key);
            grouped
                .entry(key.without_peer())
                .or_default()
                .push(Snapshot {
                    key,
                    entry,
                    own_peers: own,
                    transitive: transitive.named,
                });
        }
        Snapshots::from_groups(grouped)
    }

    fn from_groups(grouped: BTreeMap<Key, Vec<Snapshot<'a>>>) -> Result<Self, String> {
        let mut by_package: Vec<(Key, Vec<Snapshot<'a>>)> = grouped.into_iter().collect();
        for (_, snapshots) in &mut by_package {
            if snapshots.len() > 1 {
                snapshots.sort_by_cached_key(|snapshot| snapshot.key.to_string());
            }
        }
        let index = by_package
            .iter()
            .enumerate()
            .flat_map(|(package, (_, snapshots))| {
                snapshots
                    .iter()
                    .enumerate()
                    .map(move |(index, snapshot)| (snapshot.key, (package, index)))
            })
            .collect();
        let varying = by_package
            .iter()
            .map(|(_, snapshots)| varying_bindings(snapshots))
            .collect();
        let components = binding_components(&by_package);
        let mut snapshots = Snapshots {
            by_package,
            index,
            varying,
            named: HashMap::new(),
            components,
        };
        let named = snapshots
            .index
            .keys()
            .map(|key| {
                Ok((
                    *key,
                    snapshots.naming_bindings_in(key, &mut Budget::default())?,
                ))
            })
            .collect::<Result<_, String>>()?;
        snapshots.named = named;
        Ok(snapshots)
    }

    /// The bindings that name the snapshot `target` among its package's:
    /// the peers the package's snapshots bind differently, as `target`
    /// binds them. `None` when nothing tells the snapshot apart.
    fn naming_bindings(&self, target: &Key) -> Option<NamedBindings> {
        self.named.get(target).cloned().flatten()
    }

    fn naming_bindings_in(
        &self,
        target: &Key,
        budget: &mut Budget,
    ) -> Result<Option<NamedBindings>, String> {
        let Some(&(package, index)) = self.index.get(target) else {
            return Ok(None);
        };
        let varying = &self.varying[package].names;
        if varying.is_empty() || budget.visiting.contains(target) {
            return Ok(None);
        }
        budget.enter(target)?;
        let snapshot = &self.by_package[package].1[index];
        let mut peers = NamedBindings::new();
        for (name, reference) in snapshot.bindings() {
            if !varying.contains(name) {
                continue;
            }
            budget.take_binding()?;
            let nested = match reference.resolve(name) {
                Some(next) if self.binding_needs_nested(target, name, reference) => {
                    self.naming_bindings_in(&next, budget)?
                }
                _ => None,
            };
            peers.insert(
                name.to_string(),
                NextRef::of(reference.without_groups(), nested),
            );
        }
        budget.visiting.remove(target);
        Ok(Some(peers))
    }

    fn binding_needs_nested(&self, source: &Key, name: &str, reference: &DepRef) -> bool {
        let Some(target) = reference.resolve(name) else {
            return false;
        };
        // Cycle edges have one version-only description, independent of
        // the entry point.
        if self.components.get(source) == self.components.get(&target) {
            return false;
        }
        let Some(&(package, _)) = self.index.get(source) else {
            return false;
        };
        self.varying[package]
            .nested
            .contains(&(name.to_string(), reference.without_groups()))
    }

    /// Nested discriminators must also be written on the bindings they
    /// identify.
    pub(super) fn mark_nested_bindings(&self, explicit: &mut Explicit) {
        for snapshot in self.by_package.iter().flat_map(|(_, snapshots)| snapshots) {
            let Some(bindings) = self.naming_bindings(snapshot.key) else {
                continue;
            };
            let names: Vec<&str> = snapshot
                .binding_names()
                .filter(|name| {
                    bindings
                        .get(*name)
                        .is_some_and(|reference| reference.peers().is_some())
                })
                .collect();
            for name in names {
                explicit.mark_snapshot_reference(snapshot.key, name);
            }
        }
    }

    pub(super) fn sections(&self, lockfile: &Lockfile<'_>, explicit: &Explicit) -> Sections {
        let snapshots = self
            .by_package
            .iter()
            .map(|(package, snapshots)| {
                let entries = snapshots
                    .iter()
                    .map(|snapshot| snapshot.next_entry(explicit, self))
                    .collect();
                (package.clone(), entries)
            })
            .collect();
        let importers = lockfile
            .importers
            .iter()
            .map(|(id, importer)| (id.clone(), self.next_importer(id, importer, explicit)))
            .collect();
        Sections {
            importers,
            snapshots,
        }
    }

    /// The importer with its versions' groups dropped. A reference the
    /// importer's scope does not select names its snapshot by its bindings.
    fn next_importer(&self, id: &str, importer: &ImporterIn, explicit: &Explicit) -> ImporterOut {
        let convert = |deps: &BTreeMap<String, DepIn>| {
            deps.iter()
                .map(|(alias, dep)| {
                    let peers = explicit
                        .has_importer_reference(id, alias)
                        .then(|| dep.version.resolved_key(alias))
                        .flatten()
                        .and_then(|target| self.naming_bindings(&target));
                    let out = DepOut {
                        specifier: dep.specifier.clone(),
                        version: importer_version_without_groups(&dep.version),
                        peers,
                    };
                    (alias.clone(), out)
                })
                .collect()
        };
        let [dependencies, dev, optional] = &importer.groups;
        ImporterOut {
            groups: [
                dependencies.as_ref().map(convert),
                dev.as_ref().map(convert),
                optional.as_ref().map(convert),
            ],
            skipped: importer.skipped.clone(),
        }
    }

    /// Marks every reference that read back to another snapshot. Returns
    /// whether it marked one that was not marked before.
    pub(super) fn mark_mismatches(
        &self,
        lockfile: &Lockfile<'_>,
        restored: &Restored,
        explicit: &mut Explicit,
    ) -> bool {
        let mut marked = false;
        for (package, snapshots) in &self.by_package {
            for (index, snapshot) in snapshots.iter().enumerate() {
                for name in snapshot.mismatches(restored, package, index) {
                    marked |= explicit.mark_snapshot_reference(snapshot.key, &name);
                }
            }
        }
        for (id, importer) in &lockfile.importers {
            for (alias, dep) in importer.dependencies() {
                let Some(expected) = dep.version.resolved_key(alias) else {
                    continue;
                };
                if restored.importer_key(id, alias) != Some(&expected) {
                    marked |= explicit.mark_importer_reference(id, alias);
                }
            }
        }
        marked
    }
}

/// The peers that `snapshots`, of one package, bind differently or not
/// all bind.
fn varying_bindings(snapshots: &[Snapshot<'_>]) -> Variation {
    let mut values: BTreeMap<&str, (BTreeSet<&DepRef>, usize)> = BTreeMap::new();
    for snapshot in snapshots {
        for (name, reference) in snapshot.bindings() {
            let (bound, count) = values.entry(name).or_default();
            bound.insert(reference);
            *count += 1;
        }
    }
    let mut variation = Variation::default();
    for (name, (bound, count)) in values {
        if bound.len() > 1 || count < snapshots.len() {
            variation.names.insert(name.to_string());
        }
        let mut seen = BTreeSet::new();
        for reference in bound {
            let version = reference.without_groups();
            if !seen.insert(version.clone()) {
                variation.nested.insert((name.to_string(), version));
            }
        }
    }
    variation
}

/// The strongly connected component of each snapshot over its bindings.
fn binding_components<'a>(groups: &[(Key, Vec<Snapshot<'a>>)]) -> HashMap<&'a Key, usize> {
    let snapshots: Vec<&Snapshot<'a>> =
        groups.iter().flat_map(|(_, snapshots)| snapshots).collect();
    let indices: HashMap<&Key, usize> = snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| (snapshot.key, index))
        .collect();
    let found = tarjan::components(
        snapshots.len(),
        |index| {
            snapshots[index]
                .bindings()
                .filter_map(|(name, reference)| {
                    reference
                        .resolve(name)
                        .and_then(|key| indices.get(&key).copied())
                })
                .collect()
        },
        |_| true,
    );
    found
        .component_of
        .into_iter()
        .enumerate()
        .filter_map(|(index, component)| Some((snapshots[index].key, component?)))
        .collect()
}

/// An importer version without its peer groups.
fn importer_version_without_groups(version: &ImporterVersion) -> ImporterVersion {
    match version {
        ImporterVersion::Regular(suffix) => ImporterVersion::Regular(suffix.without_peer()),
        ImporterVersion::Alias(key) => ImporterVersion::Alias(key.without_peer()),
        ImporterVersion::File(payload) => ImporterVersion::File(
            VerPeer::parse(&format!("file:{payload}"))
                .ok()
                .and_then(|suffix| {
                    suffix
                        .without_peer()
                        .to_string()
                        .strip_prefix("file:")
                        .map(str::to_string)
                })
                .unwrap_or_else(|| payload.clone()),
        ),
        ImporterVersion::Link(path) => ImporterVersion::Link(path.clone()),
    }
}
