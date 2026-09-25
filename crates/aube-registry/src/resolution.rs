//! An in-memory projection of the existing cache, with deferred release decoding.
use crate::{Packument, VersionMetadata};
use serde::{Deserialize, Deserializer};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

/// Complete version inventory with dependency metadata decoded on demand.
/// This is never persisted separately from the authoritative packument cache.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolutionPackument {
    pub name: String,
    #[serde(default)]
    pub modified: Option<String>,
    #[serde(default)]
    pub versions: BTreeMap<String, ResolutionVersion>,
    #[serde(
        rename = "dist-tags",
        default,
        deserialize_with = "crate::non_string_tolerant_map"
    )]
    pub dist_tags: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "crate::non_string_tolerant_map")]
    pub time: BTreeMap<String, String>,
}

/// Borrow version JSON from a single cache read before retaining shared slices.
#[derive(Deserialize)]
pub(crate) struct RawResolutionPackument<'a> {
    name: String,
    #[serde(default)]
    modified: Option<String>,
    #[serde(default, borrow)]
    versions: BTreeMap<String, sonic_rs::LazyValue<'a>>,
    #[serde(
        rename = "dist-tags",
        default,
        deserialize_with = "crate::non_string_tolerant_map"
    )]
    dist_tags: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "crate::non_string_tolerant_map")]
    time: BTreeMap<String, String>,
}

impl RawResolutionPackument<'_> {
    /// Reuse the same read for stale-cache revalidation, decoding each release
    /// directly without building the compact projection first.
    pub(crate) fn into_packument(self) -> Option<Packument> {
        Some(Packument {
            name: self.name,
            modified: self.modified,
            versions: self
                .versions
                .into_iter()
                .map(|(key, raw)| {
                    sonic_rs::from_str(raw.as_raw_str()).map(|metadata| (key, metadata))
                })
                .collect::<Result<_, _>>()
                .ok()?,
            dist_tags: self.dist_tags,
            time: self.time,
        })
    }

    pub(crate) fn into_resolution(self, content: &bytes::Bytes) -> Option<ResolutionPackument> {
        let mut versions = BTreeMap::new();
        for (key, value) in self.versions {
            let raw = value.as_raw_str();
            // LazyValue borrows JSON objects from this input. Bounds checks
            // also make an unexpected owned value a cache miss, not a panic.
            let start = (raw.as_ptr() as usize).checked_sub(content.as_ptr() as usize)?;
            let end = start.checked_add(raw.len())?;
            content.get(start..end)?;
            let candidate: Candidate = sonic_rs::from_str(raw).ok()?;
            versions.insert(
                key,
                ResolutionVersion {
                    deprecated: candidate.deprecated.is_some(),
                    trust: candidate.trust(),
                    data: Arc::new(VersionData::Deferred {
                        raw: content.slice(start..end),
                        metadata: OnceLock::new(),
                    }),
                },
            );
        }
        Some(ResolutionPackument {
            name: self.name,
            modified: self.modified,
            versions,
            dist_tags: self.dist_tags,
            time: self.time,
        })
    }
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default, deserialize_with = "crate::deprecated_string")]
    deprecated: Option<String>,
    #[serde(default)]
    approver: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "_npmUser",
        deserialize_with = "crate::npm_user_tolerant"
    )]
    npm_user: Option<crate::NpmUser>,
    #[serde(default)]
    dist: Option<crate::VersionTrustDist>,
}

impl Candidate {
    fn trust(self) -> crate::VersionTrustMetadata {
        crate::VersionTrustMetadata {
            approver: self.approver,
            npm_user: self.npm_user,
            dist: self.dist,
        }
    }
}

/// A candidate retains its exact JSON until the resolver needs its full fields.
#[derive(Debug, Clone)]
pub struct ResolutionVersion {
    deprecated: bool,
    trust: crate::VersionTrustMetadata,
    data: Arc<VersionData>,
}

#[derive(Debug)]
enum VersionData {
    Deferred {
        raw: bytes::Bytes,
        metadata: OnceLock<Result<Box<VersionMetadata>, sonic_rs::Error>>,
    },
    Decoded(Box<VersionMetadata>),
}

impl ResolutionVersion {
    /// Complete publishing evidence, including versions never selected.
    pub fn trust_metadata(&self) -> &crate::VersionTrustMetadata {
        &self.trust
    }

    /// Whether registry deprecation status should lower this candidate's rank.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated
    }

    /// Decode once; failures remain errors rather than empty dependency lists.
    pub fn metadata(&self) -> Result<&VersionMetadata, &sonic_rs::Error> {
        match self.data.as_ref() {
            VersionData::Decoded(metadata) => Ok(metadata),
            VersionData::Deferred { raw, metadata } => metadata
                .get_or_init(|| sonic_rs::from_slice(raw).map(Box::new))
                .as_deref(),
        }
    }
}

