use aube_registry::NetworkMode;
use aube_registry::client::RegistryClient;
use aube_registry::config::{FetchPolicy, NpmConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn schema_errors_preserve_full_json_without_body_decode_retries() {
    let server = MockServer::start().await;
    let raw = serde_json::json!({
        "name": "demo", "description": "still available to view",
        "versions": {"1.0.0": {"name":"demo", "version":"1.0.0", "dist":{"tarball":42}}}
    });
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&raw))
        .expect(1)
        .mount(&server)
        .await;
    let client = RegistryClient::from_config_with_policy(
        NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        },
        FetchPolicy {
            retries: 2,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..Default::default()
        },
    );
    let cache = tempfile::tempdir().unwrap();
    assert!(
        client
            .fetch_packument_with_time_cached("demo", cache.path())
            .await
            .is_err()
    );
    // The full cache is shared with view; schema errors from an install
    // consumer do not make the original, valid JSON unusable to that reader.
    let offline = client.with_network_mode(NetworkMode::Offline);
    assert_eq!(
        offline
            .fetch_packument_full_cached("demo", cache.path())
            .await
            .unwrap(),
        raw
    );
    assert!(
        offline
            .fetch_packument_with_time_cached("demo", cache.path())
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
