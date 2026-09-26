use crate::{Error, FxHashMap, FxHashSet, Resolver};
use aube_registry::client::RegistryClient;
use aube_registry::{Packument, VersionTrustMetadata};
use aube_util::adaptive::AdaptiveLimit;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use tokio::task::JoinSet;

/// Bound synchronous cache reads and JSON parsing independently from registry
/// capacity. The permit moves into `spawn_blocking` so cancellation cannot
/// release it while the blocking task is still running.
static PACKUMENT_CACHE_IO: LazyLock<Arc<tokio::sync::Semaphore>> = LazyLock::new(|| {
    let concurrency = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 32));
    Arc::new(tokio::sync::Semaphore::new(concurrency))
});

/// Spawns and tracks in-flight packument fetches.
///
/// Owns the `JoinSet` of running fetch tasks plus the bookkeeping the
/// resolver needs to dedupe spawns (`active_fetches`) and to know
/// which packuments came from the bundled primer
/// (`primer_seeded_names`, so range misses against the primer's
/// capped history can trigger a live refetch before reporting
/// `ERR_AUBE_NO_MATCHING_VERSION`).
///
/// Pre-clones the immutable Resolver bits the spawn body needs so
/// `ensure_fetch` doesn't need a `&Resolver` borrow at call time —
/// keeping it compatible with the BFS loop's `&mut self.resolver.cache`
/// access pattern.
pub(super) struct FetchScheduler {
    in_flight: JoinSet<(FetchKey, Result<FetchResult, Error>)>,
    active_fetches: FxHashSet<FetchKey>,
    task_keys: FxHashMap<tokio::task::Id, FetchKey>,
    primer_seeded_names: FxHashSet<String>,
    sem: Arc<AdaptiveLimit>,
    client: Arc<RegistryClient>,
    cache_dir: Option<PathBuf>,
    full_cache_dir: Option<PathBuf>,
    mra_exclude: crate::trust::PackageVersionPolicy,
    force_metadata_primer: bool,
    needs_time: bool,
}

