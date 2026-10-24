use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct InstallPathInfo {
    pub(super) name: String,
    pub(super) dep_path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RawNpmLockfile {
    #[serde(rename = "lockfileVersion")]
    pub(super) lockfile_version: u32,
    #[serde(default)]
    pub(super) packages: BTreeMap<String, RawNpmPackage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawNpmPackage {
    /// npm emits this field only when the entry is an npm-alias
    /// (`"h3-v2": "npm:h3@..."` resolves to `node_modules/h3-v2` with
    /// `name: "h3"`). For non-aliased packages the name is recoverable
    /// from the install path and npm omits the field. We use the
    /// presence of this field — combined with inequality against the
    /// install-path segment — to detect aliases.
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default)]
    pub(super) integrity: Option<String>,
    /// Full registry tarball URL npm wrote when it locked this entry.
    /// We capture it so aliased packages (whose registry name differs
    /// from the install-path-derived name used to key the graph) don't
    /// need to re-derive the URL from the registry base — and so we
    /// can round-trip `resolved:` faithfully when we write back.
    #[serde(default)]
    pub(super) resolved: Option<String>,
    #[serde(default)]
    pub(super) link: bool,
    #[serde(default)]
    pub(super) peer: bool,
    #[serde(default)]
    pub(super) optional: bool,
    #[serde(default)]
    pub(super) in_bundle: bool,
    #[serde(default, alias = "bundledDependencies")]
    pub(super) bundle_dependencies: Vec<String>,
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) optional_dependencies: BTreeMap<String, String>,
    /// npm v7+ records `peerDependencies` verbatim on each package
    /// entry (pulled straight from the package's own `package.json`
    /// at lockfile-write time). The flat npm layout relies on peers
    /// being auto-installed into *some* ancestor `node_modules/` so
    /// Node's upward walk finds them, but aube's isolated layout
    /// wants them as explicit siblings — without this field, the
    /// resolver's peer-context pass has nothing to work with on the
    /// lockfile-driven install path and peers silently go missing
    /// from `.aube/<dep_path>/node_modules/`.
    #[serde(default)]
    pub(super) peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) peer_dependencies_meta: BTreeMap<String, RawNpmPeerDepMeta>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) libc: Vec<String>,
    /// Captured verbatim for round-trip. npm writes these on every
    /// package entry; dropping them on re-emit is one of the
    /// remaining sources of `aube install --no-frozen-lockfile`
    /// churn against native npm output.
    ///
    /// Uses `aube_manifest::engines_tolerant` so the legacy array
    /// shape (e.g. `ansi-html-community@0.0.8` ships
    /// `"engines": ["node >= 0.8.0"]` and npm preserves it verbatim
    /// in the lockfile) doesn't blow up the whole parse. We normalize
    /// the array to an empty map — same behavior modern npm gives the
    /// shape for engine-strict checks, and the same tolerance the
    /// manifest parser already applies.
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    pub(super) engines: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) bin: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) license: Option<RawNpmLicense>,
    #[serde(default)]
    pub(super) funding: Option<RawNpmFunding>,
    #[serde(default)]
    pub(super) has_install_script: bool,
}

pub(super) const BUNDLED_OPTIONAL_KEY: &str = "__aube_npm_bundled_optional";

pub(super) const BUNDLED_ENTRIES_KEY: &str = "__aube_npm_bundled_entries";

pub(super) const RETAINED_ENTRIES_KEY: &str = "__aube_npm_retained_entries";

pub(super) const ENTRY_METADATA_KEY: &str = "__aube_npm_entry_metadata";

pub(super) const WORKSPACES_KEY: &str = "__aube_npm_workspaces";

/// `LockedPackage::extra_meta` key carrying npm's `hasInstallScript: true`
/// so a parse/write cycle keeps it.
pub(super) const HAS_INSTALL_SCRIPT_KEY: &str = "hasInstallScript";

/// npm's `license:` field on a package entry. Modern npm writes the
/// SPDX expression as a bare string, but older packages (e.g. `tv4`)
/// still ship the deprecated object / array-of-objects shapes that
/// npm copies verbatim from the package's `package.json`:
///
/// 1. SPDX string: `"license": "MIT"`
/// 2. object: `"license": {"type": "MIT", "url": "…"}`
/// 3. array: `"license": [{"type": "Public Domain", …}, {"type": "MIT", …}]`
///
/// Aube only carries a single `license: Option<String>` on
/// `LockedPackage`, so on read we collapse to the first usable
/// `type` (or bare string element); on write we always emit the
/// bare string form.
#[derive(Debug, Clone, Default)]
pub(super) struct RawNpmLicense {
    pub(super) value: Option<String>,
}

impl<'de> Deserialize<'de> for RawNpmLicense {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct LicenseVisitor;

        impl<'de> Visitor<'de> for LicenseVisitor {
            type Value = RawNpmLicense;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an SPDX string, a {type: ...} object, or an array of either")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmLicense {
                    value: Some(v.to_owned()),
                })
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmLicense { value: Some(v) })
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut value: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "type" {
                        value = map.next_value::<Option<String>>()?;
                    } else {
                        // Skip unknown fields (e.g. `url`).
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(RawNpmLicense { value })
            }

            fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
            where
                S: SeqAccess<'de>,
            {
                // Pick the first usable license from the array; aube's
                // single-string model can't represent a list. Drain the
                // rest so the deserializer state stays consistent.
                let mut chosen: Option<String> = None;
                while let Some(item) = seq.next_element::<RawNpmLicense>()? {
                    if chosen.is_none() {
                        chosen = item.value;
                    }
                }
                Ok(RawNpmLicense { value: chosen })
            }
        }

        deserializer.deserialize_any(LicenseVisitor)
    }
}

/// npm's `funding:` block on a package entry. npm copies the field
/// verbatim from the package's `package.json`, which means all three
/// shapes the registry permits show up in real lockfiles:
///
/// 1. bare URL string: `"funding": "https://example.com/sponsor"`
/// 2. object: `"funding": {"url": "…", "type": "github"}`
/// 3. mixed array: `"funding": ["https://…", {"url": "…"}]`
///
/// `url` is the first URL found, which is what `LockedPackage::funding_url`
/// models. `raw` keeps the value verbatim so the npm writer can re-emit
/// it unchanged, `type` keys and every array element included.
#[derive(Debug, Clone)]
pub(super) struct RawNpmFunding {
    pub(super) url: Option<String>,
    pub(super) raw: serde_json::Value,
}

impl<'de> Deserialize<'de> for RawNpmFunding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(RawNpmFunding {
            url: funding_url(&raw),
            raw,
        })
    }
}

/// First URL in a `funding` value: the string itself, an object's `url`,
/// or the first array element that yields one.
fn funding_url(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(url) => Some(url.clone()),
        serde_json::Value::Object(map) => map.get("url")?.as_str().map(str::to_owned),
        serde_json::Value::Array(items) => items.iter().find_map(funding_url),
        _ => None,
    }
}

/// `peerDependenciesMeta` value — only `optional` is meaningful to
/// us today (matches pnpm's model). Other fields that might appear
/// (`description`, etc.) are preserved only as far as serde's
/// `deny_unknown_fields` stays off.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct RawNpmPeerDepMeta {
    #[serde(default)]
    pub(super) optional: bool,
}
