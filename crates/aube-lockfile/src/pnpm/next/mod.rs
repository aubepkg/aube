//! pnpm's experimental v13 lockfile (`lockfileVersion: '13.0'`), read back
//! into the v9 shape the rest of the pnpm reader understands.
//!
//! pnpm 12.19 introduced the format (pnpm/pnpm PR 17886, pnpm/rfcs PR 72).
//! It drops the peer suffixes from snapshot keys and nests each package's
//! snapshots under its `packages` entry. It also leaves out whatever the
//! rest of the file implies: a registry package's resolution, an importer
//! dependency's version when its specifier implies one, and a catalog
//! entry's version. The file may be YAML or JSON with trailing commas.
//!
//! This module rebuilds the v9 sections with pnpm's own rules, ported from
//! `crates/lockfile/src/next_format` in pnpm. As a result, a v13 file reads
//! into the same graph as the v9 file pnpm writes for the same project.
//! `write.rs` writes the format back the way pnpm does.

mod refs;
mod restore;
mod tarjan;
mod write;

use super::raw::RawPnpmLockfile;
use refs::{DepRef, ImporterVersion, Key, NamedBindings, NextRef};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// Catalog → alias → (specifier, version).
type Catalogs = BTreeMap<String, BTreeMap<String, (String, String)>>;

/// Root-level blocks of the pre-v9 layout, which the v9 reader refuses.
const LEGACY_ROOT_BLOCKS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "specifiers",
];

/// The major `lockfileVersion` of the format.
const NEXT_MAJOR: u16 = 13;

/// pnpm's default `peersSuffixMaxLength`.
const DEFAULT_PEERS_SUFFIX_MAX_LENGTH: usize = 1000;

/// The importer dependency groups, in the order pnpm reads them.
const GROUPS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

/// Whether `content` is a v13 lockfile. Like pnpm, this looks only at the
/// `lockfileVersion` of the main document, without parsing the file.
pub(super) fn declares_next_format(content: &str) -> bool {
    let content = normalized(content);
    let document = main_document(&content);
    let version = if document.trim_start().starts_with('{') {
        json_lockfile_version(document)
    } else {
        document.lines().find_map(yaml_version_value)
    };
    version.and_then(comver_major) == Some(NEXT_MAJOR)
}

/// The v13 lockfile `content` as the v9 document it stands for.
pub(super) fn to_raw(content: &str) -> Result<RawPnpmLockfile, String> {
    let content = normalized(content);
    let document = parse_document(main_document(&content))?;
    let mut v9 = Next::read(&document)?.into_v9()?;
    // v13 has no root-level dependency blocks. Carry any over so the
    // reader's pre-v9 layout guard refuses them instead of the graph
    // silently coming back without those dependencies.
    if let (Some(v9), Some(root)) = (v9.as_object_mut(), document.as_object()) {
        for block in LEGACY_ROOT_BLOCKS {
            if let Some(value) = root.get(block) {
                v9.insert(block.to_string(), value.clone());
            }
        }
    }
    serde_json::from_value(v9).map_err(|err| err.to_string())
}

/// The v9 document `v9` as the text of a v13 lockfile. `overrides_order`
/// is the order of the overrides of the lockfile being replaced, which the
/// graph does not keep.
pub(super) fn to_next(v9: &Value, overrides_order: &[String]) -> Result<String, String> {
    write::to_next(v9, overrides_order)
}

/// The keys of the overrides of the lockfile `content`, in file order: the
/// root `overrides` of a v9 file, or `settings.overrides` of a v13 one.
/// Empty when the file does not parse.
pub(super) fn overrides_order(content: &str) -> Vec<String> {
    let content = normalized(content);
    let Ok(document) = parse_document(main_document(&content)) else {
        return Vec::new();
    };
    let overrides = if declares_next_format(&content) {
        document
            .get("settings")
            .and_then(|settings| settings.get("overrides"))
    } else {
        document.get("overrides")
    };
    overrides
        .and_then(Value::as_object)
        .map(|overrides| overrides.keys().cloned().collect())
        .unwrap_or_default()
}

/// `content` without a byte-order mark and with `\n` line ends.
fn normalized(content: &str) -> Cow<'_, str> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if content.contains("\r\n") {
        Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

/// The lockfile document after a leading `---` env document, if there is
/// one. Blank and comment lines may come before the first `---`, and a
/// `---` with no second document after it opens the only document.
fn main_document(content: &str) -> &str {
    let mut start = 0;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            break;
        }
        start += line.len();
    }
    let Some(rest) = content[start..].strip_prefix("---\n") else {
        return content;
    };
    match rest.find("\n---\n") {
        Some(at) => &rest[at + "\n---\n".len()..],
        None => rest,
    }
}

