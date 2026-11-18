//! The v13 document of one round, as a JSON value in pnpm's field order.
//! Ported from pnpm's `next_format/{document,settings,package,importer}.rs`
//! and the `package/{hoist,peers,dependencies}.rs` writers.

use super::super::refs::{ImporterVersion, Key, NamedBindings, NextRef, version_separator_index};
use super::super::{Entry, Implied, catalog_implied_version, claim, one_to_one, read_catalogs};
use super::Lockfile;
use super::snapshots::{DepOut, ImporterOut, Sections};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The root fields of the v9 document that v13 moves into `settings`, in
/// the order pnpm writes them there.
const MOVED_TO_SETTINGS: [&str; 6] = [
    "catalogs",
    "overrides",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "ignoredOptionalDependencies",
    "patchedDependencies",
];

/// The root fields of the v9 document that v13 rebuilds or drops.
const REBUILT: [&str; 6] = [
    "lockfileVersion",
    "settings",
    "importers",
    "packages",
    "snapshots",
    "time",
];

/// The v13 document of `lockfile`, with `sections` for its projects and
/// packages.
pub(super) fn render(
    lockfile: &Lockfile<'_>,
    sections: &Sections,
    overrides_order: &[String],
) -> Value {
    let root = lockfile.root;
    let mut document = Map::new();
    document.insert("lockfileVersion".into(), json!("13.0"));
    let settings = settings(root, overrides_order);
    if !settings.is_empty() {
        document.insert("settings".into(), Value::Object(settings));
    }
    let projects = projects(lockfile, sections);
    if !projects.is_empty() {
        document.insert("projects".into(), Value::Object(projects));
    }
    let packages: Map<String, Value> = sections
        .snapshots
        .iter()
        .map(|(package, entries)| {
            let metadata = lockfile.packages.get(package).copied();
            (
                package.to_string(),
                package_entry(package, metadata, entries.clone()),
            )
        })
        .collect();
    if !packages.is_empty() {
        document.insert("packages".into(), Value::Object(packages));
    }
    if let Some(time) = root.get("time") {
        document.insert("time".into(), time.clone());
    }
    for (field, value) in root {
        if !REBUILT.contains(&field.as_str()) && !MOVED_TO_SETTINGS.contains(&field.as_str()) {
            document.insert(field.clone(), value.clone());
        }
    }
    Value::Object(document)
}

/// The v9 `settings`, with the root fields v13 keeps beside them.
fn settings(root: &Map<String, Value>, overrides_order: &[String]) -> Map<String, Value> {
    let mut settings = root
        .get("settings")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for field in MOVED_TO_SETTINGS {
        let Some(value) = root.get(field) else {
            continue;
        };
        let value = match field {
            "catalogs" => collapsed_catalogs(value),
            "overrides" => ordered_overrides(value, overrides_order),
            _ => value.clone(),
        };
        settings.insert(field.into(), value);
    }
    settings
}

/// The catalogs, with an entry whose version follows from its specifier
/// written as the specifier alone.
fn collapsed_catalogs(value: &Value) -> Value {
    let Some(catalogs) = value.as_object() else {
        return value.clone();
    };
    let catalogs = catalogs
        .iter()
        .map(|(catalog, entries)| {
            let Some(entries) = entries.as_object() else {
                return (catalog.clone(), entries.clone());
            };
            let entries = entries
                .iter()
                .map(|(alias, entry)| {
                    let specifier = entry.get("specifier").and_then(Value::as_str);
                    let version = entry.get("version").and_then(Value::as_str);
                    let collapsed = match (specifier, version) {
                        (Some(specifier), Some(version))
                            if catalog_implied_version(specifier) == Some(version) =>
                        {
                            json!(specifier)
                        }
                        _ => entry.clone(),
                    };
                    (alias.clone(), collapsed)
                })
                .collect();
            (catalog.clone(), Value::Object(entries))
        })
        .collect();
    Value::Object(catalogs)
}

