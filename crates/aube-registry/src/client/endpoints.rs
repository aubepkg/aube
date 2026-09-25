use super::body::{check_body_cap, read_body_capped};
use super::cache::{now_secs, packument_full_cache_path, parse_cache_control_max_age};
use super::{
    AUDIT_BODY_CAP, PACKUMENT_FULL_ACCEPT, RegistryClient, check_dist_tag_status,
    dist_tag_root_url, dist_tag_url, parse_full_response, parse_full_response_seed,
};
use crate::{Error, NetworkMode};
use serde::Deserialize;
use std::borrow::Cow;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSearchResult {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct PackageSearchResponse {
    #[serde(default)]
    objects: Vec<PackageSearchObject>,
}

#[derive(Deserialize)]
struct PackageSearchObject {
    package: PackageSearchPackage,
}

#[derive(Deserialize)]
struct PackageSearchPackage {
    name: String,
    #[serde(default)]
    version: String,
    description: Option<String>,
}

impl RegistryClient {
    /// Search package names using npm's lightweight `/-/v1/search` endpoint.
    ///
    /// Scoped queries use their configured registry and scope-specific auth,
    /// so private package completion follows the same `.npmrc` routing as
    /// packument fetches.
    ///
    /// This intentionally bypasses the normal metadata retry loop: callers use
    /// it for interactive completion, where returning no candidates promptly is
    /// better than delaying the shell while retries back off.
    pub async fn search_packages(
        &self,
        query: &str,
        limit: usize,
        timeout: std::time::Duration,
    ) -> Result<Vec<PackageSearchResult>, Error> {
        let routing_name = if query.starts_with('@') && !query.contains('/') {
            Cow::Owned(format!("{query}/"))
        } else {
            Cow::Borrowed(query)
        };
        let registry_url = self.config.registry_for(&routing_name);
        let mut url = reqwest::Url::parse(&format!(
            "{}/-/v1/search",
            registry_url.trim_end_matches('/')
        ))
        .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error)))?;
        url.query_pairs_mut()
            .append_pair("text", query)
            .append_pair("size", &limit.clamp(1, 250).to_string());
        let response = self
            .authed_for_package(
                self.http_for_package(registry_url, &routing_name).get(url),
                registry_url,
                &routing_name,
            )
            .timeout(timeout)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;
        let bytes = read_body_capped(response, 2 << 20, "package search").await?;
        let body: PackageSearchResponse = serde_json::from_slice(&bytes)
            .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        Ok(body
            .objects
            .into_iter()
            .map(|entry| PackageSearchResult {
                name: entry.package.name,
                version: entry.package.version,
                description: entry.package.description,
            })
            .collect())
    }

    pub async fn fetch_advisories_bulk(
        &self,
        pkg_versions: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<serde_json::Value, Error> {
        // The bulk endpoint lives on the default registry; scoped registries
        // don't all implement it, so we always post to the top-level one.
        let registry_url = &self.config.registry;
        let url = format!(
            "{}/-/npm/v1/security/advisories/bulk",
            registry_url.trim_end_matches('/')
        );

        let body = serde_json::to_vec(pkg_versions)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

        let resp = self
            .authed(self.http_for(registry_url).post(&url), registry_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await?;

        // Some registries (Verdaccio, private mirrors) don't implement the
        // bulk advisory endpoint and return 404. Treat that as "no advisories"
        // — the alternative is making every air-gapped setup pass
        // `--ignore-registry-errors`, which is noisy.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }

        let resp = resp.error_for_status()?;
        check_body_cap(&resp, AUDIT_BODY_CAP, "bulk advisories")?;
        let json: serde_json::Value = resp.json().await?;
        Ok(json)
    }

    /// Fetch a single VersionMetadata via the per-version registry
    /// endpoint `{registry}/{name}/{version}`. Returns ~1-4 KiB JSON
    /// vs the full packument's 100 KiB-2 MiB. Use when caller knows
    /// the exact version, e.g. lockfile drift refetch with locked
    /// version pinned. Wins 200-1000 ms on lockfile CI installs that
    /// trigger re-resolve.
    pub async fn fetch_single_version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<crate::VersionMetadata, Error> {
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!(
                "version metadata for {name}@{version}"
            )));
        }
        let (packument_url, registry_url) = self.packument_url(name);
        let url = format!("{packument_url}/{version}");
        let resp = self
            .send_metadata_with_retry(&format!("version {name}@{version}"), || {
                self.authed_get_for_package(&url, registry_url, name)
                    .header("Accept", "application/json")
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("{name}@{version}")));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(
            &resp,
            self.fetch_policy.packument_max_bytes,
            "version-metadata",
        )?;
        parse_full_response(resp).await
    }

    /// Fetch one exact release plus compact publish-time/trust history from a
    /// single full-packument response. Historical dependency and distribution
    /// metadata is discarded during deserialization.
    pub async fn fetch_exact_version_packument(
        &self,
        name: &str,
        version: &str,
    ) -> Result<crate::ExactVersionPackument, Error> {
        self.fetch_exact_version_packument_response(name, version)
            .await
            .map(|(exact, _)| exact)
    }

    /// Cache an exact release and its complete publish-time/trust history.
    /// Entries are isolated from full packuments and keyed by registry, package,
    /// and version so a compact response cannot hide other releases.
    pub async fn fetch_exact_version_packument_cached(
        &self,
        name: &str,
        version: &str,
        cache_dir: &Path,
    ) -> Result<crate::ExactVersionPackument, Error> {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct CachedExact {
            fetched_at: u64,
            max_age_secs: Option<u64>,
            exact: crate::ExactVersionPackument,
        }
        let cache_path = packument_full_cache_path(
            &cache_dir.join("exact-v1"),
            name,
            self.config.registry_for(name),
        )
        .ok_or_else(|| Error::InvalidName(name.to_string()))?
        .with_file_name(name.replace('/', "%2F"))
        .join(format!(
            "{}.json",
            blake3::hash(version.as_bytes()).to_hex()
        ));
        let read_path = cache_path.clone();
        let cached = tokio::task::spawn_blocking(move || {
            std::fs::read(read_path)
                .ok()
                .and_then(|bytes| sonic_rs::from_slice::<CachedExact>(&bytes).ok())
        })
        .await
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        if let Some(cached) = cached
            && cached.exact.metadata.name == name
            && cached.exact.metadata.version == version
            && self.trust_cached_packument(cached.fetched_at, cached.max_age_secs)
        {
            return Ok(cached.exact);
        }
        let (exact, max_age_secs) = self
            .fetch_exact_version_packument_response(name, version)
            .await?;
        let cached = CachedExact {
            fetched_at: now_secs(),
            max_age_secs,
            exact,
        };
        tokio::task::spawn_blocking(move || {
            let written = sonic_rs::to_vec(&cached)
                .map_err(std::io::Error::other)
                .and_then(|bytes| aube_util::fs_atomic::atomic_write(&cache_path, &bytes));
            if let Err(error) = written {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                    %error,
                    "failed to cache exact package metadata"
                );
            }
            cached.exact
        })
        .await
        .map_err(|error| Error::Io(std::io::Error::other(error)))
    }

    async fn fetch_exact_version_packument_response(
        &self,
        name: &str,
        version: &str,
    ) -> Result<(crate::ExactVersionPackument, Option<u64>), Error> {
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("trust history for {name}")));
        }
        let (url, registry_url) = self.packument_url(name);
        let resp = self
            .send_metadata_with_retry(&format!("trust history {name}"), || {
                self.authed_get_for_package(&url, registry_url, name)
                    .header("Accept", PACKUMENT_FULL_ACCEPT)
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(
            &resp,
            self.fetch_policy.packument_max_bytes,
            "packument-trust-history",
        )?;
        let max_age_secs = parse_cache_control_max_age(&resp);
        let exact =
            parse_full_response_seed(resp, crate::ExactVersionPackumentSeed { version }).await?;
        Ok((exact, max_age_secs))
    }

    /// Fetch the *full* (non-corgi) packument as raw JSON, bypassing the
    /// on-disk cache entirely. Used by mutating commands like `deprecate`
    /// that need a fresh read-modify-write against the authoritative copy
    /// on the registry — a stale cached document would roll back other
    /// publishers' changes on the subsequent PUT.
    pub async fn fetch_packument_json_fresh(&self, name: &str) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);
        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
                self.authed_get_for_package(&url, registry_url, name)
                    .header("Accept", PACKUMENT_FULL_ACCEPT)
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(&resp, self.fetch_policy.packument_max_bytes, "packument")?;
        let value: serde_json::Value = resp.json().await?;
        Ok(value)
    }

    /// PUT a full packument back to the registry. Used by `deprecate` /
    /// `undeprecate`. Honors `--otp` via the `npm-otp` header.
    ///
    /// Returns the registry's raw response body as `serde_json::Value`
    /// (npm responds with `{ok: true, id, rev}` on success). On HTTP
    /// failure the body is included in the error so 401/403/409 messages
    /// make it to the user.
    pub async fn put_packument(
        &self,
        name: &str,
        body: &serde_json::Value,
        otp: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);

        let mut req = self.authed_for_package(
            self.http_for_package(registry_url, name)
                .put(&url)
                .header("Content-Type", "application/json")
                .json(body),
            registry_url,
            name,
        );
        if let Some(code) = otp {
            req = req.header("npm-otp", code);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryWrite {
                status: status.as_u16(),
                body,
            });
        }
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok(value)
    }

    /// Drop any on-disk *full* packument cache entry for `name`, if one
    /// exists. Call this after a successful mutating PUT (deprecate,
    /// dist-tag, ...) so subsequent `aube view` calls don't serve the
    /// pre-mutation document for the remaining TTL window. Missing files
    /// and I/O errors are swallowed — the cache is advisory, not load
    /// bearing.
    pub fn invalidate_full_packument_cache(&self, name: &str, cache_dir: &Path) {
        let registry_url = self.config.registry_for(name).to_string();
        if let Some(path) = packument_full_cache_path(cache_dir, name, &registry_url) {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Fetch the authoritative dist-tag map for a package from the
    /// registry's `/-/package/<pkg>/dist-tags` endpoint. This is the
    /// same endpoint `npm dist-tag ls` calls. A GET against this
    /// endpoint doesn't require auth for public packages, but we still
    /// attach the user's token so private packages Just Work.
    pub async fn fetch_dist_tags(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_root_url(registry_url, name);
        let resp = self
            .send_metadata_with_retry(&format!("dist-tags {name}"), || {
                self.authed_get_for_package(&url, registry_url, name)
            })
            .await?;
        check_dist_tag_status(&resp, name)?;
        let map: std::collections::BTreeMap<String, String> =
            resp.error_for_status()?.json().await?;
        Ok(map)
    }

    /// Create or update a dist-tag for a package. The npm registry
    /// expects a PUT with a JSON-string body — e.g. `"1.2.3"`, *with*
    /// the quotes — and Content-Type: application/json. Requires auth.
    pub async fn put_dist_tag(
        &self,
        name: &str,
        tag: &str,
        version: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);

        // serde_json is already a workspace dep and used elsewhere in
        // this file; hand-serializing would miss control-character
        // escapes and other edge cases. The output is always a JSON
        // string literal like `"1.2.3"`.
        let body = serde_json::to_string(version).map_err(std::io::Error::other)?;

        let mut req = self
            .http_for_package(registry_url, name)
            .put(&url)
            .header("Content-Type", "application/json")
            .body(body);
        if self.config.is_public_npmjs(name) {
            req = req.header("npm-auth-type", "web");
        }
        let req = if let Some(code) = otp {
            req.header("npm-otp", code)
        } else {
            req
        };
        let resp = self
            .authed_for_package(req, registry_url, name)
            .send()
            .await?;
        check_dist_tag_status(&resp, name)?;
        resp.error_for_status()?;
        Ok(())
    }

    /// Remove a dist-tag from a package. Registry DELETE against
    /// `/-/package/<pkg>/dist-tags/<tag>`. Requires auth.
    pub async fn delete_dist_tag(
        &self,
        name: &str,
        tag: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);
        let mut req = self.http_for_package(registry_url, name).delete(&url);
        if self.config.is_public_npmjs(name) {
            req = req.header("npm-auth-type", "web");
        }
        let req = if let Some(code) = otp {
            req.header("npm-otp", code)
        } else {
            req
        };
        let resp = self
            .authed_for_package(req, registry_url, name)
            .send()
            .await?;
        // 404 here is ambiguous: package doesn't exist vs tag doesn't
        // exist on this package. Surface the `name@tag` form so the
        // caller can render it either way.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("{name}@{tag}")));
        }
        if matches!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(Error::Unauthorized);
        }
        resp.error_for_status()?;
        Ok(())
    }

    /// Construct the tarball URL for a package from the registry.
    /// Format: {registry}/{name}/-/{unscoped_name}-{version}.tgz
    pub fn tarball_url(&self, name: &str, version: &str) -> String {
        let registry_url = self.registry_url_for(name);
        let registry = registry_url.trim_end_matches('/');
        let unscoped = if let Some(rest) = name.strip_prefix('@') {
            // @scope/pkg -> pkg
            rest.split('/').nth(1).unwrap_or(rest)
        } else {
            name
        };
        format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn package_search_uses_registry_endpoint_and_parses_descriptions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/v1/search"))
            .and(query_param("text", "rea"))
            .and(query_param("size", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "objects": [{
                    "package": {
                        "name": "react",
                        "version": "19.1.0",
                        "description": "React is a JavaScript library"
                    }
                }]
            })))
            .mount(&server)
            .await;

        let client = RegistryClient::new(&server.uri());
        let results = client
            .search_packages("rea", 5, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            results,
            vec![PackageSearchResult {
                name: "react".to_string(),
                version: "19.1.0".to_string(),
                description: Some("React is a JavaScript library".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn scoped_package_search_uses_its_configured_registry() {
        let default_server = MockServer::start().await;
        let scoped_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/v1/search"))
            .and(query_param("text", "@acme/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "objects": [{
                    "package": {"name": "@acme/tool", "version": "2.0.0"}
                }]
            })))
            .expect(1)
            .mount(&scoped_server)
            .await;

        let mut config = crate::config::NpmConfig {
            registry: default_server.uri(),
            ..Default::default()
        };
        config
            .scoped_registries
            .insert("@acme".to_string(), scoped_server.uri());
        let client = RegistryClient::from_config(config);
        let results = client
            .search_packages("@acme/tool", 5, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(results[0].name, "@acme/tool");
    }

    #[tokio::test]
    async fn incomplete_scope_search_uses_its_configured_registry() {
        let default_server = MockServer::start().await;
        let scoped_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/v1/search"))
            .and(query_param("text", "@acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "objects": [{
                    "package": {"name": "@acme/tool", "version": "2.0.0"}
                }]
            })))
            .expect(1)
            .mount(&scoped_server)
            .await;

        let mut config = crate::config::NpmConfig {
            registry: default_server.uri(),
            ..Default::default()
        };
        config
            .scoped_registries
            .insert("@acme".to_string(), scoped_server.uri());
        let client = RegistryClient::from_config(config);
        let results = client
            .search_packages("@acme", 5, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(results[0].name, "@acme/tool");
    }
}