/// The major of a ComVer version, `major.minor`.
fn comver_major(version: &str) -> Option<u16> {
    let (major, minor) = version.split_once('.')?;
    minor.parse::<u16>().ok()?;
    major.parse().ok()
}

/// The value of a top-level `lockfileVersion:` line of a YAML document.
fn yaml_version_value(line: &str) -> Option<&str> {
    let rest = [
        "lockfileVersion:",
        "'lockfileVersion':",
        "\"lockfileVersion\":",
    ]
    .into_iter()
    .find_map(|prefix| line.strip_prefix(prefix))?;
    let end = comment_start(rest).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches(['\'', '"']))
}

/// Where a YAML comment starts: a `#` after a space or a tab.
fn comment_start(text: &str) -> Option<usize> {
    text.match_indices('#')
        .map(|(at, _)| at)
        .find(|&at| text[..at].ends_with([' ', '\t']))
}

/// The string value of the top-level `lockfileVersion` key of a JSON
/// document, found by scanning the document instead of parsing it.
fn json_lockfile_version(document: &str) -> Option<&str> {
    let mut rest = document;
    let mut depth = 0usize;
    let mut expecting_key = false;
    loop {
        let at = rest.find(['{', '}', '[', ']', ',', ':', '"'])?;
        let found = rest.as_bytes()[at];
        rest = &rest[at + 1..];
        match found {
            b'"' => {
                let (text, after) = string_end(rest)?;
                if depth == 1 && expecting_key && text == "lockfileVersion" {
                    return string_value(after);
                }
                rest = after;
                expecting_key = false;
            }
            b'{' | b'[' => {
                depth += 1;
                expecting_key = depth == 1 && found == b'{';
            }
            b'}' | b']' => depth = depth.checked_sub(1)?,
            b',' => expecting_key = depth == 1,
            _ => expecting_key = false,
        }
    }
}

/// The text of a JSON string whose opening quote is just before `text`,
/// and what follows its closing quote.
fn string_end(text: &str) -> Option<(&str, &str)> {
    let mut from = 0;
    loop {
        let at = from + text.get(from..)?.find(['"', '\\'])?;
        if text.as_bytes()[at] == b'"' {
            return Some((&text[..at], &text[at + 1..]));
        }
        from = at + 2;
    }
}

/// The string after a key's `:`.
fn string_value(after: &str) -> Option<&str> {
    let rest = after
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    string_end(rest).map(|(text, _)| text)
}

/// Blanks every comma that a closing `}` or `]` follows, outside strings,
/// so the JSON form's trailing commas parse.
fn blank_trailing_commas(bytes: &mut [u8]) {
    enum Scan {
        Code { comma: Option<usize> },
        Text,
        Escape,
    }
    let mut scan = Scan::Code { comma: None };
    for at in 0..bytes.len() {
        scan = match (scan, bytes[at]) {
            (Scan::Text, b'\\') => Scan::Escape,
            (Scan::Text, b'"') => Scan::Code { comma: None },
            (Scan::Text | Scan::Escape, _) => Scan::Text,
            (Scan::Code { .. }, b'"') => Scan::Text,
            (Scan::Code { .. }, b',') => Scan::Code { comma: Some(at) },
            (Scan::Code { comma }, b'}' | b']') => {
                if let Some(comma) = comma {
                    bytes[comma] = b' ';
                }
                Scan::Code { comma: None }
            }
            (scan @ Scan::Code { .. }, b' ' | b'\n' | b'\r' | b'\t') => scan,
            (Scan::Code { .. }, _) => Scan::Code { comma: None },
        };
    }
}

fn parse_document(document: &str) -> Result<Value, String> {
    if document.trim_start().starts_with('{') {
        let mut bytes = document.as_bytes().to_vec();
        blank_trailing_commas(&mut bytes);
        serde_json::from_slice(&bytes).map_err(|err| err.to_string())
    } else {
        yaml_serde::from_str(document).map_err(|err| err.to_string())
    }
}

/// One snapshot of a package: its references and peer bindings.
#[derive(Debug, Clone, Default)]
struct Entry {
    pub dependencies: BTreeMap<String, NextRef>,
    pub optional_dependencies: BTreeMap<String, NextRef>,
    /// The peers the package declares, bound.
    pub own: BTreeMap<String, NextRef>,
    /// The peers a dependency below declares, bound here.
    pub below: BTreeMap<String, NextRef>,
    pub unbound: Option<Vec<String>>,
    pub optional: bool,
}

impl Entry {
    pub fn bindings(&self) -> impl Iterator<Item = (&String, &NextRef)> {
        self.own.iter().chain(&self.below)
    }

