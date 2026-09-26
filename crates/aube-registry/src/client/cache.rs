use crate::Packument;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Disk-cached packument with revalidation metadata.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedPackument {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    /// Unix epoch seconds when this entry was written
    pub(super) fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: Packument,
}

/// Validated JSON retained without allocating a tree of arbitrary fields.
/// The canonical cache still contains the complete registry response.
#[derive(Debug, Clone)]
pub(super) struct RawPackument(pub(super) bytes::Bytes);

impl RawPackument {
    pub(super) fn from_bytes(bytes: bytes::Bytes) -> Result<Self, sonic_rs::Error> {
        // LazyValue's scanner does not validate Unicode escape digits. Visit
        // every scalar without retaining it, matching the former Value decode.
        sonic_rs::from_slice::<ValidatedJson>(&bytes)?;
        Ok(Self(bytes))
    }

    pub(super) fn into_resolution(self) -> Result<crate::ResolutionPackument, sonic_rs::Error> {
        let projected: crate::resolution::RawResolutionPackument = sonic_rs::from_slice(&self.0)?;
        projected.into_resolution(&self.0)
    }
}

impl<'de> Deserialize<'de> for RawPackument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = sonic_rs::LazyValue::deserialize(deserializer)?;
        Self::from_bytes(bytes::Bytes::copy_from_slice(raw.as_raw_str().as_bytes()))
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for RawPackument {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let raw: sonic_rs::LazyValue<'_> =
            sonic_rs::from_slice(&self.0).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

/// Validate scalars through the normal decoder without allocating a JSON tree.
/// IgnoredAny would use the same permissive string-skipping path as LazyValue.
struct ValidatedJson;

impl<'de> Deserialize<'de> for ValidatedJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> serde::de::Visitor<'de> for ValidatedJson {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("valid JSON")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<Self>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        while map.next_entry::<Self, Self>()?.is_some() {}
        Ok(self)
    }
}

/// Disk-cached *full* (non-corgi) packument. Stored as raw JSON so we
/// preserve fields the resolver doesn't parse (`description`, `repository`,
/// `license`, `keywords`, `maintainers`, ...), for use by human-facing
/// commands like `aube view`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedFullPackument<T = serde_json::Value> {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: T,
}

#[derive(Debug, Default)]
pub struct CachedPackumentLookup {
    pub packument: Option<Packument>,
    pub stale: bool,
    pub(super) cached: Option<CachedPackumentLookupEntry>,
}

impl CachedPackumentLookup {
    /// Whether the retained cache inventory contains a version, including a
    /// stale entry awaiting revalidation. `None` means no cached inventory.
    pub fn contains_version(&self, version: &str) -> Option<bool> {
        self.packument
            .as_ref()
            .map(|packument| packument.versions.contains_key(version))
            .or_else(|| {
                self.cached.as_ref().map(|cached| match cached {
                    CachedPackumentLookupEntry::Abbreviated(cached) => {
                        cached.packument.versions.contains_key(version)
                    }
                    CachedPackumentLookupEntry::Full(cached) => {
                        cached.packument.versions.contains_key(version)
                    }
                })
            })
    }
}

/// A selective cache hit, or an already-read entry for normal revalidation.
#[derive(Debug, Default)]
pub struct CachedResolutionPackumentLookup {
    pub packument: Option<crate::ResolutionPackument>,
    pub revalidation: CachedPackumentLookup,
}

#[derive(Debug)]
pub(super) enum CachedPackumentLookupEntry {
    Abbreviated(CachedPackument),
    Full(CachedFullPackumentTyped),
}

#[derive(Debug)]
pub(super) struct CachedFullPackumentTyped {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) fetched_at: u64,
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: Packument,
}

/// How long to trust a cached packument before revalidating with the registry.
/// Trust cached packuments for 30 minutes before revalidating. This keeps
/// repeated installs in a long-lived dev session from devolving into hundreds
/// of conditional metadata requests once the cache is just over pnpm's 5-minute
/// default staleness window.
const PACKUMENT_TTL_SECS: u64 = 1800;

pub(super) fn cached_is_fresh(fetched_at: u64, max_age_secs: Option<u64>) -> bool {
    let age = now_secs().saturating_sub(fetched_at);
    let budget = max_age_secs.unwrap_or(PACKUMENT_TTL_SECS);
    age < budget
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pull ETag + Last-Modified off a response as owned strings.
pub(super) fn extract_cache_headers(resp: &reqwest::Response) -> (Option<String>, Option<String>) {
    let headers = resp.headers();
    let grab = |name: reqwest::header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    (
        grab(reqwest::header::ETAG),
        grab(reqwest::header::LAST_MODIFIED),
    )
}

pub(super) fn parse_cache_control_max_age(resp: &reqwest::Response) -> Option<u64> {
    let raw = resp
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())?;
    let mut max_age = None;
    let mut s_maxage = None;
    let mut force_revalidate = false;
    for directive in raw.split(',').map(str::trim) {
        let directive_lc = directive.to_ascii_lowercase();
        match directive_lc.as_str() {
            "no-store" | "no-cache" | "private" => force_revalidate = true,
            _ => {}
        }
        if let Some(val) = directive_lc.strip_prefix("s-maxage=") {
            s_maxage = val.parse::<u64>().ok();
        } else if let Some(val) = directive_lc.strip_prefix("max-age=") {
            max_age = val.parse::<u64>().ok();
        }
    }
    if force_revalidate {
        return Some(0);
    }
    s_maxage.or(max_age)
}

