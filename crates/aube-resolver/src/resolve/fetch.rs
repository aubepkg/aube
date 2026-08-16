use crate::{Error, FxHashSet, Resolver};
use aube_registry::client::RegistryClient;
use aube_registry::{Packument, VersionTrustMetadata};
use aube_util::adaptive::AdaptiveLimit;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinSet;

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
pub(super) type FetchResult = (String, Packument, bool, Option<TrustHistory>);
pub(super) type FetchOutcome =
    Option<Result<(FetchKey, Result<FetchResult, Error>), tokio::task::JoinError>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum FetchKey {
    Full(String),
    Exact(String, String),
}

impl FetchScheduler {
    pub(super) fn new(resolver: &Resolver, sem: Arc<AdaptiveLimit>, needs_time: bool) -> Self {
        Self {
            in_flight: JoinSet::new(),
            active_fetches: FxHashSet::default(),
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
        self.in_flight.spawn(async move {
            let result = fetch_one_packument(inputs).await;
            (key, result)
        });
    }

    /// Fetch an exact optional dependency without retaining every historical
    /// version's dependency metadata. The full document is decoded into its
    /// publish-time/trust subset so policy checks keep their semantics.
    pub(super) fn ensure_exact_optional_fetch(
        &mut self,
        name: &str,
        version: &str,
        published_by: Option<&str>,
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
            force_refresh: false,
        };
        let version = version.to_string();
        self.in_flight.spawn(async move {
            let result = fetch_exact_optional_packument(inputs, version).await;
            (key, result)
        });
    }

    /// Wait for the next in-flight fetch to complete.
    pub(super) async fn join_next(&mut self) -> FetchOutcome {
        let outcome = self.in_flight.join_next().await;
        if let Some(Ok((key, _))) = &outcome {
            self.active_fetches.remove(key);
        }
        outcome
    }

    pub(super) fn note_primer_seeded(&mut self, name: String) {
        self.primer_seeded_names.insert(name);
    }

    /// Returns true if `name` was marked as primer-seeded, removing it.
    pub(super) fn take_primer_seeded(&mut self, name: &str) -> bool {
        self.primer_seeded_names.remove(name)
    }

    pub(super) async fn drain(&mut self) {
        while self.in_flight.join_next().await.is_some() {}
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

/// Body of the per-packument fetch task spawned by the resolver.
///
/// Returns `(name, packument, from_primer)` — `from_primer` is true
/// when the result came from the bundled metadata primer (only its
/// capped slice of high-traffic histories), so the caller knows a
/// range miss must trigger a live registry refetch before reporting
/// `ERR_AUBE_NO_MATCHING_VERSION`.
async fn fetch_one_packument(inputs: FetchInputs) -> Result<FetchResult, Error> {
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        primer_covers_cutoff,
        force_metadata_primer,
        sem,
        needs_time,
        force_refresh,
    } = inputs;
    let _diag_span =
        aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "packument_fetch")
            .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)));
    let _diag_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Pack);
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
    if force_refresh
        && needs_time
        && let Some(dir) = full_cache_dir.as_ref()
    {
        client.invalidate_full_packument_cache(&name, dir);
    }
    let mut cached = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => client.cached_full_packument_lookup(&name, dir),
            None => Default::default(),
        }
    } else if let Some(ref dir) = cache_dir {
        client.cached_packument_lookup(&name, dir)
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
        permit.record_cancelled();
        return Ok((name, packument, false, None));
    }
    let use_metadata_primer = (force_metadata_primer
        || client.uses_default_npm_registry_for(&name))
        && primer_covers_cutoff;
    if use_metadata_primer
        && !cached.stale
        && let Some(seed) = crate::primer::get(&name)
    {
        let mut packument = seed.packument();
        if force_metadata_primer {
            for version in packument.versions.values_mut() {
                let tarball = client.tarball_url(&version.name, &version.version);
                version.dist = version.dist.take().map(|mut dist| {
                    dist.tarball = tarball;
                    dist
                });
            }
        }
        if needs_time {
            if let Some(dir) = full_cache_dir.as_ref() {
                client.seed_full_packument_cache(
                    &name,
                    dir,
                    &packument,
                    seed.etag.as_deref(),
                    seed.last_modified.as_deref(),
                    false,
                );
            }
        } else if let Some(dir) = cache_dir.as_ref() {
            client.seed_packument_cache(
                &name,
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
                    aube_util::diag::jstr(&name),
                    packument.versions.len()
                )
            },
        );
        permit.record_cancelled();
        return Ok((name, packument, true, None));
    }
    let fetch_outcome = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => {
                client
                    .fetch_packument_with_time_cached_after_lookup(&name, dir, cached)
                    .await
            }
            None => client.fetch_packument(&name).await,
        }
    } else if let Some(ref dir) = cache_dir {
        client
            .fetch_packument_cached_after_lookup(&name, dir, cached)
            .await
    } else {
        client.fetch_packument(&name).await
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
    Ok((name, packument, false, None))
}

async fn fetch_exact_optional_packument(
    inputs: FetchInputs,
    version: String,
) -> Result<FetchResult, Error> {
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
                },
                false,
                Some(exact.history.versions),
            ))
        }
        Err(err) => {
            tracing::debug!(
                "compact exact metadata fetch failed for optional dep {name}@{version}; falling back to full packument: {err}"
            );
            permit.record_cancelled();
            fetch_one_packument(inputs).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_packument(versions: &[&str]) -> Packument {
        Packument {
            name: "shared".to_string(),
            modified: None,
            versions: versions
                .iter()
                .map(|version| {
                    (
                        (*version).to_string(),
                        serde_json::from_value(serde_json::json!({
                            "name": "shared",
                            "version": version,
                        }))
                        .unwrap(),
                    )
                })
                .collect(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn completed_full_fetch_can_force_refresh_a_fresh_disk_entry() {
        let stale = test_packument(&["1.0.0"]);
        let fresh = test_packument(&["1.0.0", "2.0.0"]);
        let body = serde_json::to_vec(&fresh).unwrap();
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
                    requests.fetch_add(1, Ordering::Relaxed);
                    let body = body.clone();
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

        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        client.seed_full_packument_cache("shared", cache.path(), &stale, None, None, true);
        let resolver = Resolver::new(Arc::clone(&client))
            .with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_fetch("shared", None, false);
        let Some(Ok((FetchKey::Full(_), Ok((_, first, _, _))))) = scheduler.join_next().await
        else {
            panic!("cached full fetch did not complete");
        };
        assert_eq!(first.versions.len(), 1);
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        scheduler.ensure_fetch("shared", None, true);
        let Some(Ok((FetchKey::Full(_), Ok((_, refreshed, _, _))))) = scheduler.join_next().await
        else {
            panic!("forced full refresh did not restart");
        };
        assert_eq!(refreshed.versions.len(), 2);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