    /// The reference this snapshot links as `name`, as a dependency or as
    /// a bound peer.
    pub fn reference_in_scope(&self, name: &str) -> Option<&NextRef> {
        self.dependencies
            .get(name)
            .or_else(|| self.optional_dependencies.get(name))
            .or_else(|| self.own.get(name))
            .or_else(|| self.below.get(name))
    }

    /// This snapshot of a list, with what every snapshot of the list
    /// shares added. The shared entries win.
    fn over(mut self, shared: &Entry) -> Entry {
        for (map, shared_map) in [
            (&mut self.dependencies, &shared.dependencies),
            (
                &mut self.optional_dependencies,
                &shared.optional_dependencies,
            ),
            (&mut self.own, &shared.own),
            (&mut self.below, &shared.below),
        ] {
            map.extend(shared_map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if self.unbound.is_none() {
            self.unbound.clone_from(&shared.unbound);
        }
        self.optional |= shared.optional;
        self
    }
}

/// The metadata of a package.
#[derive(Debug)]
struct Package {
    pub version: Option<String>,
    /// The ranges of the peers the package declares.
    pub peer_ranges: BTreeMap<String, String>,
    pub optional_peers: BTreeSet<String>,
    /// The v9 `packages` entry.
    metadata: Map<String, Value>,
}

/// An importer dependency, with its version.
#[derive(Debug)]
struct ResolvedDep {
    pub specifier: String,
    pub version: ImporterVersion,
    pub peers: Option<NamedBindings>,
}

#[derive(Debug, Default)]
struct Importer {
    /// `dependencies`, `devDependencies` and `optionalDependencies`.
    pub groups: [Option<BTreeMap<String, ResolvedDep>>; 3],
    /// `skippedOptionalDependencies`, carried as it is.
    pub skipped: Option<Value>,
}

impl Importer {
    /// Every dependency, in pnpm's group order.
    pub fn resolved(&self) -> impl Iterator<Item = (&String, &ResolvedDep)> {
        self.groups.iter().flatten().flatten()
    }
}

/// An importer dependency as written: its version may be left implied.
struct WrittenDep {
    specifier: String,
    version: Option<String>,
    peers: Option<NamedBindings>,
}

/// The settings the restore depends on.
struct Rules {
    pub dedupe: bool,
    pub max_length: usize,
}

/// A v13 document, split into the parts the restore reads.
struct Next {
    settings: Map<String, Value>,
    rules: Rules,
    /// Catalog → alias → (specifier, version).
    catalogs: Catalogs,
    overrides: Option<Value>,
    package_extensions_checksum: Option<Value>,
    pnpmfile_checksum: Option<Value>,
    ignored_optional_dependencies: Option<Value>,
    patched: BTreeMap<String, String>,
    importers: BTreeMap<String, Importer>,
    packages: BTreeMap<Key, Package>,
    /// `None` when the document has no `packages`.
    snapshots: Option<BTreeMap<Key, Vec<Entry>>>,
    time: Option<Value>,
}

impl Next {
    fn read(document: &Value) -> Result<Next, String> {
        let root = object(document, "the lockfile")?;
        let mut settings = match root.get("settings") {
            Some(value) => object(value, "settings")?.clone(),
            None => Map::new(),
        };
        let catalogs = settings.remove("catalogs");
        let patched = settings.remove("patchedDependencies");
        let overrides = settings.remove("overrides");
        let package_extensions_checksum = settings.remove("packageExtensionsChecksum");
        let pnpmfile_checksum = settings.remove("pnpmfileChecksum");
        let ignored_optional_dependencies = settings.remove("ignoredOptionalDependencies");
        let rules = Rules {
            dedupe: settings.get("dedupePeers") == Some(&Value::Bool(true)),
            max_length: settings
                .get("peersSuffixMaxLength")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_PEERS_SUFFIX_MAX_LENGTH, |length| {
                    usize::try_from(length).unwrap_or(usize::MAX)
                }),
        };

        let mut packages = BTreeMap::new();
        let mut snapshots = None;
        if let Some(value) = root.get("packages") {
            let listed = snapshots.insert(BTreeMap::new());
            for (key, entry) in object(value, "packages")? {
                let key = Key::parse(key).map_err(|err| format!("package {key}: {err}"))?;
                let (package, entries) = split_package(&key, entry)?;
                packages.insert(key.clone(), package);
                listed.insert(key, entries);
            }
        }

        let catalogs = read_catalogs(catalogs.as_ref())?;
        let written = match root.get("projects").or_else(|| root.get("importers")) {
            Some(value) => read_importers(value)?,
            None => BTreeMap::new(),
        };
        let mut named = BTreeMap::new();
        for (id, importer) in &written {
            if let Some(name) = &importer.name {
                claim(&mut named, name, id);
            }
        }
        let implied = Implied::new(&catalogs, packages.keys(), one_to_one(named));
        let importers = written
            .into_iter()
            .map(|(id, importer)| {
                let mut resolved = implied.resolve(&id, &importer.groups)?;
                resolved.skipped = importer.skipped;
                Ok((id, resolved))
            })
            .collect::<Result<_, String>>()?;

        Ok(Next {
            settings,
            rules,
            catalogs,
            overrides,
            package_extensions_checksum,
            pnpmfile_checksum,
            ignored_optional_dependencies,
            patched: read_patched(patched.as_ref())?,
            importers,
            packages,
            snapshots,
            time: root.get("time").cloned(),
        })
    }

    fn into_v9(self) -> Result<Value, String> {
        let restored = self.restore();
        self.into_v9_with(restored)
    }

    /// The peer-suffixed keys and references the document stands for.
    fn restore(&self) -> restore::Restored {
        restore::restore(&restore::Input {
            snapshots: self.snapshots.as_ref(),
            importers: &self.importers,
            packages: &self.packages,
            patched: &self.patched,
            rules: &self.rules,
        })
    }

    /// The v9 document, with `restored` from [`Next::restore`].
    fn into_v9_with(self, restored: restore::Restored) -> Result<Value, String> {
        let mut root = Map::new();
        root.insert("lockfileVersion".into(), json!("13.0"));
        if !self.settings.is_empty() {
            root.insert("settings".into(), Value::Object(self.settings.clone()));
        }
        if !self.catalogs.is_empty() {
            let catalogs: Map<String, Value> = self
                .catalogs
                .iter()
                .map(|(name, entries)| {
                    let entries: Map<String, Value> = entries
                        .iter()
                        .map(|(alias, (specifier, version))| {
                            (
                                alias.clone(),
                                json!({ "specifier": specifier, "version": version }),
                            )
                        })
                        .collect();
                    (name.clone(), Value::Object(entries))
                })
                .collect();
            root.insert("catalogs".into(), Value::Object(catalogs));
        }
        for (field, value) in [
            ("overrides", &self.overrides),
            (
                "packageExtensionsChecksum",
                &self.package_extensions_checksum,
            ),
            ("pnpmfileChecksum", &self.pnpmfile_checksum),
            (
                "ignoredOptionalDependencies",
                &self.ignored_optional_dependencies,
            ),
            ("time", &self.time),
        ] {
            if let Some(value) = value {
                root.insert(field.into(), value.clone());
            }
        }
        if !self.patched.is_empty() {
            root.insert("patchedDependencies".into(), json!(self.patched));
        }

        let importers: Map<String, Value> = self
            .importers
            .iter()
            .map(|(id, importer)| {
                let mut out = Map::new();
                for (group, deps) in GROUPS.iter().zip(&importer.groups) {
                    let Some(deps) = deps else { continue };
                    let deps: Map<String, Value> = deps
                        .iter()
                        .map(|(alias, dep)| {
                            let version = restored.importer_version(id, alias, &dep.version);
                            (
                                alias.clone(),
                                json!({ "specifier": dep.specifier, "version": version.to_string() }),
                            )
                        })
                        .collect();
                    out.insert((*group).into(), Value::Object(deps));
                }
                if let Some(skipped) = &importer.skipped {
                    out.insert("skippedOptionalDependencies".into(), skipped.clone());
                }
                (id.clone(), Value::Object(out))
            })
            .collect();
        root.insert("importers".into(), Value::Object(importers));

        let packages: Map<String, Value> = self
            .packages
            .iter()
            .map(|(key, package)| (key.to_string(), Value::Object(package.metadata.clone())))
            .collect();
        root.insert("packages".into(), Value::Object(packages));

        let snapshots: Map<String, Value> = restored
            .snapshots
            .into_iter()
            .map(|(key, snapshot)| (key.to_string(), snapshot.into_value()))
            .collect();
        root.insert("snapshots".into(), Value::Object(snapshots));
        Ok(Value::Object(root))
    }
}

fn object<'v>(value: &'v Value, what: &str) -> Result<&'v Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be a mapping"))
}

