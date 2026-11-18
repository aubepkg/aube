//! Reference shapes of the v13 lockfile: package keys, the values of a
//! snapshot's dependency maps, and importer versions. They mirror the
//! parsing rules of pnpm's `PkgNameVerPeer`, `SnapshotDepRef` and
//! `ImporterDepVersion`, kept as plain strings because aube only needs
//! to compare and re-render them.

use std::collections::BTreeMap;
use std::fmt;

/// A version and the `(…)` groups that follow it, e.g. `1.0.0(react@18.3.1)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct VerPeer {
    pub version: String,
    pub peer: String,
}

impl VerPeer {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("an empty version".to_string());
        }
        if !value.ends_with(')') {
            if value.contains(['(', ')']) {
                return Err(format!("mismatched parentheses in `{value}`"));
            }
            return Ok(VerPeer {
                version: value.to_string(),
                peer: String::new(),
            });
        }
        let open = value
            .find('(')
            .ok_or_else(|| format!("mismatched parentheses in `{value}`"))?;
        if open == 0 {
            return Err(format!("no version before the groups of `{value}`"));
        }
        Ok(VerPeer {
            version: value[..open].to_string(),
            peer: value[open..].to_string(),
        })
    }

    pub fn without_peer(&self) -> VerPeer {
        self.with_peer(String::new())
    }

    pub fn with_peer(&self, peer: String) -> VerPeer {
        VerPeer {
            version: self.version.clone(),
            peer,
        }
    }
}

impl fmt::Display for VerPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.version, self.peer)
    }
}

/// `name@version(groups)`, the key of a package or a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Key {
    pub name: String,
    pub suffix: VerPeer,
}

impl Key {
    pub fn new(name: &str, suffix: VerPeer) -> Self {
        Key {
            name: name.to_string(),
            suffix,
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let at = version_separator_index(value)
            .ok_or_else(|| format!("`{value}` has no version after the package name"))?;
        let name = &value[..at];
        if matches!(name, "" | "@" | "@/") {
            return Err(format!("`{value}` has an empty package name"));
        }
        Ok(Key {
            name: name.to_string(),
            suffix: VerPeer::parse(&value[at + 1..])?,
        })
    }

    pub fn without_peer(&self) -> Key {
        Key {
            name: self.name.clone(),
            suffix: self.suffix.without_peer(),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.suffix)
    }
}

/// pnpm's `version_separator_index`: the `@` between a package name and
/// what follows it, skipping a scope's leading `@`.
pub(super) fn version_separator_index(value: &str) -> Option<usize> {
    match value.strip_prefix('@') {
        Some(rest) => rest.find('@').map(|offset| offset + 1),
        None => value.find('@'),
    }
}

/// Whether a reference names its package (`is-odd@3.0.1`) instead of
/// giving only the version under the alias it is listed by.
fn looks_like_alias(value: &str) -> bool {
    if value.starts_with('@') {
        return true;
    }
    let Some(at) = value.find('@') else {
        return false;
    };
    value.find('(').is_none_or(|paren| at < paren) && value.find(':').is_none_or(|colon| at < colon)
}

/// A value of a snapshot's `dependencies` map.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DepRef {
    Plain(VerPeer),
    Alias(Key),
    Link(String),
}

impl DepRef {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(target) = value.strip_prefix("link:") {
            return Ok(DepRef::Link(target.to_string()));
        }
        if looks_like_alias(value) {
            return Key::parse(value).map(DepRef::Alias);
        }
        VerPeer::parse(value).map(DepRef::Plain)
    }

    /// The key this reference stands for when listed under `alias`.
    pub fn resolve(&self, alias: &str) -> Option<Key> {
        match self {
            DepRef::Plain(suffix) => Some(Key::new(alias, suffix.clone())),
            DepRef::Alias(key) => Some(key.clone()),
            DepRef::Link(_) => None,
        }
    }

    pub fn as_link_target(&self) -> Option<&str> {
        match self {
            DepRef::Link(target) => Some(target),
            DepRef::Plain(_) | DepRef::Alias(_) => None,
        }
    }

    pub fn without_groups(&self) -> DepRef {
        match self {
            DepRef::Plain(suffix) => DepRef::Plain(suffix.without_peer()),
            DepRef::Alias(key) => DepRef::Alias(key.without_peer()),
            DepRef::Link(path) => DepRef::Link(path.clone()),
        }
    }

    /// This reference pointed at `key`, keeping its shape.
    pub fn with_key(&self, key: &Key) -> DepRef {
        match self {
            DepRef::Plain(_) => DepRef::Plain(key.suffix.clone()),
            DepRef::Alias(_) => DepRef::Alias(key.clone()),
            DepRef::Link(path) => DepRef::Link(path.clone()),
        }
    }
}