pub(super) fn packument_cache_path(
    cache_dir: &Path,
    name: &str,
    registry_url: &str,
) -> Option<PathBuf> {
    // `name` is derived from registry responses and user-written
    // manifests. `replace('/', "__")` alone would let `../../evil`
    // escape the cache directory and turn a first resolve into an
    // arbitrary-file-write primitive. Delegate to the store's
    // shared validator so the grammar never drifts across crates.
    let safe_name = aube_store::validate_and_encode_name(name)?;
    // Partition by registry origin: a packument fetched against
    // registry A must never be returned to a request that resolves
    // to registry B (CVE-2018-7167 class). Hash the URL so port,
    // trailing-slash, and scheme variants share the same bucket only
    // when literally identical bytes were configured.
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

fn registry_origin_segment(registry_url: &str) -> String {
    let digest = blake3::hash(registry_url.as_bytes()).to_hex();
    format!("origin-{}", &digest.as_str()[..16])
}

pub(super) fn read_cached_packument(path: &Path) -> Option<CachedPackument> {
    // sonic-rs is faster than serde_json on packument-shape JSON and,
    // unlike simd-json, takes an immutable `&[u8]` so the file content
    // doesn't need to be kept mutable for the parse to be zero-copy.
    let content = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&content).ok()
}

pub(super) fn write_cached_packument(path: &Path, cached: &CachedPackument) -> std::io::Result<()> {
    // sonic-rs serializer for symmetry with the read path; output
    // format doesn't need to match anything external (cache file we
    // own), so we trade serde_json's stable formatting for a small
    // throughput win on the cold-install metadata phase.
    let json = sonic_rs::to_vec(cached).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

pub(super) fn packument_full_cache_path(
    cache_dir: &Path,
    name: &str,
    registry_url: &str,
) -> Option<PathBuf> {
    let safe_name = aube_store::validate_and_encode_name(name)?;
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

/// Disk-cached compact trust history ([`crate::PackumentTrustHistory`])
/// for the lockfile trust-policy validator. Same revalidation envelope
/// as the packument caches, but the payload keeps only the `time` map
/// and per-version trust evidence — orders of magnitude smaller than
/// the raw full packument the validator previously cached.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedTrustHistory {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    /// Unix epoch seconds when this entry was written
    pub(super) fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_age_secs: Option<u64>,
    pub(super) history: crate::PackumentTrustHistory,
}

pub(super) fn read_cached_trust_history(path: &Path) -> Option<CachedTrustHistory> {
    let content = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&content).ok()
}

pub(super) fn write_cached_trust_history(
    path: &Path,
    cached: &CachedTrustHistory,
) -> std::io::Result<()> {
    let json = sonic_rs::to_vec(cached).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

pub(super) fn read_cached_full_packument<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Option<CachedFullPackument<T>> {
    let content = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&content).ok()
}

/// Typed fast-path read used by `fetch_packument_with_time_cached`
/// in the warm-cache branch. Reads the file once and uses `sonic-rs`
/// to deserialize the cached wrapper directly into a tiny typed struct
/// holding `fetched_at` plus a fully-typed [`Packument`].
///
/// Returns a missing lookup on file/parse errors, and a stale lookup
/// when revalidation is needed, so callers can decide whether a primer
/// fallback is safe without reading the cache a second time.
pub(super) fn read_cached_full_packument_typed_lookup(
    path: &Path,
    force_cache: bool,
) -> CachedPackumentLookup {
    #[derive(Deserialize)]
    struct Typed {
        etag: Option<String>,
        last_modified: Option<String>,
        fetched_at: u64,
        #[serde(default)]
        max_age_secs: Option<u64>,
        packument: Packument,
    }

    let Ok(content) = std::fs::read(path) else {
        return CachedPackumentLookup::default();
    };
    let Ok(typed) = sonic_rs::from_slice::<Typed>(&content) else {
        return CachedPackumentLookup::default();
    };
    let typed = CachedFullPackumentTyped {
        etag: typed.etag,
        last_modified: typed.last_modified,
        fetched_at: typed.fetched_at,
        max_age_secs: typed.max_age_secs,
        packument: typed.packument,
    };
    if !force_cache && !cached_is_fresh(typed.fetched_at, typed.max_age_secs) {
        return CachedPackumentLookup {
            packument: None,
            stale: true,
            cached: Some(CachedPackumentLookupEntry::Full(typed)),
        };
    }
    CachedPackumentLookup {
        packument: Some(typed.packument),
        stale: false,
        cached: None,
    }
}

pub(super) fn read_cached_full_packument_typed(
    path: &Path,
    force_cache: bool,
) -> Option<Packument> {
    read_cached_full_packument_typed_lookup(path, force_cache).packument
}

pub(super) fn write_cached_full_packument<T: Serialize>(
    path: &Path,
    etag: Option<&str>,
    last_modified: Option<&str>,
    fetched_at: u64,
    max_age_secs: Option<u64>,
    packument: &T,
) -> std::io::Result<()> {
    // Serialize by reference. Raw responses retain every field without
    // allocating a JSON tree; typed primer entries use the same envelope.
    #[derive(Serialize)]
    struct CachedFullPackumentRef<'a, T> {
        etag: Option<&'a str>,
        last_modified: Option<&'a str>,
        fetched_at: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_age_secs: Option<u64>,
        packument: &'a T,
    }
    let json = sonic_rs::to_vec(&CachedFullPackumentRef {
        etag,
        last_modified,
        fetched_at,
        max_age_secs,
        packument,
    })
    .map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}