fn string<'v>(value: &'v Value, what: &str) -> Result<&'v str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{what} must be a string"))
}

fn boolean(value: Option<&Value>, what: &str) -> Result<Option<bool>, String> {
    value
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("{what} must be true or false"))
        })
        .transpose()
}

/// A snapshot reference: a version, or `{version, with}` naming the
/// peers of the snapshot it means.
fn next_ref(value: &Value, what: &str) -> Result<NextRef, String> {
    match value {
        Value::String(text) => DepRef::parse(text)
            .map(NextRef::Plain)
            .map_err(|err| format!("{what}: {err}")),
        Value::Object(map) => {
            let version = map
                .get("version")
                .ok_or_else(|| format!("{what} has no version"))?;
            let version =
                DepRef::parse(string(version, what)?).map_err(|err| format!("{what}: {err}"))?;
            let with = map
                .get("with")
                .ok_or_else(|| format!("{what} has no `with`"))?;
            Ok(NextRef::Named {
                version,
                with: named_bindings(with, what)?,
            })
        }
        _ => Err(format!("{what} must be a version or a mapping")),
    }
}

fn named_bindings(value: &Value, what: &str) -> Result<NamedBindings, String> {
    object(value, what)?
        .iter()
        .map(|(name, value)| Ok((name.clone(), next_ref(value, what)?)))
        .collect()
}