pub(super) type TrustHistory = std::collections::BTreeMap<String, VersionTrustMetadata>;
pub(super) type FetchResult = (
    String,
    aube_registry::ResolutionPackument,
    FetchSource,
    Option<TrustHistory>,
);
pub(super) type FetchOutcome =
    Option<Result<(FetchKey, Result<FetchResult, Error>), tokio::task::JoinError>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum FetchKey {
    Full(String),
    Exact(String, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchSource {
    Disk,
    Primer,
    Network,
    Exact,
}

impl FetchScheduler {
    pub(super) fn new(resolver: &Resolver, sem: Arc<AdaptiveLimit>, needs_time: bool) -> Self {
        Self {
            in_flight: JoinSet::new(),
            active_fetches: FxHashSet::default(),
            task_keys: FxHashMap::default(),
            primer_seeded_names: FxHashSet::default(),
            sem,
            client: resolver.client.clone(),
            cache_dir: resolver.packument_cache_dir.clone(),
            full_cache_dir: resolver.packument_full_cache_dir.clone(),
            mra_exclude: resolver
                .minimum_release_age
                .as_ref()
                .map(|m| m.exclude.clone())
                .unwrap_or_else(crate::trust::PackageVersionPolicy::empty),
            force_metadata_primer: resolver.force_metadata_primer,
            needs_time,
        }
    }

    pub(super) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether an exact request for any version of `name` is still running.
    /// A successful exact response can supersede a retained full-fetch error
    /// by proving the package is reachable and requiring a fresh full fetch.
    pub(super) fn has_active_exact_fetch(&self, name: &str) -> bool {
        self.active_fetches
            .iter()
            .any(|key| matches!(key, FetchKey::Exact(active_name, _) if active_name == name))
    }

    /// Spawn a full fetch for `name` unless one was already scheduled.
    ///
    /// The caller is responsible for the resolver-cache gate — passing
    /// a name that's already in the cache wastes a spawn but is
    /// otherwise harmless.
    pub(super) fn ensure_fetch(
        &mut self,
        name: &str,
        published_by: Option<&str>,
        force_refresh: bool,
    ) {
        let key = FetchKey::Full(name.to_string());
        if !self.active_fetches.insert(key.clone()) {
            return;
        }
        // Only a name-only exclude lets us skip the cutoff sight-unseen;
        // a version-specific rule still needs publish times to tell which
        // versions are exempt, so it falls through to the cutoff check.
        let primer_covers_cutoff = self.mra_exclude.matches_name_only(name)
            || published_by.is_none_or(crate::primer::covers_cutoff);
        let inputs = FetchInputs {
            name: name.to_string(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
            full_cache_dir: self.full_cache_dir.clone(),
            primer_covers_cutoff,
            force_metadata_primer: self.force_metadata_primer,
            sem: self.sem.clone(),
            needs_time: self.needs_time,
            force_refresh,
        };
        let task_key = key.clone();
        let handle = self.in_flight.spawn(async move {
            let result = fetch_one_packument(inputs, true, None).await;
            (task_key, result)
        });
        self.task_keys.insert(handle.id(), key);
    }

    /// Fetch an exact optional dependency without retaining every historical
    /// version's dependency metadata. The full document is decoded into its
    /// publish-time/trust subset so policy checks keep their semantics.
    pub(super) fn ensure_exact_optional_fetch(
        &mut self,
        name: &str,
        version: &str,
        published_by: Option<&str>,
        force_refresh: bool,
    ) {
        let key = FetchKey::Exact(name.to_string(), version.to_string());
        if !self.active_fetches.insert(key.clone()) {
            return;
        }
        let primer_covers_cutoff = self.mra_exclude.matches_name_only(name)
            || published_by.is_none_or(crate::primer::covers_cutoff);
        let inputs = FetchInputs {
            name: name.to_string(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
            full_cache_dir: self.full_cache_dir.clone(),
            primer_covers_cutoff,
            force_metadata_primer: self.force_metadata_primer,
            sem: self.sem.clone(),
            needs_time: self.needs_time,
            force_refresh,
        };
        let version = version.to_string();
        let task_key = key.clone();
        let handle = self.in_flight.spawn(async move {
            let result = fetch_exact_optional_packument(inputs, version).await;
            (task_key, result)
        });
        self.task_keys.insert(handle.id(), key);
    }

    /// Wait for the next in-flight fetch to complete.
    pub(super) async fn join_next(&mut self) -> FetchOutcome {
        match self.in_flight.join_next_with_id().await {
            Some(Ok((id, outcome))) => {
                self.task_keys.remove(&id);
                self.active_fetches.remove(&outcome.0);
                Some(Ok(outcome))
            }
            Some(Err(error)) => {
                if let Some(key) = self.task_keys.remove(&error.id()) {
                    self.active_fetches.remove(&key);
                }
                Some(Err(error))
            }
            None => None,
        }
    }

    pub(super) fn note_primer_seeded(&mut self, name: String) {
        self.primer_seeded_names.insert(name);
    }

    /// Returns true if `name` was marked as primer-seeded, removing it.
    pub(super) fn take_primer_seeded(&mut self, name: &str) -> bool {
        self.primer_seeded_names.remove(name)
    }

    pub(super) async fn drain(&mut self) {
        while self.join_next().await.is_some() {}
    }
}

/// Inputs the packument-fetch task needs once it's spawned.
///
/// All fields are owned/`Arc`-cloned so the future can be moved into
/// the resolver's `JoinSet` without borrowing the outer scope.
#[derive(Clone)]
struct FetchInputs {
    name: String,
    client: Arc<RegistryClient>,
    cache_dir: Option<PathBuf>,
    full_cache_dir: Option<PathBuf>,
    /// Precomputed from the resolver's `minimum_release_age` exclude
    /// list and `published_by` cutoff — if false, the primer is
    /// bypassed even when it would otherwise be eligible.
    primer_covers_cutoff: bool,
    /// `force_metadata_primer` from the resolver: when true, use the
    /// primer even for non-default registries (and rewrite tarball URLs
    /// to the active registry).
    force_metadata_primer: bool,
    sem: Arc<AdaptiveLimit>,
    /// True when the caller needs the packument's `time:` map and
    /// must therefore use the full-packument path.
    needs_time: bool,
    /// Ignore an apparently fresh full-packument disk entry because a newer
    /// compact response proved that it is incomplete.
    force_refresh: bool,
}

/// Reuse the authoritative full cache for both ranges and exact optionals.
async fn fetch_cached_resolution(
    inputs: &FetchInputs,
) -> Result<Option<aube_registry::client::CachedResolutionPackumentLookup>, Error> {
    if !inputs.needs_time || inputs.force_refresh {
        return Ok(None);
    }
    let Some(dir) = inputs.full_cache_dir.clone() else {
        return Ok(None);
    };
    let permit = Arc::clone(&PACKUMENT_CACHE_IO)
        .acquire_owned()
        .await
        .map_err(|e| Error::Registry(inputs.name.clone(), e.to_string()))?;
    let client = Arc::clone(&inputs.client);
    let name = inputs.name.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Some(client.cached_resolution_packument(&name, &dir))
    })
    .await
    .map_err(|e| Error::Registry(inputs.name.clone(), e.to_string()))
}

/// Body of the per-packument fetch task spawned by the resolver.
///
/// Returns the result source so callers can distinguish incomplete local
/// metadata from a live registry response.
async fn fetch_one_packument(
    mut inputs: FetchInputs,
    allow_primer: bool,
    required_version: Option<&str>,
) -> Result<FetchResult, Error> {
    let _diag_span =
        aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "packument_fetch")
            .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&inputs.name)));
    let _diag_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Pack);
    let requested_refresh = inputs.force_refresh;
    let mut selective = fetch_cached_resolution(&inputs).await?;
    if let Some(version) = required_version
        && let Some(lookup) = selective.as_ref()
        && lookup
            .packument
            .as_ref()
            .map(|packument| packument.versions.contains_key(version))
            .or_else(|| lookup.revalidation.contains_version(version))
            == Some(false)
    {
        // Do not send a validator for an inventory already known to lack the
        // requested version: a 304 cannot fill in a capped primer's history.
        inputs.force_refresh = true;
        selective = None;
    }
    if let Some(indexed) = selective
        .as_mut()
        .and_then(|lookup| lookup.packument.take())
    {
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_disk_hit",
            || {
                format!(
                    r#"{{"name":{},"versions":{}}}"#,
                    aube_util::diag::jstr(&inputs.name),
                    indexed.versions.len()
                )
            },
        );
        return Ok((inputs.name, indexed, FetchSource::Disk, None));
    }
    let mut primer_inputs = inputs.clone();
    // An incomplete inventory needs a live refresh, but must not disable an
    // otherwise eligible offline fallback. Explicit refresh requests still do.
    primer_inputs.force_refresh = requested_refresh;
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        sem,
        needs_time,
        force_refresh,
        ..
    } = inputs;
    let cache_lookup_dir = if needs_time {
        full_cache_dir.clone()
    } else {
        cache_dir.clone()
    };
    let mut cached = if force_refresh && needs_time {
        // Bypass validators without deleting usable versions if the request fails.
        Default::default()
    } else if let Some(lookup) = selective {
        lookup.revalidation
    } else if let Some(cache_lookup_dir) = cache_lookup_dir {
        let cache_io_permit = Arc::clone(&PACKUMENT_CACHE_IO)
            .acquire_owned()
            .await
            .map_err(|e| Error::Registry(name.clone(), e.to_string()))?;
        let lookup_client = Arc::clone(&client);
        let lookup_name = name.clone();
        tokio::task::spawn_blocking(move || {
            let _cache_io_permit = cache_io_permit;
            if needs_time {
                lookup_client.cached_full_packument_lookup(&lookup_name, &cache_lookup_dir)
            } else {
                lookup_client.cached_packument_lookup(&lookup_name, &cache_lookup_dir)
            }
        })
        .await
        .map_err(|e| Error::Registry(name.clone(), format!("packument cache lookup: {e}")))?
    } else {
        Default::default()
    };
    if let Some(packument) = cached.packument.take() {
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_disk_hit",
            || {
                format!(
                    r#"{{"name":{},"versions":{}}}"#,
                    aube_util::diag::jstr(&name),
                    packument.versions.len()
                )
            },
        );
        return Ok((name, packument.into(), FetchSource::Disk, None));
    }
    if allow_primer
        && !force_refresh
        && !cached.stale
        && let Some(fetched) = fetch_primer(&primer_inputs, None, true)
    {
        return Ok(fetched);
    }
    let primer_fallback = required_version.is_some() && !cached.stale;
    // The adaptive limit models registry capacity. Local metadata does not
    // consume that capacity and must not queue behind slow HTTP requests.
    let permit_wait = std::time::Instant::now();
    let permit = sem.acquire().await;
    let permit_wait_ms = permit_wait.elapsed();
    if permit_wait_ms.as_millis() > 1 {
        aube_util::diag::event_lazy(
            aube_util::diag::Category::Resolver,
            "packument_permit_wait",
            permit_wait_ms,
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
    }
    aube_util::diag::attribute_wait(aube_util::diag::Slot::Pack, &name, permit_wait_ms);
    let _holder_guard = aube_util::diag::register_holder(aube_util::diag::Slot::Pack, &name);
    let fetch_outcome = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) if force_refresh => {
                client
                    .refresh_resolution_packument(
                        &name,
                        dir,
                        if requested_refresh {
                            None
                        } else {
                            required_version
                        },
                    )
                    .await
            }
            Some(dir) => {
                client
                    .fetch_resolution_packument_after_lookup(&name, dir, cached)
                    .await
            }
            None => client.fetch_packument(&name).await.map(Into::into),
        }
    } else if let Some(ref dir) = cache_dir {
        client
            .fetch_packument_cached_after_lookup(&name, dir, cached)
            .await
            .map(Into::into)
    } else {
        client.fetch_packument(&name).await.map(Into::into)
    };
    let packument = match fetch_outcome {
        Ok(p) => {
            permit.record_success();
            p
        }
        Err(e) => {
            if e.is_throttle() {
                permit.record_throttle();
            } else {
                permit.record_cancelled();
            }
            if primer_fallback
                && let Some(fetched) = fetch_primer(&primer_inputs, required_version, false)
            {
                return Ok(fetched);
            }
            return Err(Error::Registry(name.clone(), e.to_string()));
        }
    };
    aube_util::diag::instant_lazy(
        aube_util::diag::Category::Resolver,
        "packument_network_hit",
        || {
            format!(
                r#"{{"name":{},"versions":{}}}"#,
                aube_util::diag::jstr(&name),
                packument.versions.len()
            )
        },
    );
    Ok((name, packument, FetchSource::Network, None))
}

