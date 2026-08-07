//! Integration test for explicit setting detection under an embedder that
//! disables tool-branded environment aliases.

use aube_settings::{ResolveCtx, has_explicit_value};
use aube_util::Embedder;
use std::collections::BTreeMap;

static NUBLIKE: Embedder = Embedder {
    name: "nublike",
    display_name: "nublike",
    vendor: None,
    version: "1.0.0",
    user_agent: "nublike/1.0.0",
    self_names: &["nublike"],
    compatible_names: &["pnpm"],
    lockfile_basename: "lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: Some("NUB"),
    cache_namespace: "nublike",
    data_namespace: "nublike",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
};

#[test]
fn disabled_branded_env_alias_is_not_explicit() {
    aube_util::set_embedder(&NUBLIKE);
    let workspace = BTreeMap::new();
    let branded = vec![("AUBE_MINIMUM_RELEASE_AGE".to_string(), "999".to_string())];
    let ctx = ResolveCtx {
        managed_aube_config: &[],
        project_aube_config: &[],
        project_npmrc: &[],
        user_aube_config: &[],
        user_npmrc: &[],
        workspace_yaml: &workspace,
        env: &branded,
        cli: &[],
        embedder_defaults: &[],
    };

    assert!(!has_explicit_value("minimumReleaseAge", &ctx));

    let neutral = vec![(
        "NPM_CONFIG_MINIMUM_RELEASE_AGE".to_string(),
        "999".to_string(),
    )];
    let ctx = ResolveCtx {
        env: &neutral,
        ..ctx
    };
    assert!(has_explicit_value("minimumReleaseAge", &ctx));
}