fn ref_map(value: Option<&Value>, what: &str) -> Result<BTreeMap<String, NextRef>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    object(value, what)?
        .iter()
        .map(|(name, value)| Ok((name.clone(), next_ref(value, what)?)))
        .collect()
}

/// A peer binding of a `peerDependencies` map: `None` for an entry that
/// binds nothing, else whether it is a `below` binding and its reference.
fn peer_binding(
    details: &Map<String, Value>,
    what: &str,
) -> Result<Option<(bool, NextRef)>, String> {
    let Some(version) = details.get("version") else {
        return Ok(None);
    };
    let version = DepRef::parse(string(version, what)?).map_err(|err| format!("{what}: {err}"))?;
    let with = details
        .get("with")
        .map(|with| named_bindings(with, what))
        .transpose()?;
    let below = boolean(details.get("below"), what)? == Some(true);
    Ok(Some((below, NextRef::of(version, with))))
}

/// A `packages` entry: the package's v9 metadata, and its snapshots with
/// what they share added.
fn split_package(key: &Key, value: &Value) -> Result<(Package, Vec<Entry>), String> {
    let what = format!("package {key}");
    let map = object(value, &what)?;
    let mut shared = Entry::default();
    let mut peer_ranges = BTreeMap::new();
    let mut optional_peers = BTreeSet::new();
    let mut peer_meta = Map::new();
    if let Some(peers) = map.get("peerDependencies") {
        for (name, entry) in object(peers, &what)? {
            let details = match entry {
                Value::String(range) => {
                    peer_ranges.insert(name.clone(), range.clone());
                    continue;
                }
                Value::Object(details) => details,
                _ => {
                    return Err(format!(
                        "the peer {name} of {key} must be a range or a mapping"
                    ));
                }
            };
            if let Some(optional) = boolean(details.get("optional"), &what)? {
                peer_meta.insert(name.clone(), json!({ "optional": optional }));
                if optional {
                    optional_peers.insert(name.clone());
                }
            }
            if let Some(specifier) = details.get("specifier") {
                peer_ranges.insert(name.clone(), string(specifier, &what)?.to_string());
            }
            if let Some((below, binding)) = peer_binding(details, &what)? {
                let bindings = if below {
                    &mut shared.below
                } else {
                    &mut shared.own
                };
                bindings.insert(name.clone(), binding);
            }
        }
    }

    let resolution = resolution(key, map)?;
    let directory = resolution.get("type") == Some(&json!("directory"));
    shared.dependencies = package_deps(key, directory, map.get("dependencies"), &what)?;
    shared.optional_dependencies =
        package_deps(key, directory, map.get("optionalDependencies"), &what)?;
    shared.unbound = map
        .get("unboundPeerDependencies")
        .map(|names| string_list(names, &what))
        .transpose()?;
    shared.optional = boolean(map.get("optional"), &what)?.unwrap_or(false);

    let mut metadata = Map::new();
    metadata.insert("resolution".into(), Value::Object(resolution));
    for field in [
        "version",
        "engines",
        "cpu",
        "os",
        "libc",
        "hasBin",
        "bundledDependencies",
    ] {
        if let Some(value) = map.get(field) {
            metadata.insert(field.into(), value.clone());
        }
    }
    if !peer_ranges.is_empty() {
        metadata.insert("peerDependencies".into(), json!(peer_ranges));
    }
    if !peer_meta.is_empty() {
        metadata.insert("peerDependenciesMeta".into(), Value::Object(peer_meta));
    }

    let entries = match map.get("snapshots") {
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .map(|snapshot| Ok(read_snapshot(snapshot, &what)?.over(&shared)))
            .collect::<Result<_, String>>()?,
        Some(Value::Array(_)) | None => vec![shared],
        Some(_) => return Err(format!("the snapshots of {key} must be a list")),
    };
    let package = Package {
        version: map
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        peer_ranges,
        optional_peers,
        metadata,
    };
    Ok((package, entries))
}

