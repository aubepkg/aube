//! Which children of a snapshot are its own peers, and which packages
//! provide the peers bound below it. Ported from pnpm's `write/peers.rs`
//! and `write/peers/{candidates,constraint}.rs`, with ordered collections
//! where pnpm's are hashed, so the output does not depend on hash order.

use super::super::refs::{DepRef, Key};
use super::super::tarjan;
use super::SnapshotIn;
use serde_json::{Map, Value};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// How many constraints a [`Candidates`] keeps the providers of.
const MAX_CACHED_BINDINGS: usize = 8;

/// The `(…)` groups of a peer suffix, or `None` when its parentheses do
/// not match.
fn split_peer_suffix_segments(peer: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut rest = peer;
    while !rest.is_empty() {
        let body = rest.strip_prefix('(')?;
        let close = matching_close_paren(body)?;
        segments.push(body[..close].to_string());
        rest = &body[close + 1..];
    }
    Some(segments)
}

/// Where the group that `text` is the inside of closes.
fn matching_close_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, byte) in text.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(at),
            b')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The `(patch_hash=…)` group a peer suffix opens with, or `""`.
fn patch_group(peer: &str) -> &str {
    if !peer.starts_with("(patch_hash=") {
        return "";
    }
    peer.find(')').map_or("", |close| &peer[..=close])
}

/// The provider groups of a peer suffix, or `None` when it is hashed.
pub(super) fn provider_segments(peer: &str) -> Option<Vec<String>> {
    let mut segments = split_peer_suffix_segments(peer).unwrap_or_default();
    segments.retain(|segment| !segment.starts_with("patch_hash="));
    let hashed = matches!(
        segments.as_slice(),
        [only] if only.len() == 32 && only.bytes().all(|byte| byte.is_ascii_hexdigit()),
    );
    (!hashed).then_some(segments)
}

/// Whether the peer groups `segments` name the provider `reference`,
/// listed under `alias`.
fn names_provider(segments: &[String], alias: &str, reference: &DepRef) -> bool {
    let named = |name: &str| {
        segments.iter().any(|segment| {
            segment
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('@'))
        })
    };
    if reference.as_link_target().is_some() {
        return named(alias);
    }
    let Some(provider) = reference.resolve(alias) else {
        return false;
    };
    if !named(&provider.name) {
        return false;
    }
    let full = provider.to_string();
    let pair = format!("{}@{}", provider.name, provider.suffix.without_peer());
    segments
        .iter()
        .any(|segment| *segment == full || *segment == pair)
}

/// The children of a snapshot that are its own peers: those its peer
/// groups name, or for a hashed key, those its package declares as peers.
pub(super) fn own_peer_names<'a>(
    key: &Key,
    entry: &'a SnapshotIn,
    metadata: Option<&Map<String, Value>>,
) -> BTreeSet<&'a str> {
    let peer = &key.suffix.peer;
    if peer.len() == patch_group(peer).len() {
        return BTreeSet::new();
    }
    let segments = provider_segments(peer);
    let transitive = entry.transitive_names();
    entry
        .children()
        .filter(|(alias, _)| !transitive.contains(alias.as_str()))
        .filter(|(alias, reference)| match &segments {
            Some(segments) => names_provider(segments, alias, reference),
            None => is_own_hashed_peer(entry, metadata, alias),
        })
        .map(|(alias, _)| alias.as_str())
        .collect()
}

/// Whether the package declares `alias` as a peer that this snapshot
/// binds: a required one, or an optional one it installs.
fn is_own_hashed_peer(
    entry: &SnapshotIn,
    metadata: Option<&Map<String, Value>>,
    alias: &str,
) -> bool {
    let field = |name: &str| metadata.and_then(|metadata| metadata.get(name));
    if !field("peerDependencies")
        .and_then(Value::as_object)
        .is_some_and(|declared| declared.contains_key(alias))
    {
        return false;
    }
    let optional_peer = field("peerDependenciesMeta")
        .and_then(|meta| meta.get(alias))
        .and_then(|peer| peer.get("optional"))
        == Some(&Value::Bool(true));
    !optional_peer || entry.optional_dependencies.contains_key(alias)
}

