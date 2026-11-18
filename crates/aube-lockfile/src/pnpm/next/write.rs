//! The v13 lockfile written from the v9 document of the pnpm writer.
//!
//! Ported from pnpm's `crates/lockfile/src/next_format/write`. A reference
//! is written without peer groups, unless reading the result back would
//! select another snapshot for it. The writer reads its own output back
//! with the reader of this module and writes those references with their
//! peer groups, until nothing changes. A graph whose output does not read
//! back to it is refused, so a write never loses a peer snapshot.

mod document;
mod flow;
mod providers;
mod snapshots;

use super::refs::{DepRef, ImporterVersion, Key};
use super::{GROUPS, Next, object, string};
use serde_json::{Map, Value};
use snapshots::Snapshots;
use std::collections::{BTreeMap, BTreeSet};

/// Reading back can need a reference with peer groups only where an earlier
/// round wrote another reference with them, so the rounds are few. This
/// bounds them for a graph that cannot be represented.
const MAX_ROUNDS: usize = 8;

/// The root fields of the v9 document the v13 document holds elsewhere or
/// rebuilds, so the round-trip check compares them on their own.
const SECTIONS: [&str; 4] = ["lockfileVersion", "importers", "packages", "snapshots"];

/// The v13 document for the v9 document `v9`, rendered the way pnpm
/// renders it. `overrides_order` is the order of the overrides of the
/// lockfile being replaced: pnpm keeps them in config order, which the
/// graph does not record.
pub(super) fn to_next(v9: &Value, overrides_order: &[String]) -> Result<String, String> {
    let lockfile = Lockfile::parse(v9)?;
    let snapshots = Snapshots::of(&lockfile)?;
    let mut explicit = Explicit::default();
    snapshots.mark_nested_bindings(&mut explicit);
    for _ in 0..MAX_ROUNDS {
        let sections = snapshots.sections(&lockfile, &explicit);
        let document = document::render(&lockfile, &sections, overrides_order);
        let next = Next::read(&document)?;
        let restored = next.restore();
        if snapshots.mark_mismatches(&lockfile, &restored, &mut explicit) {
            continue;
        }
        // The marks above cover which snapshot each reference selects and
        // the key each snapshot gets. The entries themselves can still read
        // back differently, so the whole document is compared once.
        let read_back = next.into_v9_with(restored)?;
        let restored = Lockfile::parse(&read_back)?;
        if explicit.mark_unbound_peer_names(&lockfile, &restored) {
            continue;
        }
        check_round_trip(&lockfile, &restored)?;
        return Ok(flow::to_string(&flow::sorted(document)));
    }
    Err("the references did not settle".to_string())
}

/// A v9 snapshot.
#[derive(Debug, PartialEq)]
struct SnapshotIn {
    dependencies: BTreeMap<String, DepRef>,
    optional_dependencies: BTreeMap<String, DepRef>,
    transitive: Option<Vec<String>>,
    optional: bool,
}

impl SnapshotIn {
    /// Every dependency, then every optional one.
    fn children(&self) -> impl Iterator<Item = (&String, &DepRef)> {
        self.dependencies.iter().chain(&self.optional_dependencies)
    }

    fn transitive_names(&self) -> BTreeSet<&str> {
        self.transitive
            .iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// Equal, comparing transitive peer names as a set.
    fn same_as(&self, other: &SnapshotIn) -> bool {
        self.dependencies == other.dependencies
            && self.optional_dependencies == other.optional_dependencies
            && self.optional == other.optional
            && self.transitive_names() == other.transitive_names()
    }
}

/// A v9 importer dependency.
#[derive(Debug, PartialEq)]
struct DepIn {
    specifier: String,
    version: ImporterVersion,
}

#[derive(Debug, PartialEq)]
struct ImporterIn {
    /// `dependencies`, `devDependencies` and `optionalDependencies`.
    groups: [Option<BTreeMap<String, DepIn>>; 3],
    /// `skippedOptionalDependencies`, carried as it is.
    skipped: Option<Value>,
}

impl ImporterIn {
    fn dependencies(&self) -> impl Iterator<Item = (&String, &DepIn)> {
        self.groups.iter().flatten().flatten()
    }
}

/// The parts of a v9 document the writer converts.
struct Lockfile<'v> {
    root: &'v Map<String, Value>,
    importers: BTreeMap<String, ImporterIn>,
    /// Each package's v9 metadata.
    packages: BTreeMap<Key, &'v Map<String, Value>>,
    snapshots: BTreeMap<Key, SnapshotIn>,
}