/// The v9 resolution of a `packages` entry, which leaves out what follows
/// from the entry: the integrity of a registry package, and the directory
/// of a `name@file:<path>` package.
fn resolution(key: &Key, map: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let integrity = map
        .get("integrity")
        .map(|value| string(value, "integrity"))
        .transpose()?;
    match (map.get("resolution"), integrity) {
        (None, Some(integrity)) => Ok(Map::from_iter([("integrity".into(), json!(integrity))])),
        (None, None) => {
            let directory = key
                .suffix
                .version
                .strip_prefix("file:")
                .ok_or_else(|| format!("The lockfile records no resolution for {key}"))?;
            Ok(Map::from_iter([
                ("type".into(), json!("directory")),
                ("directory".into(), json!(directory)),
            ]))
        }
        (Some(resolution), integrity) => {
            let mut resolution = object(resolution, "resolution")?.clone();
            if let Some(integrity) = integrity
                && resolution.contains_key("tarball")
                && !resolution.contains_key("type")
            {
                resolution
                    .entry("integrity")
                    .or_insert_with(|| json!(integrity));
            }
            Ok(resolution)
        }
    }
}

/// The references of a `dependencies` or `optionalDependencies` map of a
/// `packages` entry. A directory package's maps can also record what it
/// declares, as `{specifier}` entries, which link nothing.
fn package_deps(
    key: &Key,
    directory: bool,
    value: Option<&Value>,
    what: &str,
) -> Result<BTreeMap<String, NextRef>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let mut references = BTreeMap::new();
    for (alias, dep) in object(value, what)? {
        let invalid = |detail: &str| {
            format!("The dependency {alias} of {key} in the lockfile is invalid: {detail}")
        };
        let map = match dep {
            Value::String(text) => {
                let version = DepRef::parse(text).map_err(|err| invalid(&err))?;
                references.insert(alias.clone(), NextRef::Plain(version));
                continue;
            }
            Value::Object(map) => map,
            _ => return Err(invalid("not a version or a mapping")),
        };
        let declares =
            map.contains_key("specifier") || boolean(map.get("injected"), what)? == Some(true);
        if declares && !directory {
            return Err(invalid("a specifier in a package that records none"));
        }
        let version = map
            .get("version")
            .map(|version| {
                string(version, what)
                    .and_then(|text| DepRef::parse(text).map_err(|err| invalid(&err)))
            })
            .transpose()?;
        let with = map
            .get("with")
            .map(|with| named_bindings(with, what))
            .transpose()?;
        match (version, with) {
            (Some(version), with) => {
                references.insert(alias.clone(), NextRef::of(version, with));
            }
            (None, None) if map.contains_key("specifier") => {}
            (None, _) => return Err(invalid("no version")),
        }
    }
    Ok(references)
}

/// One entry of a package's `snapshots` list.
fn read_snapshot(value: &Value, what: &str) -> Result<Entry, String> {
    let map = object(value, what)?;
    let mut entry = Entry {
        dependencies: ref_map(map.get("dependencies"), what)?,
        optional_dependencies: ref_map(map.get("optionalDependencies"), what)?,
        unbound: map
            .get("unboundPeerDependencies")
            .map(|names| string_list(names, what))
            .transpose()?,
        optional: boolean(map.get("optional"), what)?.unwrap_or(false),
        ..Entry::default()
    };
    if let Some(peers) = map.get("peerDependencies") {
        for (name, details) in object(peers, what)? {
            let Some(details) = details.as_object() else {
                continue;
            };
            if let Some((below, binding)) = peer_binding(details, what)? {
                let bindings = if below {
                    &mut entry.below
                } else {
                    &mut entry.own
                };
                bindings.insert(name.clone(), binding);
            }
        }
    }
    Ok(entry)
}

fn string_list(value: &Value, what: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{what}: expected a list"))?
        .iter()
        .map(|name| string(name, what).map(str::to_string))
        .collect()
}

/// `settings.catalogs`, with each entry's version. A string entry is a
/// specifier whose version follows from it.
fn read_catalogs(value: Option<&Value>) -> Result<Catalogs, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let mut catalogs = BTreeMap::new();
    for (catalog, entries) in object(value, "catalogs")? {
        let mut resolved = BTreeMap::new();
        for (alias, entry) in object(entries, "catalogs")? {
            let pair = match entry {
                Value::String(specifier) => {
                    let version = catalog_implied_version(specifier).ok_or_else(|| {
                        format!(
                            "The lockfile does not say which version {alias} resolves to in the catalog {catalog}: nothing follows from the specifier {specifier}"
                        )
                    })?;
                    (specifier.clone(), version.to_string())
                }
                Value::Object(map) => {
                    let field = |name: &str| {
                        map.get(name)
                            .ok_or_else(|| format!("the catalog entry {alias} has no {name}"))
                            .and_then(|value| string(value, "catalog entry"))
                            .map(str::to_string)
                    };
                    (field("specifier")?, field("version")?)
                }
                _ => {
                    return Err(format!(
                        "the catalog entry {alias} must be a string or a mapping"
                    ));
                }
            };
            resolved.insert(alias.clone(), pair);
        }
        catalogs.insert(catalog.clone(), resolved);
    }
    Ok(catalogs)
}