/// The transitive peers a key names the providers of, and the names left
/// for [`ProviderFinder`].
pub(super) fn transitive_named_in_key<'a>(
    key: &Key,
    entry: &'a SnapshotIn,
) -> (Vec<(String, DepRef)>, Vec<&'a str>) {
    let providers: Vec<Key> = provider_segments(&key.suffix.peer)
        .into_iter()
        .flatten()
        .filter_map(|segment| Key::parse(&segment).ok())
        .collect();
    let mut named = Vec::new();
    let mut unnamed = Vec::new();
    for name in entry.transitive.iter().flatten() {
        match providers.iter().find(|provider| provider.name == *name) {
            Some(provider) => named.push((name.clone(), DepRef::Plain(provider.suffix.clone()))),
            None => unnamed.push(name.as_str()),
        }
    }
    (named, unnamed)
}

/// Whether every peer group of the key names one of the snapshot's own
/// peers or one of its `named_transitive` peers. A hashed key names none.
pub(super) fn all_groups_named(
    key: &Key,
    entry: &SnapshotIn,
    own_peers: &BTreeSet<&str>,
    named_transitive: usize,
) -> bool {
    let Some(segments) = provider_segments(&key.suffix.peer) else {
        return false;
    };
    let own_children = entry
        .children()
        .filter(|(alias, _)| own_peers.contains(alias.as_str()))
        .count();
    segments.len() <= own_children + named_transitive
}

/// Which providers a snapshot's key allows for one of its peers. A hashed
/// key allows any.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Constraint {
    alias: String,
    segments: Option<Vec<String>>,
}

impl Constraint {
    pub(super) fn new(alias: &str, segments: Option<&[String]>) -> Self {
        Constraint {
            alias: alias.to_string(),
            segments: segments.map(<[String]>::to_vec),
        }
    }

    fn matches(&self, reference: &DepRef) -> bool {
        self.segments
            .as_ref()
            .is_none_or(|segments| names_provider(segments, &self.alias, reference))
    }
}

/// Finds which package below a snapshot provides a peer it binds from
/// outside, by following the children that need the peer from outside.
pub(super) struct ProviderFinder<'a, 'o> {
    snapshots: &'a BTreeMap<Key, SnapshotIn>,
    own_peers: &'o BTreeMap<&'a Key, BTreeSet<&'a str>>,
    /// `None` while a lookup is in progress, which ends a dependency cycle.
    found: HashMap<(&'a Key, &'a str), Option<&'a DepRef>>,
    candidates: HashMap<&'a str, Candidates<'a>>,
    needing_peer: OnceCell<BTreeMap<&'a str, BTreeSet<&'a Key>>>,
}

impl<'a, 'o> ProviderFinder<'a, 'o> {
    pub(super) fn new(
        snapshots: &'a BTreeMap<Key, SnapshotIn>,
        own_peers: &'o BTreeMap<&'a Key, BTreeSet<&'a str>>,
    ) -> Self {
        ProviderFinder {
            snapshots,
            own_peers,
            found: HashMap::new(),
            candidates: HashMap::new(),
            needing_peer: OnceCell::new(),
        }
    }

    pub(super) fn provider_below(
        &mut self,
        key: &'a Key,
        name: &'a str,
        constraint: &Constraint,
    ) -> Option<&'a DepRef> {
        if let Some(reference) = self
            .first_provider_below(key, name)
            .filter(|reference| constraint.matches(reference))
        {
            return Some(reference);
        }
        if !self.candidates.contains_key(name) {
            let candidates = Candidates::of(self, name);
            self.candidates.insert(name, candidates);
        }
        self.candidates.get_mut(name)?.matching(key, constraint)
    }

    fn first_provider_below(&mut self, key: &'a Key, name: &'a str) -> Option<&'a DepRef> {
        if let Some(found) = self.found.get(&(key, name)) {
            return *found;
        }
        self.found.insert((key, name), None);
        let found = self.search(key, name);
        self.found.insert((key, name), found);
        found
    }

    fn search(&mut self, key: &'a Key, name: &'a str) -> Option<&'a DepRef> {
        let snapshots = self.snapshots;
        let entry = snapshots.get(key)?;
        let own = self.own_peers.get(key)?;
        if let Some((_, reference)) = entry
            .children()
            .find(|(alias, _)| own.contains(alias.as_str()) && *alias == name)
        {
            return Some(reference);
        }
        self.children_needing_peer(key, name)
            .into_iter()
            .find_map(|child| self.first_provider_below(child, name))
    }

    fn children_needing_peer(&self, key: &Key, name: &str) -> Vec<&'a Key> {
        let snapshots = self.snapshots;
        let Some(entry) = snapshots.get(key) else {
            return Vec::new();
        };
        entry
            .children()
            .filter_map(|(alias, reference)| {
                let (child, _) = snapshots.get_key_value(&reference.resolve(alias)?)?;
                self.needs_from_outside(child, name).then_some(child)
            })
            .collect()
    }

    fn keys_needing_peer(&self, name: &str) -> Vec<&'a Key> {
        self.needing_peer
            .get_or_init(|| {
                let mut indexed: BTreeMap<&'a str, BTreeSet<&'a Key>> = BTreeMap::new();
                for (key, entry) in self.snapshots {
                    let own = self.own_peers.get(key).into_iter().flatten().copied();
                    for name in entry
                        .transitive
                        .iter()
                        .flatten()
                        .map(String::as_str)
                        .chain(own)
                    {
                        indexed.entry(name).or_default().insert(key);
                    }
                }
                indexed
            })
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    fn needs_from_outside(&self, key: &Key, name: &str) -> bool {
        self.snapshots
            .get(key)
            .is_some_and(|entry| entry.transitive.iter().flatten().any(|peer| peer == name))
            || self
                .own_peers
                .get(key)
                .is_some_and(|own| own.contains(name))
    }
}

