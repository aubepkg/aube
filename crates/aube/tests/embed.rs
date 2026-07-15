use aube::embed::{Host, InstallControl, InstallOptions};

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

fn initialize_test_host() {
    aube::embed::initialize(
        &TEST_HOST,
        vec![("minimumReleaseAge".to_string(), "0".to_string())],
    );
}

#[tokio::test]
async fn facade_initializes_host_and_runs_install() {
    initialize_test_host();
    assert_eq!(aube::embed::host().name, "testhost");

    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("package.json"), "{}\n").unwrap();

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install(options).await.unwrap();

    assert!(project.path().join("testhost-lock.yaml").is_file());
}

#[tokio::test]
async fn facade_adds_local_package_to_workspace_member() {
    initialize_test_host();
    let workspace = tempfile::tempdir().unwrap();
    let app = workspace.path().join("packages/app");
    let library = workspace.path().join("packages/library");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{"private":true}
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"app"}
"#,
    )
    .unwrap();
    std::fs::write(
        library.join("package.json"),
        r#"{"name":"library","version":"1.0.0"}
"#,
    )
    .unwrap();

    aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            ignore_scripts: true,
            offline: true,
            control: InstallControl::silent(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let manifest = std::fs::read_to_string(app.join("package.json")).unwrap();
    assert!(manifest.contains(r#""library": "workspace:*""#));
    assert!(workspace.path().join("testhost-lock.yaml").is_file());
    assert!(!app.join("testhost-lock.yaml").exists());
}

#[test]
fn error_code_reads_structured_diagnostic_code() {
    let error = miette::miette!(code = "ERR_AUBE_TEST", "test failure");
    assert_eq!(
        aube::embed::error_code(&error).as_deref(),
        Some("ERR_AUBE_TEST")
    );
}