/// The overrides in the order of the lockfile being replaced, then the
/// rest in the order the graph has them.
fn ordered_overrides(value: &Value, order: &[String]) -> Value {
    let Some(overrides) = value.as_object() else {
        return value.clone();
    };
    let mut ordered = Map::new();
    for key in order {
        if let Some(value) = overrides.get(key) {
            ordered.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in overrides {
        if !ordered.contains_key(key) {
            ordered.insert(key.clone(), value.clone());
        }
    }
    Value::Object(ordered)
}

fn projects(lockfile: &Lockfile<'_>, sections: &Sections) -> Map<String, Value> {
    let catalogs = read_catalogs(lockfile.root.get("catalogs")).unwrap_or_default();
    let linked = linked_importers(&sections.importers);
    let implied = Implied::new(
        &catalogs,
        sections.snapshots.iter().map(|(key, _)| key),
        linked.clone(),
    );
    let names: BTreeMap<&str, &str> = linked
        .iter()
        .map(|(name, id)| (id.as_str(), name.as_str()))
        .collect();
    sections
        .importers
        .iter()
        .map(|(id, importer)| {
            let mut out = Map::new();
            if let Some(name) = names.get(id.as_str()) {
                out.insert("name".into(), json!(name));
            }
            for (group, deps) in super::GROUPS.iter().zip(&importer.groups) {
                let Some(deps) = deps else { continue };
                let deps = deps
                    .iter()
                    .map(|(alias, dep)| (alias.clone(), importer_dep(&implied, id, alias, dep)))
                    .collect();
                out.insert((*group).into(), Value::Object(deps));
            }
            if let Some(skipped) = &importer.skipped {
                out.insert("skippedOptionalDependencies".into(), skipped.clone());
            }
            (id.clone(), Value::Object(out))
        })
        .collect()
}

/// An importer dependency: its specifier alone when the version follows
/// from it, else with its version, and with the bindings that name its
/// snapshot when the importer's scope does not select it.
fn importer_dep(implied: &Implied<'_>, id: &str, alias: &str, dep: &DepOut) -> Value {
    let version = dep.version.to_string();
    let collapses = implied
        .version(id, alias, &dep.specifier)
        .is_some_and(|implied| *implied == version);
    if collapses && dep.peers.is_none() {
        return json!(dep.specifier);
    }
    let mut out = Map::new();
    out.insert("specifier".into(), json!(dep.specifier));
    if !collapses {
        out.insert("version".into(), json!(version));
    }
    if let Some(peers) = &dep.peers {
        out.insert("with".into(), bindings_value(peers));
    }
    Value::Object(out)
}

/// The projects `workspace:` dependencies link to, by the name the
/// dependency gives them. A link to a path that is not an importer is
/// left out.
fn linked_importers(importers: &BTreeMap<String, ImporterOut>) -> BTreeMap<String, String> {
    let mut linked = BTreeMap::new();
    for (id, importer) in importers {
        for (alias, dep) in importer.dependencies() {
            let Some(target) = dep.specifier.strip_prefix("workspace:") else {
                continue;
            };
            let ImporterVersion::Link(link) = &dep.version else {
                continue;
            };
            let Some(target_id) = link_target(id, link) else {
                continue;
            };
            if !importers.contains_key(&target_id) {
                continue;
            }
            let name = version_separator_index(target).map_or(alias.as_str(), |at| &target[..at]);
            claim(&mut linked, name, &target_id);
        }
    }
    one_to_one(linked)
}

/// The importer id the `link:` path `link` of the importer `id` points at.
fn link_target(id: &str, link: &str) -> Option<String> {
    let mut segments: Vec<&str> = id
        .split('/')
        .filter(|segment| !matches!(*segment, "" | "."))
        .collect();
    for segment in link.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    Some(if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join("/")
    })
}

fn ref_value(reference: &NextRef) -> Value {
    match reference {
        NextRef::Plain(version) => json!(version.to_string()),
        NextRef::Named { version, with } => {
            json!({ "version": version.to_string(), "with": bindings_value(with) })
        }
    }
}

fn bindings_value(bindings: &NamedBindings) -> Value {
    Value::Object(
        bindings
            .iter()
            .map(|(name, reference)| (name.clone(), ref_value(reference)))
            .collect(),
    )
}

/// A `packages` entry: the package's metadata, what its snapshots share,
/// and what each adds when it has several.
fn package_entry(key: &Key, metadata: Option<&Map<String, Value>>, entries: Vec<Entry>) -> Value {
    let mut out = Map::new();
    let resolution = metadata.and_then(|metadata| metadata.get("resolution"));
    let (resolution, integrity) = match resolution {
        Some(resolution) => split_resolution(key, resolution),
        None => (None, None),
    };
    if let Some(resolution) = resolution {
        out.insert("resolution".into(), resolution);
    }
    if let Some(integrity) = integrity {
        out.insert("integrity".into(), integrity);
    }
    for field in [
        "version",
        "engines",
        "cpu",
        "os",
        "libc",
        "hasBin",
        "prepare",
        "bundledDependencies",
    ] {
        if let Some(value) = metadata.and_then(|metadata| metadata.get(field)) {
            out.insert(field.into(), value.clone());
        }
    }
    let (shared, snapshots) = hoist(entries);
    if let Some(peers) = merged_peers(metadata, &shared) {
        out.insert("peerDependencies".into(), Value::Object(peers));
    }
    let directory = resolution_type(metadata) == Some("directory");
    if let Some(deps) = package_deps(&shared.dependencies, directory, directory) {
        out.insert("dependencies".into(), deps);
    }
    if let Some(deps) = package_deps(&shared.optional_dependencies, directory, false) {
        out.insert("optionalDependencies".into(), deps);
    }
    if shared.optional {
        out.insert("optional".into(), Value::Bool(true));
    }
    if let Some(unbound) = &shared.unbound {
        out.insert("unboundPeerDependencies".into(), json!(unbound));
    }
    if let Some(snapshots) = snapshots {
        let snapshots = snapshots.iter().map(snapshot_item).collect();
        out.insert("snapshots".into(), Value::Array(snapshots));
    }
    Value::Object(out)
}

fn resolution_type(metadata: Option<&Map<String, Value>>) -> Option<&str> {
    metadata?.get("resolution")?.get("type")?.as_str()
}

/// The resolution as the entry writes it, with the integrity apart: none
/// for a registry package, whose integrity is enough, or for the directory
/// of a `name@file:<path>` key.
fn split_resolution(key: &Key, resolution: &Value) -> (Option<Value>, Option<Value>) {
    let Some(map) = resolution.as_object() else {
        return (Some(resolution.clone()), None);
    };
    let integrity = map.get("integrity");
    if let Some(integrity) = integrity
        && map.len() == 1
    {
        return (None, Some(integrity.clone()));
    }
    match map.get("type").and_then(Value::as_str) {
        Some("directory") => {
            let directory = map.get("directory").and_then(Value::as_str);
            if directory.is_some() && directory == key.suffix.version.strip_prefix("file:") {
                return (None, None);
            }
        }
        None if map.contains_key("tarball") => {
            if let Some(integrity) = integrity {
                let mut rest = map.clone();
                rest.remove("integrity");
                return (Some(Value::Object(rest)), Some(integrity.clone()));
            }
        }
        _ => {}
    }
    (Some(resolution.clone()), None)
}

/// What every one of `entries` shares, and what each adds when there are
/// several.
fn hoist(mut entries: Vec<Entry>) -> (Entry, Option<Vec<Entry>>) {
    if entries.len() <= 1 {
        return (entries.pop().unwrap_or_default(), None);
    }
    let (first, rest) = entries.split_at(1);
    let first = &first[0];
    let common = |map: fn(&Entry) -> &BTreeMap<String, NextRef>| -> BTreeMap<String, NextRef> {
        map(first)
            .iter()
            .filter(|(name, reference)| {
                rest.iter()
                    .all(|other| map(other).get(*name) == Some(*reference))
            })
            .map(|(name, reference)| (name.clone(), reference.clone()))
            .collect()
    };
    let shared = Entry {
        dependencies: common(|entry| &entry.dependencies),
        optional_dependencies: common(|entry| &entry.optional_dependencies),
        own: common(|entry| &entry.own),
        below: common(|entry| &entry.below),
        unbound: first
            .unbound
            .as_ref()
            .filter(|unbound| {
                rest.iter()
                    .all(|other| other.unbound.as_ref() == Some(*unbound))
            })
            .cloned(),
        optional: first.optional && rest.iter().all(|other| other.optional),
    };
    for entry in &mut entries {
        for (map, shared_map) in [
            (&mut entry.dependencies, &shared.dependencies),
            (
                &mut entry.optional_dependencies,
                &shared.optional_dependencies,
            ),
            (&mut entry.own, &shared.own),
            (&mut entry.below, &shared.below),
        ] {
            map.retain(|name, _| !shared_map.contains_key(name));
        }
        if shared.unbound.is_some() {
            entry.unbound = None;
        }
        if shared.optional {
            entry.optional = false;
        }
    }
    (shared, Some(entries))
}

/// The declared peers, their meta and the bindings every snapshot shares,
/// as the one map the entry writes.
fn merged_peers(
    metadata: Option<&Map<String, Value>>,
    shared: &Entry,
) -> Option<Map<String, Value>> {
    let ranges = metadata
        .and_then(|metadata| metadata.get("peerDependencies"))
        .and_then(Value::as_object);
    let meta = metadata
        .and_then(|metadata| metadata.get("peerDependenciesMeta"))
        .and_then(Value::as_object);
    let mut entries: BTreeMap<String, PeerOut> = ranges
        .into_iter()
        .flatten()
        .map(|(name, range)| (name.clone(), PeerOut::Specifier(range.clone())))
        .collect();
    for (name, details) in meta.into_iter().flatten() {
        let optional = details.get("optional").cloned();
        let specifier = ranges.and_then(|ranges| ranges.get(name)).cloned();
        entries.insert(
            name.clone(),
            PeerOut::Detailed(PeerDetails {
                specifier,
                optional,
                ..PeerDetails::default()
            }),
        );
    }
    for (bindings, below) in [(&shared.own, false), (&shared.below, true)] {
        for (name, binding) in bindings {
            let entry = entries
                .entry(name.clone())
                .or_insert_with(|| PeerOut::Detailed(PeerDetails::default()));
            if let PeerOut::Specifier(range) = entry {
                *entry = PeerOut::Detailed(PeerDetails {
                    specifier: Some(std::mem::take(range)),
                    ..PeerDetails::default()
                });
            }
            if let PeerOut::Detailed(details) = entry {
                details.binding = Some(binding.clone());
                details.below = below;
            }
        }
    }
    (!entries.is_empty()).then(|| {
        entries
            .into_iter()
            .map(|(name, entry)| (name, entry.into_value()))
            .collect()
    })
}

enum PeerOut {
    Specifier(Value),
    Detailed(PeerDetails),
}

#[derive(Default)]
struct PeerDetails {
    specifier: Option<Value>,
    binding: Option<NextRef>,
    optional: Option<Value>,
    below: bool,
}

impl PeerOut {
    fn into_value(self) -> Value {
        let details = match self {
            PeerOut::Specifier(range) => return range,
            PeerOut::Detailed(details) => details,
        };
        let mut out = Map::new();
        if let Some(specifier) = details.specifier {
            out.insert("specifier".into(), specifier);
        }
        if let Some(binding) = &details.binding {
            binding_fields(&mut out, binding);
        }
        if let Some(optional) = details.optional {
            out.insert("optional".into(), optional);
        }
        if details.below {
            out.insert("below".into(), Value::Bool(true));
        }
        Value::Object(out)
    }
}

/// `version` and `with` of a binding.
fn binding_fields(out: &mut Map<String, Value>, binding: &NextRef) {
    out.insert("version".into(), json!(binding.version().to_string()));
    if let Some(with) = binding.peers() {
        out.insert("with".into(), bindings_value(with));
    }
}

/// A `dependencies` or `optionalDependencies` map of a package entry. A
/// directory package writes each reference as a map, so a reader does not
/// take it for a declaration, and its `dependencies` even when empty, to
/// say it records none.
fn package_deps(
    references: &BTreeMap<String, NextRef>,
    directory: bool,
    always: bool,
) -> Option<Value> {
    if references.is_empty() && !always {
        return None;
    }
    let deps = references
        .iter()
        .map(|(alias, reference)| {
            let value = match reference {
                NextRef::Plain(version) if directory => json!({ "version": version.to_string() }),
                reference => ref_value(reference),
            };
            (alias.clone(), value)
        })
        .collect();
    Some(Value::Object(deps))
}

/// One item of a package's `snapshots` list.
fn snapshot_item(entry: &Entry) -> Value {
    let mut out = Map::new();
    for (field, map) in [
        ("dependencies", &entry.dependencies),
        ("optionalDependencies", &entry.optional_dependencies),
    ] {
        if !map.is_empty() {
            let refs = map
                .iter()
                .map(|(alias, reference)| (alias.clone(), ref_value(reference)))
                .collect();
            out.insert(field.into(), Value::Object(refs));
        }
    }
    let peers: BTreeMap<&String, Value> = [(&entry.own, false), (&entry.below, true)]
        .into_iter()
        .flat_map(|(bindings, below)| {
            bindings.iter().map(move |(name, binding)| {
                let mut details = Map::new();
                binding_fields(&mut details, binding);
                if below {
                    details.insert("below".into(), Value::Bool(true));
                }
                (name, Value::Object(details))
            })
        })
        .collect();
    if !peers.is_empty() {
        let peers = peers
            .into_iter()
            .map(|(name, details)| (name.clone(), details))
            .collect();
        out.insert("peerDependencies".into(), Value::Object(peers));
    }
    if let Some(unbound) = &entry.unbound {
        out.insert("unboundPeerDependencies".into(), json!(unbound));
    }
    if entry.optional {
        out.insert("optional".into(), Value::Bool(true));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::link_target;

    #[test]
    fn link_target_walks_from_the_importer() {
        assert_eq!(
            link_target(".", "packages/a").as_deref(),
            Some("packages/a")
        );
        assert_eq!(
            link_target("packages/b", "../a").as_deref(),
            Some("packages/a")
        );
        assert_eq!(link_target("packages/b", "../..").as_deref(), Some("."));
        assert_eq!(link_target(".", ".."), None);
    }
}