/// The providers of one peer name, per strongly connected component of
/// the snapshots that need it from outside.
struct Candidates<'a> {
    component_of: HashMap<&'a Key, usize>,
    components: Vec<Component<'a>>,
    cached: VecDeque<(Constraint, Vec<Option<&'a DepRef>>)>,
}

struct Component<'a> {
    /// The own-peer references of the component's snapshots.
    references: BTreeSet<&'a DepRef>,
    /// The components below it, each listed before it.
    descendants: BTreeSet<usize>,
}

impl<'a> Candidates<'a> {
    fn of(finder: &ProviderFinder<'a, '_>, name: &str) -> Self {
        let keys = finder.keys_needing_peer(name);
        let positions: HashMap<&Key, usize> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (*key, index))
            .collect();
        let edges: Vec<Vec<usize>> = keys
            .iter()
            .map(|key| {
                finder
                    .children_needing_peer(key, name)
                    .into_iter()
                    .filter_map(|child| positions.get(child).copied())
                    .collect()
            })
            .collect();
        let found = tarjan::components(keys.len(), |index| edges[index].clone(), |_| true);
        let mut components: Vec<Component<'a>> = Vec::with_capacity(found.components.len());
        for members in &found.components {
            let references = members
                .iter()
                .flat_map(|&index| {
                    let key = keys[index];
                    let own = finder.own_peers.get(key);
                    finder
                        .snapshots
                        .get(key)
                        .into_iter()
                        .flat_map(SnapshotIn::children)
                        .filter(move |(alias, _)| {
                            *alias == name && own.is_some_and(|own| own.contains(alias.as_str()))
                        })
                        .map(|(_, reference)| reference)
                })
                .collect();
            let ready = components.len();
            let descendants = members
                .iter()
                .flat_map(|&index| &edges[index])
                .filter_map(|&child| found.component_of[child])
                .filter(|&component| component < ready)
                .collect();
            components.push(Component {
                references,
                descendants,
            });
        }
        let component_of = keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| Some((*key, found.component_of[index]?)))
            .collect();
        Candidates {
            component_of,
            components,
            cached: VecDeque::new(),
        }
    }

    fn matching(&mut self, key: &Key, constraint: &Constraint) -> Option<&'a DepRef> {
        let cached = match self
            .cached
            .iter()
            .position(|(cached, _)| cached == constraint)
            .and_then(|index| self.cached.remove(index))
        {
            Some(cached) => cached,
            None => {
                if self.cached.len() == MAX_CACHED_BINDINGS {
                    self.cached.pop_front();
                }
                (constraint.clone(), self.matching_providers(constraint))
            }
        };
        let found = self
            .component_of
            .get(key)
            .and_then(|&component| cached.1[component]);
        self.cached.push_back(cached);
        found
    }

    fn matching_providers(&self, constraint: &Constraint) -> Vec<Option<&'a DepRef>> {
        let mut providers: Vec<Option<&'a DepRef>> = Vec::with_capacity(self.components.len());
        for component in &self.components {
            let found = component
                .references
                .iter()
                .copied()
                .find(|reference| constraint.matches(reference))
                .or_else(|| {
                    component
                        .descendants
                        .iter()
                        .find_map(|&index| providers[index])
                });
            providers.push(found);
        }
        providers
    }
}