#[cfg(test)]
mod exact_cache_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn document() -> serde_json::Value {
        serde_json::json!({
            "name": "demo",
            "versions": {
                "1.0.0": { "name": "demo", "version": "1.0.0", "dependencies": { "child": "^1" } },
                "2.0.0": { "name": "demo", "version": "2.0.0", "_npmUser": { "name": "publisher" } }
            },
            "time": { "1.0.0": "2024-01-01T00:00:00Z", "2.0.0": "2024-02-01T00:00:00Z" }
        })
    }

    #[tokio::test]
    async fn exact_cache_preserves_selected_metadata_and_complete_trust_history() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document()))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new(&server.uri());
        let first = client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
        assert_eq!(first.metadata.dependencies["child"], "^1");
        assert_eq!(first.history.versions.len(), 1);
        assert!(first.history.versions["2.0.0"].npm_user.is_some());
        assert_eq!(first.history.time.len(), 2);
        let second = client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        assert!(
            client
                .cached_full_packument_lookup("demo", dir.path())
                .packument
                .is_none()
        );
    }

    #[tokio::test]
    async fn exact_cache_is_partitioned_by_registry_and_version() {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        for server in [&first, &second] {
            Mock::given(method("GET"))
                .and(path("/demo"))
                .respond_with(ResponseTemplate::new(200).set_body_json(document()))
                .expect(2)
                .mount(server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        for server in [&first, &second] {
            let client = RegistryClient::new(&server.uri());
            for version in ["1.0.0", "2.0.0"] {
                for _ in 0..2 {
                    let exact = client
                        .fetch_exact_version_packument_cached("demo", version, dir.path())
                        .await
                        .unwrap();
                    assert_eq!(exact.metadata.version, version);
                }
            }
        }
    }

    #[tokio::test]
    async fn exact_cache_respects_revalidation_headers_and_offline_mode() {
        for header in ["max-age=0", "no-cache", "no-store"] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/demo"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("cache-control", header)
                        .set_body_json(document()),
                )
                .expect(2)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut client = RegistryClient::new(&server.uri());
            for _ in 0..2 {
                client
                    .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
                    .await
                    .unwrap();
            }
            client.network_mode = NetworkMode::Offline;
            client
                .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
                .await
                .unwrap();
            assert!(matches!(
                client
                    .fetch_exact_version_packument_cached("demo", "2.0.0", dir.path())
                    .await,
                Err(Error::Offline(_))
            ));
        }
    }

    #[tokio::test]
    async fn exact_cache_keeps_ambiguous_scoped_names_separate() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new(&server.uri());
        for name in ["@scope__x/y", "@scope/x__y"] {
            let mut body = document();
            body["name"] = name.into();
            body["versions"]["1.0.0"]["name"] = name.into();
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let exact = client
                .fetch_exact_version_packument_cached(name, "1.0.0", dir.path())
                .await
                .unwrap();
            assert_eq!(exact.metadata.name, name);
            server.verify().await;
            server.reset().await;
        }
        for name in ["@scope__x/y", "@scope/x__y"] {
            let exact = client
                .fetch_exact_version_packument_cached(name, "1.0.0", dir.path())
                .await
                .unwrap();
            assert_eq!(exact.metadata.name, name);
        }
    }

    #[tokio::test]
    async fn exact_cache_rejects_mismatched_identity() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document()))
            .expect(3)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new(&server.uri());
        let file = packument_full_cache_path(
            &dir.path().join("exact-v1"),
            "demo",
            client.config.registry_for("demo"),
        )
        .unwrap()
        .with_file_name("demo")
        .join(format!("{}.json", blake3::hash(b"1.0.0").to_hex()));
        client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
        for (key, wrong) in [("name", "other"), ("version", "2.0.0")] {
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            value["exact"]["metadata"][key] = wrong.into();
            std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            let exact = client
                .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
                .await
                .unwrap();
            assert_eq!(exact.metadata.name, "demo");
            assert_eq!(exact.metadata.version, "1.0.0");
        }
    }

    #[tokio::test]
    async fn exact_cache_recovers_corruption_and_expired_entries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document()))
            .expect(3)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new(&server.uri());
        let file = packument_full_cache_path(
            &dir.path().join("exact-v1"),
            "demo",
            client.config.registry_for("demo"),
        )
        .unwrap()
        .with_extension("")
        .join(format!("{}.json", blake3::hash(b"1.0.0").to_hex()));
        client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
        let mut expired: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        expired["fetched_at"] = 0.into();
        std::fs::write(&file, serde_json::to_vec(&expired).unwrap()).unwrap();
        client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
        std::fs::write(&file, b"corrupt").unwrap();
        client
            .fetch_exact_version_packument_cached("demo", "1.0.0", dir.path())
            .await
            .unwrap();
    }
}