impl<'v> Lockfile<'v> {
    fn parse(document: &'v Value) -> Result<Self, String> {
        let root = object(document, "the lockfile")?;
        let mut importers = BTreeMap::new();
        if let Some(value) = root.get("importers") {
            for (id, importer) in object(value, "importers")? {
                importers.insert(id.clone(), parse_importer(id, importer)?);
            }
        }
        let mut packages = BTreeMap::new();
        if let Some(value) = root.get("packages") {
            for (key, metadata) in object(value, "packages")? {
                let what = format!("package {key}");
                let parsed = Key::parse(key).map_err(|err| format!("{what}: {err}"))?;
                packages.insert(parsed, object(metadata, &what)?);
            }
        }
        let mut snapshots = BTreeMap::new();
        if let Some(value) = root.get("snapshots") {
            for (key, entry) in object(value, "snapshots")? {
                let what = format!("snapshot {key}");
                let parsed = Key::parse(key).map_err(|err| format!("{what}: {err}"))?;
                snapshots.insert(parsed, parse_snapshot(entry, &what)?);
            }
        }
        Ok(Lockfile {
            root,
            importers,
            packages,
            snapshots,
        })
    }

    /// The v9 metadata of the package of `key`, without what the v13
    /// format does not record.
    fn metadata(&self, key: &Key) -> Option<Map<String, Value>> {
        let mut metadata = (*self.packages.get(&key.without_peer())?).clone();
        metadata.remove("deprecated");
        Some(metadata)
    }
}

fn parse_importer(id: &str, value: &Value) -> Result<ImporterIn, String> {
    let what = format!("importer {id}");
    let map = object(value, &what)?;
    let mut groups: [Option<BTreeMap<String, DepIn>>; 3] = Default::default();
    for (group, slot) in GROUPS.iter().zip(&mut groups) {
        let Some(deps) = map.get(*group) else {
            continue;
        };
        let mut parsed = BTreeMap::new();
        for (alias, dep) in object(deps, &what)? {
            let field = |name: &str| {
                dep.get(name)
                    .ok_or_else(|| format!("the dependency {alias} of {what} has no {name}"))
                    .and_then(|value| string(value, &what))
            };
            let version = ImporterVersion::parse(field("version")?)
                .map_err(|err| format!("the dependency {alias} of {what}: {err}"))?;
            parsed.insert(
                alias.clone(),
                DepIn {
                    specifier: field("specifier")?.to_string(),
                    version,
                },
            );
        }
        *slot = Some(parsed);
    }
    Ok(ImporterIn {
        groups,
        skipped: map.get("skippedOptionalDependencies").cloned(),
    })
}

fn parse_snapshot(value: &Value, what: &str) -> Result<SnapshotIn, String> {
    let map = object(value, what)?;
    let references = |field: &str| -> Result<BTreeMap<String, DepRef>, String> {
        let Some(value) = map.get(field) else {
            return Ok(BTreeMap::new());
        };
        object(value, what)?
            .iter()
            .map(|(alias, reference)| {
                let reference = DepRef::parse(string(reference, what)?)
                    .map_err(|err| format!("the dependency {alias} of {what}: {err}"))?;
                Ok((alias.clone(), reference))
            })
            .collect()
    };
    Ok(SnapshotIn {
        dependencies: references("dependencies")?,
        optional_dependencies: references("optionalDependencies")?,
        transitive: map
            .get("transitivePeerDependencies")
            .map(|names| super::string_list(names, what))
            .transpose()?,
        optional: map.get("optional") == Some(&Value::Bool(true)),
    })
}