impl<'de> Deserialize<'de> for ResolutionVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = sonic_rs::LazyValue::deserialize(deserializer)?;
        let candidate: Candidate =
            sonic_rs::from_str(raw.as_raw_str()).map_err(serde::de::Error::custom)?;
        Ok(Self {
            deprecated: candidate.deprecated.is_some(),
            trust: candidate.trust(),
            data: Arc::new(VersionData::Deferred {
                raw: bytes::Bytes::copy_from_slice(raw.as_raw_str().as_bytes()),
                metadata: OnceLock::new(),
            }),
        })
    }
}

impl From<VersionMetadata> for ResolutionVersion {
    fn from(metadata: VersionMetadata) -> Self {
        Self {
            deprecated: metadata.deprecated.is_some(),
            trust: crate::VersionTrustMetadata {
                approver: metadata.approver.clone(),
                npm_user: metadata.npm_user.clone(),
                dist: metadata.dist.as_ref().map(|d| crate::VersionTrustDist {
                    attestations: d.attestations.clone(),
                }),
            },
            data: Arc::new(VersionData::Decoded(Box::new(metadata))),
        }
    }
}

impl From<Packument> for ResolutionPackument {
    fn from(p: Packument) -> Self {
        Self {
            name: p.name,
            modified: p.modified,
            versions: p.versions.into_iter().map(|(k, v)| (k, v.into())).collect(),
            dist_tags: p.dist_tags,
            time: p.time,
        }
    }
}

impl ResolutionPackument {
    /// Materialize complete history for consumers that inspect full release fields.
    pub fn materialize(&self) -> Result<Packument, &sonic_rs::Error> {
        Ok(Packument {
            name: self.name.clone(),
            modified: self.modified.clone(),
            versions: self
                .versions
                .iter()
                .map(|(k, v)| Ok((k.clone(), v.metadata()?.clone())))
                .collect::<Result<_, _>>()?,
            dist_tags: self.dist_tags.clone(),
            time: self.time.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_metadata_matches_full_decoder_without_hydrating_other_versions() {
        let json = r#"{"name":"demo","versions":{
            "1.0.0":{"name":"demo","version":"different","dependencies":{"a":"^1","ignored":null},"optionalDependencies":{"opt":"2"},"peerDependencies":{"peer":"*"},"bundledDependencies":["a"],"bundleDependencies":["other"],"bin":"cli.js","engines":["node >=0.4"],"deprecated":false,"dist":{"tarball":"https://registry.example/demo.tgz","integrity":"sha512-test"}},
            "2.0.0":{"name":"demo","version":"2.0.0","deprecated":"old"}},
            "dist-tags":{"latest":"1.0.0","ignored":null},"time":{"1.0.0":"2024-01-01","ignored":false}}"#;
        let full: Packument = sonic_rs::from_str(json).unwrap();
        let lazy: ResolutionPackument = sonic_rs::from_str(json).unwrap();
        assert!(lazy.versions.values().all(|v| matches!(v.data.as_ref(), VersionData::Deferred { metadata, .. } if metadata.get().is_none())));
        assert_eq!(lazy.dist_tags, full.dist_tags);
        assert_eq!(lazy.time, full.time);
        assert_eq!(
            serde_json::to_value(lazy.versions["1.0.0"].metadata().unwrap()).unwrap(),
            serde_json::to_value(&full.versions["1.0.0"]).unwrap()
        );
        assert!(
            matches!(lazy.versions["2.0.0"].data.as_ref(), VersionData::Deferred { metadata, .. } if metadata.get().is_none())
        );
        assert!(lazy.versions["2.0.0"].is_deprecated());
        assert_eq!(
            serde_json::to_value(lazy.materialize().unwrap()).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }

    #[test]
    fn malformed_selected_metadata_is_an_error_not_empty_dependencies() {
        let lazy: ResolutionPackument =
            sonic_rs::from_str(r#"{"name":"demo","versions":{"1":{"dependencies":42}}}"#).unwrap();
        assert!(lazy.versions["1"].metadata().is_err());
        assert!(lazy.materialize().is_err());
    }

    #[test]
    fn full_materialization_keeps_historical_trust_evidence() {
        let json = r#"{"name":"demo","versions":{"1":{"name":"demo","version":"1","_npmUser":{"name":"bot","trustedPublisher":{"id":"github","oidcConfigId":"id"}},"dist":{"tarball":"url","attestations":{"url":"https://example.com/attestation","provenance":{"predicateType":"https://slsa.dev/provenance/v1"}}}},"2":{"name":"demo","version":"2","approver":{"name":"approved"}}},"time":{"1":"2024-01-01","2":"2024-02-01"}}"#;
        let full: Packument = sonic_rs::from_str(json).unwrap();
        let lazy: ResolutionPackument = sonic_rs::from_str(json).unwrap();
        let history: crate::PackumentTrustHistory = sonic_rs::from_str(json).unwrap();
        for (version, metadata) in &lazy.versions {
            assert_eq!(
                serde_json::to_value(metadata.trust_metadata()).unwrap(),
                serde_json::to_value(&history.versions[version]).unwrap()
            );
        }
        assert_eq!(
            serde_json::to_value(lazy.materialize().unwrap()).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }
}