/// The version a catalog specifier implies: itself, or the version of an
/// `npm:` alias.
fn catalog_implied_version(specifier: &str) -> Option<&str> {
    match specifier.strip_prefix("npm:") {
        None => Some(specifier),
        Some(target) => target.get(alias_name(target).len() + 1..),
    }
}

/// The package name of `name@range`, or all of it.
fn alias_name(target: &str) -> &str {
    refs::version_separator_index(target).map_or(target, |at| &target[..at])
}

/// `settings.patchedDependencies`: each key's hash.
fn read_patched(value: Option<&Value>) -> Result<BTreeMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    object(value, "patchedDependencies")?
        .iter()
        .map(|(key, entry)| {
            let hash = match entry {
                Value::String(hash) => hash.as_str(),
                Value::Object(map) => map
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("the patched dependency {key} has no hash"))?,
                _ => return Err(format!("the patched dependency {key} must be a hash")),
            };
            Ok((key.clone(), hash.to_string()))
        })
        .collect()
}

/// An importer as written.
struct WrittenImporter {
    name: Option<String>,
    groups: [Option<BTreeMap<String, WrittenDep>>; 3],
    skipped: Option<Value>,
}

fn read_importers(value: &Value) -> Result<BTreeMap<String, WrittenImporter>, String> {
    let mut importers = BTreeMap::new();
    for (id, importer) in object(value, "projects")? {
        let what = format!("project {id}");
        let map = object(importer, &what)?;
        let name = map
            .get("name")
            .map(|name| string(name, &what).map(str::to_string))
            .transpose()?;
        let mut groups: [Option<BTreeMap<String, WrittenDep>>; 3] = Default::default();
        for (group, slot) in GROUPS.iter().zip(&mut groups) {
            let Some(deps) = map.get(*group) else {
                continue;
            };
            let mut read = BTreeMap::new();
            for (alias, dep) in object(deps, &what)? {
                read.insert(alias.clone(), written_dep(dep, alias, &what)?);
            }
            *slot = Some(read);
        }
        let skipped = map.get("skippedOptionalDependencies").cloned();
        importers.insert(
            id.clone(),
            WrittenImporter {
                name,
                groups,
                skipped,
            },
        );
    }
    Ok(importers)
}

fn written_dep(value: &Value, alias: &str, what: &str) -> Result<WrittenDep, String> {
    match value {
        Value::String(specifier) => Ok(WrittenDep {
            specifier: specifier.clone(),
            version: None,
            peers: None,
        }),
        Value::Object(map) => {
            let specifier = map
                .get("specifier")
                .ok_or_else(|| format!("the dependency {alias} of {what} has no specifier"))?;
            Ok(WrittenDep {
                specifier: string(specifier, what)?.to_string(),
                version: map
                    .get("version")
                    .map(|version| string(version, what).map(str::to_string))
                    .transpose()?,
                peers: map
                    .get("with")
                    .map(|with| named_bindings(with, what))
                    .transpose()?,
            })
        }
        _ => Err(format!(
            "the dependency {alias} of {what} must be a specifier or a mapping"
        )),
    }
}

/// What an importer dependency's version follows from when the lockfile
/// leaves it out: its specifier, the catalogs, the injected packages and
/// the named projects.
struct Implied<'a> {
    catalogs: &'a Catalogs,
    /// Each injected package's directory, when only one is recorded.
    injected: BTreeMap<String, String>,
    /// Each project name's id, when only one project has it.
    linked: BTreeMap<String, String>,
}

