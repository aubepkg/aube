//! `aube::embed::dlx` with a relative `project_dir`. Kept in its own test
//! binary because it changes the process working directory, which the
//! tests in `embed.rs` must not observe.

use aube::embed::Host;

static TEST_HOST: Host = Host {
    name: "testhost",
    display_name: "Test Host",
    vendor: None,
    version: "1.0.0",
    user_agent: "testhost/1.0.0",
    self_names: &["testhost"],
    compatible_names: &["pnpm"],
    lockfile_basename: "testhost-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "testhost",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "testhost",
    data_namespace: "testhost",
    canonical_lockfile_always_wins: true,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
};

/// Switches the process cwd and restores it on drop, even while a panic
/// unwinds. Declared after the temp dir so it drops first: Windows can't
/// remove a directory that is still some process's cwd.
struct CwdGuard(std::path::PathBuf);

impl CwdGuard {
    fn enter(dir: &std::path::Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self(original)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[tokio::test]
async fn dlx_resolves_a_relative_local_spec_from_a_relative_project_dir() {
    aube::embed::initialize(
        &TEST_HOST,
        vec![("minimumReleaseAge".to_string(), "0".to_string())],
    );
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("app/tool");
    std::fs::create_dir_all(&tool).unwrap();
    std::fs::write(root.path().join("app/package.json"), "{\"name\":\"app\"}\n").unwrap();
    std::fs::write(
        tool.join("package.json"),
        r#"{"name":"tool","version":"1.0.0","bin":{"tool":"cli.js"}}
"#,
    )
    .unwrap();
    std::fs::write(
        tool.join("cli.js"),
        "#!/usr/bin/env node\nprocess.exit(7);\n",
    )
    .unwrap();
    let _cwd = CwdGuard::enter(root.path());

    // `file:./tool` is relative to `app`, which is itself relative to the
    // process cwd; dlx installs from a scratch dir elsewhere.
    let code = aube::embed::dlx(
        std::path::Path::new("app"),
        vec!["tool".to_string()],
        vec!["file:./tool".to_string()],
        None,
    )
    .await
    .unwrap();

    assert_eq!(code, Some(7));
}
