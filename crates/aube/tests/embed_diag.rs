use aube::embed::{InstallControl, InstallOptions, NetworkMode};

#[test]
fn embedded_install_honors_env_driven_diagnostics() {
    let sandbox = tempfile::tempdir().unwrap();
    let project = sandbox.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("package.json"), "{}\n").unwrap();
    let trace = sandbox.path().join("embed-diag.jsonl");

    // Safety: this integration-test binary contains one test and has not
    // created the Tokio runtime or any other threads yet.
    unsafe {
        std::env::set_var("AUBE_DIAG_FILE", &trace);
        std::env::set_var("AUBE_DIAG_FLUSH", "1");
        std::env::set_var("AUBE_DIAG_KERNEL", "1");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut options = InstallOptions::new(&project);
        options.ignore_scripts = true;
        options.network_mode = NetworkMode::Offline;
        options.control = InstallControl::silent();
        aube::embed::install_with_overrides(
            options,
            aube::embed::EmbedderInstallOverrides {
                use_global_virtual_store: Some(false),
                cache_dir: Some(sandbox.path().join("cache")),
                store_dir: Some(sandbox.path().join("store")),
            },
        )
        .await
        .unwrap();
    });

    let contents = std::fs::read_to_string(&trace).unwrap();
    assert!(contents.contains(r#""cat":"install","name":"begin""#));
    #[cfg(target_os = "linux")]
    assert!(contents.contains(r#""rss_current":"#));
}