impl fmt::Display for DepRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DepRef::Plain(suffix) => suffix.fmt(f),
            DepRef::Alias(key) => key.fmt(f),
            DepRef::Link(target) => write!(f, "link:{target}"),
        }
    }
}

pub(super) type NamedBindings = BTreeMap<String, NextRef>;

/// A reference that may name, with `with`, the peers of the snapshot it
/// means when its package has several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NextRef {
    Plain(DepRef),
    Named {
        version: DepRef,
        with: NamedBindings,
    },
}

impl NextRef {
    pub fn version(&self) -> &DepRef {
        match self {
            NextRef::Plain(version) | NextRef::Named { version, .. } => version,
        }
    }

    pub fn peers(&self) -> Option<&NamedBindings> {
        match self {
            NextRef::Plain(_) => None,
            NextRef::Named { with, .. } => Some(with),
        }
    }

    pub fn of(version: DepRef, with: Option<NamedBindings>) -> NextRef {
        match with {
            Some(with) => NextRef::Named { version, with },
            None => NextRef::Plain(version),
        }
    }
}

/// Whether `binding` is the binding `wanted` names: the same version
/// once groups are dropped, and the same peers if `wanted` lists any.
pub(super) fn names_binding(wanted: &NextRef, binding: &NextRef) -> bool {
    *wanted.version() == binding.version().without_groups()
        && wanted.peers().is_none_or(|wanted| match binding.peers() {
            Some(actual) => actual == wanted,
            None => wanted.is_empty(),
        })
}

/// The `version` of an importer dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ImporterVersion {
    Regular(VerPeer),
    Alias(Key),
    Link(String),
    File(String),
}

impl ImporterVersion {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(target) = value.strip_prefix("link:") {
            return Ok(ImporterVersion::Link(target.to_string()));
        }
        if let Some(target) = value.strip_prefix("file:") {
            return Ok(ImporterVersion::File(target.to_string()));
        }
        if looks_like_alias(value) {
            return Key::parse(value).map(ImporterVersion::Alias);
        }
        VerPeer::parse(value).map(ImporterVersion::Regular)
    }

    pub fn resolved_key(&self, alias: &str) -> Option<Key> {
        match self {
            ImporterVersion::Regular(suffix) => Some(Key::new(alias, suffix.clone())),
            ImporterVersion::Alias(key) => Some(key.clone()),
            ImporterVersion::File(payload) => VerPeer::parse(&format!("file:{payload}"))
                .ok()
                .map(|suffix| Key::new(alias, suffix)),
            ImporterVersion::Link(_) => None,
        }
    }

    pub fn as_link_target(&self) -> Option<&str> {
        match self {
            ImporterVersion::Link(target) => Some(target),
            _ => None,
        }
    }

    /// This version pointed at `key`, keeping its shape.
    pub fn with_key(&self, key: &Key) -> ImporterVersion {
        match self {
            ImporterVersion::Regular(_) => ImporterVersion::Regular(key.suffix.clone()),
            ImporterVersion::Alias(_) => ImporterVersion::Alias(key.clone()),
            ImporterVersion::File(payload) => ImporterVersion::File(
                key.suffix
                    .to_string()
                    .strip_prefix("file:")
                    .map_or_else(|| payload.clone(), str::to_string),
            ),
            ImporterVersion::Link(path) => ImporterVersion::Link(path.clone()),
        }
    }
}

impl fmt::Display for ImporterVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImporterVersion::Regular(suffix) => suffix.fmt(f),
            ImporterVersion::Alias(key) => key.fmt(f),
            ImporterVersion::Link(target) => write!(f, "link:{target}"),
            ImporterVersion::File(target) => write!(f, "file:{target}"),
        }
    }
}