fn fetch_primer(
    inputs: &FetchInputs,
    required_version: Option<&str>,
    seed_cache: bool,
) -> Option<FetchResult> {
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        primer_covers_cutoff,
        force_metadata_primer,
        needs_time,
        force_refresh,
        ..
    } = inputs;
    if !*force_refresh
        && (*force_metadata_primer || client.uses_default_npm_registry_for(name))
        && *primer_covers_cutoff
        && let Some(seed) = crate::primer::get(name)
    {
        let mut packument = seed.packument();
        if required_version.is_some_and(|version| !packument.versions.contains_key(version)) {
            return None;
        }
        if *force_metadata_primer {
            for version in packument.versions.values_mut() {
                let tarball = client.tarball_url(&version.name, &version.version);
                version.dist = version.dist.take().map(|mut dist| {
                    dist.tarball = tarball;
                    dist
                });
            }
        }
        // Error fallbacks stay in memory: they must not replace an existing
        // authoritative inventory with the primer's capped history.
        if seed_cache && *needs_time {
            if let Some(dir) = full_cache_dir.as_ref() {
                client.seed_full_packument_cache(
                    name,
                    dir,
                    &packument,
                    seed.etag.as_deref(),
                    seed.last_modified.as_deref(),
                    false,
                );
            }
        } else if seed_cache && let Some(dir) = cache_dir.as_ref() {
            client.seed_packument_cache(
                name,
                dir,
                &packument,
                seed.etag.as_deref(),
                seed.last_modified.as_deref(),
                false,
            );
        }
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_primer_hit",
            || {
                format!(
                    r#"{{"name":{},"versions":{}}}"#,
                    aube_util::diag::jstr(name),
                    packument.versions.len()
                )
            },
        );
        return Some((name.clone(), packument.into(), FetchSource::Primer, None));
    }
    None
}