impl<'a> Implied<'a> {
    /// `linked` is each linked project's id, by package name.
    fn new<'k>(
        catalogs: &'a Catalogs,
        package_keys: impl Iterator<Item = &'k Key>,
        linked: BTreeMap<String, String>,
    ) -> Self {
        let mut injected: BTreeMap<String, Option<String>> = BTreeMap::new();
        for key in package_keys {
            if let Some(path) = key.suffix.version.strip_prefix("file:") {
                injected
                    .entry(key.name.clone())
                    .and_modify(|found| *found = None)
                    .or_insert_with(|| Some(path.to_string()));
            }
        }
        Implied {
            catalogs,
            injected: injected
                .into_iter()
                .filter_map(|(name, path)| Some((name, path?)))
                .collect(),
            linked,
        }
    }

    /// Every dependency of an importer with its version.
    fn resolve(
        &self,
        id: &str,
        groups: &[Option<BTreeMap<String, WrittenDep>>; 3],
    ) -> Result<Importer, String> {
        let mut importer = Importer::default();
        for (slot, deps) in importer.groups.iter_mut().zip(groups) {
            let Some(deps) = deps else { continue };
            let mut resolved = BTreeMap::new();
            for (alias, dep) in deps {
                let version = match &dep.version {
                    Some(version) => Cow::Borrowed(version.as_str()),
                    None => self.version(id, alias, &dep.specifier).ok_or_else(|| {
                        format!(
                            "The lockfile does not say which version {alias} resolves to in {id}: nothing follows from the specifier {}",
                            dep.specifier
                        )
                    })?,
                };
                let version = ImporterVersion::parse(&version)
                    .map_err(|err| format!("the dependency {alias} of {id}: {err}"))?;
                resolved.insert(
                    alias.clone(),
                    ResolvedDep {
                        specifier: dep.specifier.clone(),
                        version,
                        peers: dep.peers.clone(),
                    },
                );
            }
            *slot = Some(resolved);
        }
        Ok(importer)
    }

    /// pnpm's `implied_version`.
    fn version(&self, id: &str, alias: &str, specifier: &str) -> Option<Cow<'_, str>> {
        if let Some(target) = specifier.strip_prefix("workspace:") {
            let name = refs::version_separator_index(target).map(|at| &target[..at]);
            let package = name.unwrap_or(alias);
            let injected = self.injected.get(package);
            let linked = self.linked.get(package).map(|target| link_path(id, target));
            return match (injected, linked) {
                (Some(path), None) => Some(Cow::Owned(match name {
                    Some(name) if name != alias => format!("{name}@file:{path}"),
                    _ => format!("file:{path}"),
                })),
                (None, Some(path)) => Some(Cow::Owned(format!("link:{path}"))),
                _ => None,
            };
        }
        if let Some(catalog) = specifier.strip_prefix("catalog:") {
            let catalog = if catalog.is_empty() {
                "default"
            } else {
                catalog
            };
            let (catalog_specifier, version) = self.catalogs.get(catalog)?.get(alias)?;
            return Some(match catalog_specifier.strip_prefix("npm:") {
                Some(target) => Cow::Owned(format!("{}@{version}", alias_name(target))),
                None => Cow::Borrowed(version),
            });
        }
        Some(Cow::Owned(
            specifier
                .strip_prefix("npm:")
                .unwrap_or(specifier)
                .to_string(),
        ))
    }
}

/// Records that `name` is the project `id`. A second, different claim
/// voids the name.
fn claim(claims: &mut BTreeMap<String, Option<String>>, name: &str, id: &str) {
    claims
        .entry(name.to_string())
        .and_modify(|found| {
            if found.as_deref() != Some(id) {
                *found = None;
            }
        })
        .or_insert_with(|| Some(id.to_string()));
}

/// The claims that stand, without the projects several names claim, since
/// a project records one name.
fn one_to_one(claims: BTreeMap<String, Option<String>>) -> BTreeMap<String, String> {
    let by_name: BTreeMap<String, String> = claims
        .into_iter()
        .filter_map(|(name, id)| Some((name, id?)))
        .collect();
    let mut seen = BTreeSet::new();
    let shared: BTreeSet<String> = by_name
        .values()
        .filter(|id| !seen.insert(id.as_str()))
        .cloned()
        .collect();
    by_name
        .into_iter()
        .filter(|(_, id)| !shared.contains(id))
        .collect()
}

/// The relative path from the project `from` to the project `to`.
fn link_path(from: &str, to: &str) -> String {
    let segments = |id: &'_ str| -> Vec<String> {
        id.split('/')
            .filter(|segment| !matches!(*segment, "" | "."))
            .map(str::to_string)
            .collect()
    };
    let (from, to) = (segments(from), segments(to));
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let path: Vec<&str> = std::iter::repeat_n("..", from.len() - common)
        .chain(to[common..].iter().map(String::as_str))
        .collect();
    if path.is_empty() {
        ".".to_string()
    } else {
        path.join("/")
    }
}

/// A rebuilt v9 snapshot.
#[derive(Debug, Default)]
struct SnapshotOut {
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub transitive_peer_dependencies: Vec<String>,
    pub optional: bool,
}

impl SnapshotOut {
    fn into_value(self) -> Value {
        let mut map = Map::new();
        if !self.dependencies.is_empty() {
            map.insert("dependencies".into(), json!(self.dependencies));
        }
        if !self.optional_dependencies.is_empty() {
            map.insert(
                "optionalDependencies".into(),
                json!(self.optional_dependencies),
            );
        }
        if !self.transitive_peer_dependencies.is_empty() {
            map.insert(
                "transitivePeerDependencies".into(),
                json!(self.transitive_peer_dependencies),
            );
        }
        if self.optional {
            map.insert("optional".into(), Value::Bool(true));
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests;