/// The references written with their peer groups.
#[derive(Default)]
struct Explicit {
    snapshot_references: BTreeMap<Key, BTreeSet<String>>,
    importer_references: BTreeMap<String, BTreeSet<String>>,
    /// The snapshots written with `unboundPeerDependencies`.
    unbound_peer_names: BTreeSet<Key>,
}

impl Explicit {
    fn has_snapshot_reference(&self, key: &Key, name: &str) -> bool {
        self.snapshot_references
            .get(key)
            .is_some_and(|names| names.contains(name))
    }

    fn has_importer_reference(&self, id: &str, name: &str) -> bool {
        self.importer_references
            .get(id)
            .is_some_and(|names| names.contains(name))
    }

    /// Returns whether the reference was not marked before.
    fn mark_snapshot_reference(&mut self, key: &Key, name: &str) -> bool {
        self.snapshot_references
            .entry(key.clone())
            .or_default()
            .insert(name.to_string())
    }

    fn mark_importer_reference(&mut self, id: &str, name: &str) -> bool {
        self.importer_references
            .entry(id.to_string())
            .or_default()
            .insert(name.to_string())
    }

    /// Marks every snapshot whose transitive peer names read back
    /// differently. Returns whether it marked one that was not marked.
    fn mark_unbound_peer_names(
        &mut self,
        lockfile: &Lockfile<'_>,
        restored: &Lockfile<'_>,
    ) -> bool {
        let mut marked = false;
        for (key, entry) in &lockfile.snapshots {
            let Some(other) = restored.snapshots.get(key) else {
                continue;
            };
            if entry.transitive_names() != other.transitive_names() {
                marked |= self.unbound_peer_names.insert(key.clone());
            }
        }
        marked
    }
}

/// Fails unless `restored` is `original` as the v13 document holds it.
fn check_round_trip(original: &Lockfile<'_>, restored: &Lockfile<'_>) -> Result<(), String> {
    if let Some((id, importer)) = original
        .importers
        .iter()
        .find(|(id, importer)| restored.importers.get(*id) != Some(importer))
    {
        let Some(other) = restored.importers.get(id) else {
            return Err(format!("importer {id}: missing"));
        };
        let mut fields: Vec<&str> = GROUPS
            .iter()
            .zip(importer.groups.iter().zip(&other.groups))
            .filter(|(_, (left, right))| left != right)
            .map(|(group, _)| *group)
            .collect();
        if importer.skipped != other.skipped {
            fields.push("skippedOptionalDependencies");
        }
        return Err(format!("importer {id}: {} differ", fields.join(", ")));
    }
    if let Some(id) = restored
        .importers
        .keys()
        .find(|id| !original.importers.contains_key(*id))
    {
        return Err(format!("importer {id}: added"));
    }
    if let Some(key) = original
        .snapshots
        .iter()
        .find(|(key, entry)| {
            !restored
                .snapshots
                .get(*key)
                .is_some_and(|other| entry.same_as(other))
        })
        .map(|(key, _)| key)
        .or_else(|| {
            restored
                .snapshots
                .keys()
                .find(|key| !original.snapshots.contains_key(*key))
        })
    {
        return Err(format!("snapshot {key}"));
    }
    for key in original.snapshots.keys() {
        let package = key.without_peer();
        if original.metadata(&package) != restored.metadata(&package) {
            return Err(format!("package {package}"));
        }
    }
    let fields = |root: &Map<String, Value>| -> Map<String, Value> {
        root.iter()
            .filter(|(field, _)| !SECTIONS.contains(&field.as_str()))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect()
    };
    let (left, right) = (fields(original.root), fields(restored.root));
    if let Some(field) = left
        .keys()
        .chain(right.keys())
        .find(|field| left.get(*field) != right.get(*field))
    {
        return Err(format!("the field {field}"));
    }
    Ok(())
}