async fn fetch_exact_optional_packument(
    inputs: FetchInputs,
    version: String,
) -> Result<FetchResult, Error> {
    if inputs.needs_time && inputs.full_cache_dir.is_some() {
        // Exact optional fetches already download the full document for trust
        // history. Reuse the canonical full cache and selective decoder so a
        // later install can use that response without another full download.
        return fetch_full_exact_optional(inputs, version, false).await;
    }
    let permit = inputs.sem.acquire().await;
    let name = inputs.name.clone();
    let fetched = inputs
        .client
        .fetch_exact_version_packument(&name, &version)
        .await;
    match fetched {
        Ok(exact) => {
            permit.record_success();
            let mut versions = std::collections::BTreeMap::new();
            versions.insert(version, exact.metadata);
            Ok((
                name.clone(),
                Packument {
                    name,
                    modified: None,
                    versions,
                    dist_tags: std::collections::BTreeMap::new(),
                    time: exact.history.time,
                }
                .into(),
                FetchSource::Exact,
                Some(exact.history.versions),
            ))
        }
        Err(err) => {
            tracing::debug!(
                "compact exact metadata fetch failed for optional dep {name}@{version}; falling back to full packument: {err}"
            );
            permit.record_cancelled();
            fetch_full_exact_optional(inputs, version, true).await
        }
    }
}

