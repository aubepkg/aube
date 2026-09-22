use aube::embed::{Host, InstallControl, InstallOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

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
static INIT: Once = Once::new();

fn initialize_test_host() {
    INIT.call_once(|| {
        aube::embed::initialize(
            &TEST_HOST,
            vec![("minimumReleaseAge".to_string(), "0".to_string())],
        );
    });
}

fn workspace_fixture() -> (tempfile::TempDir, PathBuf) {
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
    (workspace, app)
}

fn seed_cached_registry_package(cache_dir: &std::path::Path, store_dir: &std::path::Path) {
    let packument_cache_dir = cache_dir.join("packuments-v1");
    let full_packument_cache_dir = cache_dir.join("packuments-full-v1");
    std::fs::create_dir_all(&packument_cache_dir).unwrap();
    std::fs::create_dir_all(&full_packument_cache_dir).unwrap();
    let package = tempfile::tempdir().unwrap();
    std::fs::write(
        package.path().join("package.json"),
        r#"{"name":"cached-only","version":"1.0.0","bin":{"cached-only":"cli.js"}}
"#,
    )
    .unwrap();

    std::fs::write(
        package.path().join("cli.js"),
        "#!/usr/bin/env node\nconsole.log(process.execPath);\n",
    )
    .unwrap();

    let store = aube_store::Store::with_dirs(store_dir.join("v1/files"), cache_dir.to_path_buf());
    let index = store.import_directory(package.path()).unwrap();
    store
        .save_index("cached-only", "1.0.0", None, &index)
        .unwrap();

    let packument: aube_registry::Packument = serde_json::from_value(serde_json::json!({
        "name": "cached-only",
        "dist-tags": { "latest": "1.0.0" },
        "versions": {
            "1.0.0": {
                "name": "cached-only",
                "version": "1.0.0",
                "bin": {"cached-only": "cli.js"},
                "dist": {
                    "tarball": "https://registry.npmjs.org/cached-only/-/cached-only-1.0.0.tgz"
                }
            }
        }
    }))
    .unwrap();
    let client = aube_registry::client::RegistryClient::new("https://registry.npmjs.org/");
    client.seed_packument_cache(
        "cached-only",
        &packument_cache_dir,
        &packument,
        None,
        None,
        true,
    );
    client.seed_full_packument_cache(
        "cached-only",
        &full_packument_cache_dir,
        &packument,
        None,
        None,
        true,
    );
    assert!(
        client
            .cached_packument_lookup("cached-only", &packument_cache_dir)
            .packument
            .is_some()
    );
}

fn cached_package_materialization(project: &std::path::Path) -> PathBuf {
    project.join("node_modules/cached-only/package.json")
}

fn assert_cached_package_is_project_local(
    importer: &std::path::Path,
    install_root: &std::path::Path,
) {
    let package = std::fs::canonicalize(cached_package_materialization(importer)).unwrap();
    let virtual_store = std::fs::canonicalize(install_root.join("node_modules/.testhost")).unwrap();
    assert!(package.starts_with(virtual_store));
}

struct CancelOnOutput(Mutex<Option<InstallControl>>);

impl aube::embed::InstallReporter for CancelOnOutput {
    fn report(&self, event: aube::embed::InstallEvent) {
        if matches!(event, aube::embed::InstallEvent::Output { .. })
            && let Some(control) = self.0.lock().unwrap().take()
        {
            control.cancel();
        }
    }
}

#[derive(Default)]
struct RecordingReporter(Mutex<Vec<aube::embed::InstallEvent>>);

impl aube::embed::InstallReporter for RecordingReporter {
    fn report(&self, event: aube::embed::InstallEvent) {
        self.0.lock().unwrap().push(event);
    }
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
async fn facade_install_accepts_host_storage_overrides() {
    initialize_test_host();
    let project = tempfile::tempdir().unwrap();
    let host_cache = project.path().join("host-cache");
    let host_store = project.path().join("host-store");
    seed_cached_registry_package(&host_cache, &host_store);
    std::fs::write(
        project.path().join("package.json"),
        r#"{"dependencies":{"cached-only":"1.0.0"}}
"#,
    )
    .unwrap();

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install_with_overrides(
        options,
        aube::embed::EmbedderInstallOverrides {
            use_global_virtual_store: Some(false),
            cache_dir: Some(host_cache.clone()),
            store_dir: Some(host_store.clone()),
        },
    )
    .await
    .unwrap();

    assert!(host_store.join("v1/files").is_dir());
    assert_cached_package_is_project_local(project.path(), project.path());
    assert!(!host_cache.join("virtual-store").exists());

    let replacement_store = project.path().join("replacement-store");
    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    let error = aube::embed::install_with_overrides(
        options,
        aube::embed::EmbedderInstallOverrides {
            use_global_virtual_store: Some(false),
            cache_dir: Some(host_cache),
            store_dir: Some(replacement_store),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("offline"));
}

#[cfg(unix)]
#[tokio::test]
async fn facade_warm_install_registers_the_host_virtual_store() {
    initialize_test_host();
    let project = tempfile::tempdir().unwrap();
    let host_cache = project.path().join("host-cache");
    let host_store = project.path().join("host-store");
    seed_cached_registry_package(&host_cache, &host_store);
    std::fs::write(
        project.path().join("package.json"),
        r#"{"dependencies":{"cached-only":"1.0.0"}}
"#,
    )
    .unwrap();
    let overrides = aube::embed::EmbedderInstallOverrides {
        use_global_virtual_store: Some(true),
        cache_dir: Some(host_cache.clone()),
        store_dir: Some(host_store),
    };

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install_with_overrides(options, overrides.clone())
        .await
        .unwrap();

    let projects_dir = host_cache.join("virtual-store/v1/.projects");
    std::fs::remove_dir_all(&projects_dir).unwrap();

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install_with_overrides(options, overrides)
        .await
        .unwrap();

    assert!(projects_dir.is_dir());
    assert!(std::fs::read_dir(projects_dir).unwrap().next().is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn facade_install_preserves_non_utf8_storage_paths() {
    use std::os::unix::ffi::OsStringExt;

    initialize_test_host();
    let project = tempfile::tempdir().unwrap();
    let host_cache = project
        .path()
        .join(std::ffi::OsString::from_vec(b"host-cache-\xff".to_vec()));
    let host_store = project
        .path()
        .join(std::ffi::OsString::from_vec(b"host-store-\xff".to_vec()));
    seed_cached_registry_package(&host_cache, &host_store);
    std::fs::write(
        project.path().join("package.json"),
        r#"{"dependencies":{"cached-only":"1.0.0"}}
"#,
    )
    .unwrap();

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install_with_overrides(
        options,
        aube::embed::EmbedderInstallOverrides {
            use_global_virtual_store: Some(false),
            cache_dir: Some(host_cache.clone()),
            store_dir: Some(host_store.clone()),
        },
    )
    .await
    .unwrap();

    assert!(host_store.join("v1/files").is_dir());
    assert!(host_cache.join("packuments-v1").is_dir());
    assert_cached_package_is_project_local(project.path(), project.path());
}

#[tokio::test]
async fn facade_adds_local_package_to_workspace_member() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();

    aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            save_dev: true,
            ignore_scripts: true,
            offline: true,
            control: InstallControl::silent(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let manifest = std::fs::read_to_string(app.join("package.json")).unwrap();
    assert!(manifest.contains(r#""devDependencies""#));
    assert!(manifest.contains(r#""library": "workspace:*""#));
    assert!(workspace.path().join("testhost-lock.yaml").is_file());
    assert!(!app.join("testhost-lock.yaml").exists());
}

#[tokio::test]
async fn facade_add_honors_host_storage_and_materialization_overrides() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();
    let host_cache = workspace.path().join("host-cache");
    let host_store = workspace.path().join("host-store");
    seed_cached_registry_package(&host_cache, &host_store);

    aube::embed::add_with_overrides(
        &app,
        &["cached-only".to_string()],
        aube::embed::AddToProjectOptions {
            ignore_scripts: true,
            offline: true,
            control: InstallControl::silent(),
            ..Default::default()
        },
        aube::embed::EmbedderInstallOverrides {
            use_global_virtual_store: Some(false),
            cache_dir: Some(host_cache.clone()),
            store_dir: Some(host_store.clone()),
        },
    )
    .await
    .unwrap();

    assert!(host_store.join("v1/files").is_dir());
    assert_cached_package_is_project_local(&app, workspace.path());
    assert!(!host_cache.join("virtual-store").exists());
}

#[tokio::test]
async fn facade_add_runs_root_dev_preinstall() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{
  "private": true,
  "scripts": {
    "pnpm:devPreinstall": "printf ran > embedded-dev-preinstall.marker"
  }
}
"#,
    )
    .unwrap();

    aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            offline: true,
            control: InstallControl::silent(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(
        workspace
            .path()
            .join("embedded-dev-preinstall.marker")
            .is_file()
    );
}

#[tokio::test]
async fn facade_routes_lifecycle_output_to_install_events() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();
    let manifest = serde_json::json!({
        "private": true,
        "scripts": {
            "pnpm:devPreinstall":
                "node -e \"process.stdout.write('lifecycle-stdout');process.stderr.write('lifecycle-stderr')\""
        }
    });
    std::fs::write(
        workspace.path().join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let reporter = Arc::new(RecordingReporter::default());

    aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            offline: true,
            control: InstallControl::events(reporter.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let events = reporter.0.lock().unwrap();
    for message in ["lifecycle-stdout", "lifecycle-stderr"] {
        assert!(events.iter().any(|event| matches!(
            event,
            aube::embed::InstallEvent::Output {
                level: aube::embed::InstallOutputLevel::Info,
                code: Some(code),
                message: event_message,
            } if code == aube::embed::INSTALL_OUTPUT_CODE_LIFECYCLE_SCRIPT
                && event_message == message
        )));
    }
}

#[tokio::test]
async fn cancelled_manifest_mutation_is_rolled_back() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();
    let original_manifest = std::fs::read(app.join("package.json")).unwrap();
    let reporter = Arc::new(CancelOnOutput(Mutex::new(None)));
    let control = InstallControl::events(reporter.clone());
    *reporter.0.lock().unwrap() = Some(control.clone());

    let error = aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            offline: true,
            control,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert_eq!(
        aube::embed::error_code(&error).as_deref(),
        Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED)
    );
    assert_eq!(
        std::fs::read(app.join("package.json")).unwrap(),
        original_manifest
    );
    assert!(!workspace.path().join("testhost-lock.yaml").exists());
}

#[test]
fn error_code_reads_structured_diagnostic_code() {
    let error = miette::miette!(code = "ERR_AUBE_TEST", "test failure");
    assert_eq!(
        aube::embed::error_code(&error).as_deref(),
        Some("ERR_AUBE_TEST")
    );
}

#[test]
fn facade_discovers_confined_workspace_packages() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{"workspaces":["{apps,packages}/*"]}"#,
    )
    .unwrap();
    for package in ["apps/web", "packages/lib"] {
        let directory = workspace.path().join(package);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("package.json"), "{}").unwrap();
    }

    assert!(aube::embed::is_workspace_project_root(workspace.path()));
    let packages = aube::embed::discover_workspace_packages(
        workspace.path(),
        aube::embed::WorkspaceDiscoveryOptions::confined_to_root(),
    )
    .unwrap();

    assert_eq!(
        packages,
        vec![
            workspace.path().join("apps/web").canonicalize().unwrap(),
            workspace
                .path()
                .join("packages/lib")
                .canonicalize()
                .unwrap(),
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn facade_binds_node_bins_and_refreshes_warm_install_bindings() {
    use std::os::unix::fs::PermissionsExt;
    initialize_test_host();
    let project = tempfile::tempdir().unwrap();
    let cache = project.path().join("cache");
    let store = project.path().join("store");
    seed_cached_registry_package(&cache, &store);
    std::fs::write(
        project.path().join("package.json"),
        r#"{"dependencies":{"cached-only":"1.0.0"}}"#,
    )
    .unwrap();
    let first = project.path().join("first-node");
    let second = project.path().join("second-node");
    for (node, label) in [(&first, "first"), (&second, "second")] {
        std::fs::write(node, format!("#!/bin/sh\nprintf '{label}\\n'\n")).unwrap();
        std::fs::set_permissions(node, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let overrides = aube::embed::EmbedderInstallOverrides {
        cache_dir: Some(cache),
        store_dir: Some(store),
        use_global_virtual_store: Some(true),
    };
    let mut options = InstallOptions::new(project.path());
    options.runtime = Some(aube::embed::EmbedderRuntime::default().bind_bins_to(&first));
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install_with_overrides(options.clone(), overrides.clone())
        .await
        .unwrap();
    assert_cached_package_is_project_local(project.path(), project.path());
    let bin = project.path().join("node_modules/.bin/cached-only");
    assert_eq!(
        std::process::Command::new(&bin).output().unwrap().stdout,
        b"first\n"
    );
    options.frozen_mode = aube::embed::FrozenMode::Frozen;
    // No-op replay retains the binding.
    aube::embed::install_with_overrides(options.clone(), overrides.clone())
        .await
        .unwrap();
    assert_eq!(
        std::process::Command::new(&bin).output().unwrap().stdout,
        b"first\n"
    );
    // A changed binding invalidates install freshness even with a frozen graph.
    options.runtime = Some(aube::embed::EmbedderRuntime::default().bind_bins_to(&second));
    aube::embed::install_with_overrides(options.clone(), overrides.clone())
        .await
        .unwrap();
    assert_eq!(
        std::process::Command::new(&bin).output().unwrap().stdout,
        b"second\n"
    );
    options.runtime = None;
    aube::embed::install_with_overrides(options, overrides)
        .await
        .unwrap();
    assert_eq!(
        aube_linker::sys::resolve_bin_shim_with_node(&bin)
            .unwrap()
            .unwrap()
            .node,
        None
    );
}

#[tokio::test]
async fn facade_rejects_relative_bin_runtime_before_mutating_project() {
    initialize_test_host();
    let project = tempfile::tempdir().unwrap();
    let manifest = r#"{"private":true}"#;
    std::fs::write(project.path().join("package.json"), manifest).unwrap();
    let mut options = InstallOptions::new(project.path());
    options.runtime = Some(aube::embed::EmbedderRuntime::default().bind_bins_to("relative/node"));
    let error = aube::embed::install_with_overrides(options, Default::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("must be absolute"));
    assert!(!project.path().join("node_modules").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("package.json")).unwrap(),
        manifest
    );
}

#[cfg(unix)]
#[tokio::test]
async fn facade_concurrent_adds_keep_runtime_bindings_separate() {
    use std::os::unix::fs::PermissionsExt;
    initialize_test_host();
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let store = root.path().join("store");
    seed_cached_registry_package(&cache, &store);
    let install = async |label: &str| {
        let project = root.path().join(label);
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("package.json"), "{}").unwrap();
        let node = project.join("node");
        std::fs::write(&node, format!("#!/bin/sh\nprintf '{label}\\n'\n")).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        aube::embed::add_with_overrides(
            &project,
            &["cached-only@1.0.0".into()],
            aube::embed::AddToProjectOptions {
                runtime: Some(aube::embed::EmbedderRuntime::default().bind_bins_to(node)),
                ignore_scripts: true,
                offline: true,
                control: InstallControl::silent(),
                ..Default::default()
            },
            aube::embed::EmbedderInstallOverrides {
                cache_dir: Some(cache.clone()),
                store_dir: Some(store.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let bin = project.join("node_modules/.bin/cached-only");
        (bin, format!("{label}\n"))
    };
    let (first, second) = tokio::join!(install("first"), install("second"));
    for (bin, expected) in [first, second] {
        let output = std::process::Command::new(bin).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}