async fn fetch_full_exact_optional(
    inputs: FetchInputs,
    version: String,
    allow_primer: bool,
) -> Result<FetchResult, Error> {
    let mut refresh_inputs = inputs.clone();
    // Live exact fetches do not seed a capped primer. Preserve the existing
    // primer fallback when an uncached compact request has failed, though.
    let fetched = fetch_one_packument(inputs, allow_primer, Some(&version)).await?;
    if fetched.1.versions.contains_key(&version) {
        return Ok(fetched);
    }
    if !refresh_inputs.force_refresh && matches!(fetched.2, FetchSource::Disk | FetchSource::Primer)
    {
        // A primer fallback can still omit the requested version. A live
        // response is authoritative and must not trigger another download.
        refresh_inputs.force_refresh = true;
        let refreshed = fetch_one_packument(refresh_inputs, false, Some(&version)).await?;
        if refreshed.1.versions.contains_key(&version) {
            return Ok(refreshed);
        }
    }
    Err(Error::Registry(
        fetched.0,
        format!("version {version} is missing from the full packument"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_registry_responses(
        bodies: Vec<Vec<u8>>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server = {
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let request = requests.fetch_add(1, Ordering::Relaxed);
                    let body = bodies[request.min(bodies.len() - 1)].clone();
                    tokio::spawn(async move {
                        let mut buf = [0_u8; 2048];
                        let _ = socket.read(&mut buf).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.write_all(&body).await.unwrap();
                    });
                }
            })
        };
        (registry, requests, server)
    }

    async fn serve_registry(
        body: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        serve_registry_responses(vec![body]).await
    }

    #[tokio::test]
    async fn exact_optional_uses_fresh_full_cache_without_network_capacity() {
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new("http://127.0.0.1:9"));
        let full: Packument = serde_json::from_value(serde_json::json!({
            "name":"cache-only-demo", "versions": {
                "1.0.0":{"name":"cache-only-demo","version":"1.0.0","dependencies":{"child":"^1"}},
                "2.0.0":{"name":"cache-only-demo","version":"2.0.0","deprecated":"old"}
            }, "time":{"1.0.0":"2024-01-01","2.0.0":"2024-02-01"}
        }))
        .unwrap();
        client.seed_full_packument_cache("cache-only-demo", cache.path(), &full, None, None, true);
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let _held = limiter.acquire().await;
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, limiter, true);
        scheduler.ensure_exact_optional_fetch("cache-only-demo", "1.0.0", None, false);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), scheduler.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .1
            .unwrap();
        assert_eq!(result.2, FetchSource::Disk);
        assert!(result.3.is_none()); // Complete inventory, not an exact-only response.
        assert_eq!(result.1.versions.len(), 2);
        assert_eq!(
            result.1.versions["1.0.0"].metadata().unwrap().dependencies["child"],
            "^1"
        );
        assert_eq!(result.1.time, full.time);
    }

    #[tokio::test]
    async fn exact_optional_populates_the_full_cache_for_later_installs() {
        let cache = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "name":"shared", "versions": {
                "1.0.0":{"name":"shared","version":"1.0.0","dependencies":{"child":"^1"}},
                "2.0.0":{"name":"shared","version":"2.0.0","approver":{"name":"approved"}}
            }, "time":{"1.0.0":"2024-01-01","2.0.0":"2024-02-01"}
        });
        let (registry, requests, server) = serve_registry(serde_json::to_vec(&body).unwrap()).await;
        let client = Arc::new(RegistryClient::new(&registry));
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut first = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        first.ensure_exact_optional_fetch("shared", "1.0.0", None, false);
        let (_, result) = first.join_next().await.unwrap().unwrap();
        let (_, packument, source, history) = result.unwrap();
        assert_eq!(source, FetchSource::Network);
        assert!(history.is_none()); // The full inventory carries its own trust history.
        assert_eq!(packument.versions.len(), 2);
        assert!(
            packument.versions["2.0.0"]
                .trust_metadata()
                .approver
                .is_some()
        );
        assert_eq!(
            packument.versions["1.0.0"].metadata().unwrap().dependencies["child"],
            "^1"
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        // A separate resolver must reuse the disk response even while all
        // registry capacity is occupied. Request the other version to prove
        // that this is the canonical full cache, not an exact-version entry.
        let client = Arc::new(RegistryClient::new(&registry));
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let _held = limiter.acquire().await;
        let mut second = FetchScheduler::new(&resolver, limiter, true);
        second.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
        let (_, result) =
            tokio::time::timeout(std::time::Duration::from_secs(2), second.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        let (_, packument, source, _) = result.unwrap();
        assert_eq!(source, FetchSource::Disk);
        assert_eq!(packument.versions.len(), 2);
        assert!(
            packument.versions["2.0.0"]
                .trust_metadata()
                .approver
                .is_some()
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn superseded_exact_cache_snapshot_can_retry_live() {
        let cache = tempfile::tempdir().unwrap();
        let exact =
            serde_json::json!({"name":"shared", "version":"1.0.0", "dependencies":{"fresh":"^2"}});
        let history = serde_json::json!({"name":"shared", "versions":{"1.0.0":exact.clone()}, "time":{"1.0.0":"2024-01-01"}});
        let (registry, requests, server) =
            serve_registry(serde_json::to_vec(&history).unwrap()).await;
        let client = Arc::new(RegistryClient::new(&registry));
        let old = serde_json::from_value(serde_json::json!({"name":"shared", "versions":{"1.0.0":{"name":"shared","version":"1.0.0","dependencies":{"old":"^1"}}}})).unwrap();
        client.seed_full_packument_cache("shared", cache.path(), &old, None, None, true);
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch("shared", "1.0.0", None, true);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        let (_, packument, source, _) = result.unwrap();
        assert_eq!(source, FetchSource::Network);
        assert_eq!(
            packument.versions["1.0.0"].metadata().unwrap().dependencies["fresh"],
            "^2"
        );
        assert!(requests.load(Ordering::Relaxed) > 0);
        server.abort();
    }

    #[tokio::test]
    async fn malformed_selected_disk_metadata_is_invalidated_and_refetched() {
        let cache = tempfile::tempdir().unwrap();
        let bad = serde_json::json!({"name":"shared", "versions":{"1.0.0":{"name":"shared","version":"1.0.0","dependencies":42}}, "dist-tags":{"latest":"1.0.0"}});
        let good = serde_json::json!({"name":"shared", "versions":{"1.0.0":{"name":"shared","version":"1.0.0","dist":{"tarball":"https://example.com/shared.tgz"}}}, "dist-tags":{"latest":"1.0.0"}, "time":{"1.0.0":"2024-01-01T00:00:00.000Z"}});
        let (registry, requests, server) = serve_registry_responses(vec![
            serde_json::to_vec(&bad).unwrap(),
            serde_json::to_vec(&good).unwrap(),
        ])
        .await;
        let client = Arc::new(RegistryClient::new(&registry));
        // Raw JSON is cached before typed decoding rejects the first response.
        assert!(
            client
                .fetch_packument_with_time_cached("shared", cache.path())
                .await
                .is_err()
        );
        assert!(
            client
                .cached_resolution_packument("shared", cache.path())
                .packument
                .is_some()
        );
        let mut resolver =
            Resolver::new(client.clone()).with_packument_full_cache(cache.path().to_path_buf());
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .dependencies
            .insert("shared".into(), "1.0.0".into());
        let graph = resolver.resolve(&manifest, None).await.unwrap();
        assert_eq!(graph.packages.len(), 1);
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        assert!(
            client
                .cached_resolution_packument("shared", cache.path())
                .packument
                .unwrap()
                .versions["1.0.0"]
                .metadata()
                .is_ok()
        );
        server.abort();
    }

    #[tokio::test]
    async fn completed_full_fetch_can_force_refresh_a_fresh_disk_entry() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let stale = crate::primer::get(name).unwrap().packument();
        let Some(mut new_metadata) = stale.versions.values().next().cloned() else {
            return;
        };
        let new_version = "9999.0.0";
        new_metadata.version = new_version.to_string();
        let mut fresh = stale.clone();
        fresh.versions.insert(new_version.to_string(), new_metadata);
        let stale_len = stale.versions.len();
        let (registry, requests, server) =
            serve_registry(serde_json::to_vec(&fresh).unwrap()).await;

        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        client.seed_full_packument_cache(name, cache.path(), &stale, None, None, true);
        let resolver = Resolver::new(Arc::clone(&client))
            .with_packument_full_cache(cache.path().to_path_buf())
            .with_force_metadata_primer(true);
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_fetch(name, None, false);
        let Some(Ok((FetchKey::Full(_), Ok((_, first, _, _))))) = scheduler.join_next().await
        else {
            panic!("cached full fetch did not complete");
        };
        assert_eq!(first.versions.len(), stale_len);
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        scheduler.ensure_fetch(name, None, true);
        let Some(Ok((FetchKey::Full(_), Ok((_, refreshed, _, _))))) = scheduler.join_next().await
        else {
            panic!("forced full refresh did not restart");
        };
        assert!(refreshed.versions.contains_key(new_version));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn disk_metadata_does_not_wait_for_a_network_permit() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new("https://registry.npmjs.org"));
        client.seed_packument_cache(
            name,
            cache.path(),
            &crate::primer::get(name).unwrap().packument(),
            None,
            None,
            true,
        );
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let held_network_permit = limiter.acquire().await;
        let resolver = Resolver::new(client).with_packument_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, limiter, false);

        scheduler.ensure_fetch(name, None, false);
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(1), scheduler.join_next())
                .await
                .expect("disk lookup queued behind the network permit");
        let Some(Ok((FetchKey::Full(_), Ok((_, _, source, _))))) = outcome else {
            panic!("disk fetch did not complete");
        };
        assert_eq!(source, FetchSource::Disk);
        drop(held_network_permit);
    }

    #[tokio::test]
    async fn primer_metadata_does_not_wait_for_a_network_permit() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let held_network_permit = limiter.acquire().await;
        let resolver = Resolver::new(Arc::new(RegistryClient::new("https://registry.npmjs.org")))
            .with_force_metadata_primer(true);
        let mut scheduler = FetchScheduler::new(&resolver, limiter, false);

        scheduler.ensure_fetch(name, None, false);
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(1), scheduler.join_next())
                .await
                .expect("primer lookup queued behind the network permit");
        let Some(Ok((FetchKey::Full(_), Ok((_, _, source, _))))) = outcome else {
            panic!("primer fetch did not complete");
        };
        assert_eq!(source, FetchSource::Primer);
        drop(held_network_permit);
    }

    #[tokio::test]
    async fn missing_exact_version_fallback_returns_a_terminal_error() {
        let body = serde_json::to_vec(&serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" }
            },
            "dist-tags": { "latest": "1.0.0" },
            "time": { "1.0.0": "2024-01-01T00:00:00.000Z" }
        }))
        .unwrap();
        let (registry, requests, server) = serve_registry(body).await;
        let resolver = Resolver::new(Arc::new(RegistryClient::new(&registry)));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
        let Some(Ok((FetchKey::Exact(_, version), Err(Error::Registry(_, message))))) =
            scheduler.join_next().await
        else {
            panic!("missing exact version did not return a terminal registry error");
        };
        assert_eq!(version, "2.0.0");
        assert!(message.contains("version 2.0.0 is missing"));
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        server.abort();
    }

    #[tokio::test]
    async fn exact_optional_revalidates_complete_but_refreshes_incomplete_inventories() {
        for complete in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let registry = format!("http://{}/", listener.local_addr().unwrap());
            let body = serde_json::json!({"name":"shared","versions":{
                "1.0.0":{"name":"shared","version":"1.0.0"},
                "2.0.0":{"name":"shared","version":"2.0.0"}
            },"time":{"1.0.0":"2024-01-01","2.0.0":"2024-02-01"}})
            .to_string();
            let mut inventory: Packument = serde_json::from_str(&body).unwrap();
            if !complete {
                inventory.versions.remove("2.0.0");
            }
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 2048];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert_eq!(request.contains("if-none-match: \"old\""), complete);
                if complete {
                    socket.write_all(b"HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\ncache-control: max-age=3600\r\nconnection: close\r\n\r\n").await.unwrap();
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let cache = tempfile::tempdir().unwrap();
            let client = Arc::new(RegistryClient::new(&registry));
            client.seed_full_packument_cache(
                "shared",
                cache.path(),
                &inventory,
                Some("\"old\""),
                None,
                false,
            );
            let resolver =
                Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
            let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
            scheduler.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
            let (_, result) =
                tokio::time::timeout(std::time::Duration::from_secs(2), scheduler.join_next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            assert!(result.unwrap().1.versions.contains_key("2.0.0"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn exact_optional_preserves_offline_primer_fallback() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let primer = crate::primer::get(name).unwrap().packument();
        let version = primer.versions.keys().next().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://registry.npmjs.org")
            .with_network_mode(aube_registry::NetworkMode::Offline);
        let resolver =
            Resolver::new(Arc::new(client)).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch(name, version, None, false);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        let (_, packument, source, _) = result.unwrap();
        assert_eq!(source, FetchSource::Primer);
        assert!(packument.versions.contains_key(version));
    }

    #[tokio::test]
    async fn incomplete_exact_cache_preserves_offline_fallback_unless_refresh_was_requested() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let mut primer = crate::primer::get(name).unwrap().packument();
        let version = primer.versions.keys().next().unwrap().clone();
        primer.versions.remove(&version);
        for force_refresh in [false, true] {
            let cache = tempfile::tempdir().unwrap();
            let client = RegistryClient::new("https://registry.npmjs.org")
                .with_network_mode(aube_registry::NetworkMode::Offline);
            client.seed_full_packument_cache(name, cache.path(), &primer, None, None, true);
            let client = Arc::new(client);
            let resolver =
                Resolver::new(client.clone()).with_packument_full_cache(cache.path().to_path_buf());
            let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
            scheduler.ensure_exact_optional_fetch(name, &version, None, force_refresh);
            let (_, result) = scheduler.join_next().await.unwrap().unwrap();
            if force_refresh {
                assert!(result.is_err());
            } else {
                let (_, packument, source, _) = result.unwrap();
                assert_eq!(source, FetchSource::Primer);
                assert!(packument.versions.contains_key(&version));
            }
            let retained = client
                .cached_resolution_packument(name, cache.path())
                .packument
                .unwrap();
            assert!(!retained.versions.contains_key(&version));
            assert_eq!(retained.versions.len(), primer.versions.len());
        }
    }

    #[tokio::test]
    async fn failed_exact_full_fetch_does_not_repeat_without_a_primer() {
        let (registry, requests, server) = serve_registry(b"not json".to_vec()).await;
        let cache = tempfile::tempdir().unwrap();
        let client = RegistryClient::from_config_with_policy(
            aube_registry::config::NpmConfig {
                registry,
                ..Default::default()
            },
            aube_registry::config::FetchPolicy {
                retries: 0,
                ..Default::default()
            },
        );
        let resolver =
            Resolver::new(Arc::new(client)).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch("shared", "1.0.0", None, false);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        assert!(result.is_err());
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn failed_exact_refresh_preserves_other_cached_versions_for_offline_use() {
        let (registry, requests, server) = serve_registry(b"not json".to_vec()).await;
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::from_config_with_policy(
            aube_registry::config::NpmConfig {
                registry: registry.clone(),
                ..Default::default()
            },
            aube_registry::config::FetchPolicy {
                retries: 0,
                ..Default::default()
            },
        ));
        let old = serde_json::from_value(serde_json::json!({"name":"shared","versions":{
            "1.0.0":{"name":"shared","version":"1.0.0"}
        }}))
        .unwrap();
        client.seed_full_packument_cache("shared", cache.path(), &old, None, None, true);
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        assert!(result.is_err());
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        let offline =
            RegistryClient::new(&registry).with_network_mode(aube_registry::NetworkMode::Offline);
        let resolver =
            Resolver::new(Arc::new(offline)).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch("shared", "1.0.0", None, false);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        let (_, packument, source, _) = result.unwrap();
        assert_eq!(source, FetchSource::Disk);
        assert!(packument.versions.contains_key("1.0.0"));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn live_missing_exact_version_keeps_the_full_cache_without_retrying() {
        let body = serde_json::json!({"name":"shared","versions":{
            "1.0.0":{"name":"shared","version":"1.0.0"}
        }});
        let (registry, requests, server) = serve_registry(serde_json::to_vec(&body).unwrap()).await;
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        let resolver =
            Resolver::new(client.clone()).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);
        scheduler.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
        let (_, result) = scheduler.join_next().await.unwrap().unwrap();
        assert!(
            matches!(result, Err(Error::Registry(_, message)) if message.contains("version 2.0.0 is missing"))
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert!(
            client
                .cached_resolution_packument("shared", cache.path())
                .packument
                .unwrap()
                .versions
                .contains_key("1.0.0")
        );
        server.abort();
    }

    #[tokio::test]
    async fn exact_optional_refreshes_an_incomplete_disk_entry() {
        let incomplete = serde_json::from_value(serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" }
            },
            "dist-tags": { "latest": "1.0.0" },
            "time": { "1.0.0": "2024-01-01T00:00:00.000Z" }
        }))
        .unwrap();
        let refreshed = serde_json::to_vec(&serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" },
                "2.0.0": { "name": "shared", "version": "2.0.0" }
            },
            "dist-tags": { "latest": "2.0.0" },
            "time": {
                "1.0.0": "2024-01-01T00:00:00.000Z",
                "2.0.0": "2024-02-01T00:00:00.000Z"
            }
        }))
        .unwrap();
        let (registry, requests, server) = serve_registry(refreshed).await;
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        client.seed_full_packument_cache("shared", cache.path(), &incomplete, None, None, true);
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_exact_optional_fetch("shared", "2.0.0", None, false);
        let Some(Ok((FetchKey::Exact(_, _), Ok((_, packument, source, _))))) =
            scheduler.join_next().await
        else {
            panic!("forced exact fallback refresh did not complete");
        };
        assert!(packument.versions.contains_key("2.0.0"));
        assert_eq!(source, FetchSource::Network);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    async fn panic_fetch(key: FetchKey) -> (FetchKey, Result<FetchResult, Error>) {
        let _ = key;
        panic!("simulated fetch panic");
    }

    #[tokio::test]
    async fn join_error_releases_the_active_fetch_key() {
        let resolver = Resolver::new(Arc::new(RegistryClient::new("http://127.0.0.1:0")));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), false);
        let key = FetchKey::Full("shared".to_string());
        scheduler.active_fetches.insert(key.clone());
        let handle = scheduler.in_flight.spawn(panic_fetch(key.clone()));
        scheduler.task_keys.insert(handle.id(), key.clone());

        assert!(matches!(scheduler.join_next().await, Some(Err(_))));
        assert!(!scheduler.active_fetches.contains(&key));
        assert!(scheduler.task_keys.is_empty());
    }

    #[test]
    fn active_exact_fetch_is_reported_by_package_name() {
        let resolver = Resolver::new(Arc::new(RegistryClient::new("http://127.0.0.1:0")));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), false);
        scheduler
            .active_fetches
            .insert(FetchKey::Exact("shared".to_string(), "1.0.0".to_string()));

        assert!(scheduler.has_active_exact_fetch("shared"));
        assert!(!scheduler.has_active_exact_fetch("other"));
    }
}
