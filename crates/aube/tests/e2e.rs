//! Cross-platform happy-path smoke tests for the `aube` binary.
//!
//! Deliberately small: a handful of hermetic checks that exercise the
//! CLI entry point, a no-op install, and the lifecycle script runner
//! without touching the network or the user's real store. The heavier
//! coverage lives in the BATS suite under `test/`, which only runs on
//! Unix. These tests fill the gap for the Windows CI job.

use assert_cmd::Command;
use predicates::boolean::PredicateBooleanExt;
use std::fs;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Build an isolated project root plus private `HOME` / aube store /
/// cache so the test can't see or mutate the developer's real state.
struct Sandbox {
    _root: TempDir,
    project: std::path::PathBuf,
    home: std::path::PathBuf,
    store: std::path::PathBuf,
    cache: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("aube-e2e-")
            .tempdir()
            .unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        let store = root.path().join("store");
        let cache = root.path().join("cache");
        for dir in [&project, &home, &store, &cache] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            project,
            home,
            store,
            cache,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("aube").unwrap();
        cmd.current_dir(&self.project)
            .env_remove("AUBE_CONFIG")
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AUBE_STORE_DIR", &self.store)
            .env("AUBE_CACHE_DIR", &self.cache)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("NO_COLOR", "1");
        cmd
    }

    fn write_manifest(&self, contents: &str) {
        fs::write(self.project.join("package.json"), contents).unwrap();
    }
}

/// Match `expected` in miette output, whose line wrapping depends on path lengths.
fn wrapped_contains(expected: &'static str) -> impl predicates::Predicate<[u8]> {
    predicates::function::function(move |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .split_whitespace()
            .filter(|word| *word != "│")
            .collect::<Vec<_>>()
            .join(" ")
            .contains(expected)
    })
}

fn e2e_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[test]
fn version_flag_reports_binary_version() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag_lists_install_command() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("install"));
}

#[cfg(unix)]
#[test]
fn aubr_replaces_itself_with_the_final_script_shell() {
    use std::os::unix::fs::symlink;
    use std::process::Stdio;

    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"exec-handoff","private":true,"scripts":{"pid":"echo $$"}}"#);
    let aubr = sbx._root.path().join("aubr");
    symlink(assert_cmd::cargo::cargo_bin!("aube"), &aubr).unwrap();

    let child = std::process::Command::new(aubr)
        .args(["--no-install", "pid"])
        .current_dir(&sbx.project)
        .env_remove("AUBE_CONFIG")
        .env("HOME", &sbx.home)
        .env("AUBE_STORE_DIR", &sbx.store)
        .env("AUBE_CACHE_DIR", &sbx.cache)
        .env("XDG_CACHE_HOME", &sbx.cache)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        child_pid.to_string()
    );
}

#[cfg(unix)]
#[test]
fn aubr_keeps_control_when_a_post_script_must_run() {
    use assert_cmd::assert::OutputAssertExt;
    use std::os::unix::fs::symlink;

    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{"name":"exec-handoff","private":true,"scripts":{"build":"echo build","postbuild":"echo postbuild"}}"#,
    );
    let aubr = sbx._root.path().join("aubr");
    symlink(assert_cmd::cargo::cargo_bin!("aube"), &aubr).unwrap();

    std::process::Command::new(aubr)
        .args(["--no-install", "build"])
        .current_dir(&sbx.project)
        .env_remove("AUBE_CONFIG")
        .env("HOME", &sbx.home)
        .env("AUBE_STORE_DIR", &sbx.store)
        .env("AUBE_CACHE_DIR", &sbx.cache)
        .env("XDG_CACHE_HOME", &sbx.cache)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicates::str::contains("build\npostbuild\n"));
}

#[test]
fn dynamic_completion_keeps_stdout_when_use_stderr_is_configured() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    fs::write(sbx.project.join(".npmrc"), "use-stderr=true\n").unwrap();

    sbx.cmd()
        .args([
            "__complete_word__",
            "--shell",
            "bash",
            "--line",
            "aube config get ",
            "--cursor",
            "16",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-install-peers"));
}

#[test]
fn dynamic_completion_rejects_an_invalid_dir() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"dependencies":{"react":"^19"}}"#);

    sbx.cmd()
        .args([
            "__complete_word__",
            "--shell",
            "bash",
            "--line",
            "aube -C missing add ",
            "--cursor",
            "20",
            "--candidates",
            "package",
        ])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn install_on_manifest_without_deps_creates_state_file() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"e2e-empty","version":"0.0.0"}"#);

    sbx.cmd().arg("install").assert().success();

    assert!(
        sbx.project.join("node_modules/.aube-state").exists(),
        "expected aube to drop a state file after install"
    );
}

#[test]
fn run_executes_a_simple_script() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-run",
            "version": "0.0.0",
            "scripts": { "greet": "echo aube-e2e-ok" }
        }"#,
    );

    sbx.cmd()
        .arg("run")
        .arg("greet")
        .assert()
        .success()
        .stdout(predicates::str::contains("aube-e2e-ok"));
}

// --- standalone exit-code contract ---------------------------------------
//
// The command layer (`aube::commands::*::run`) no longer calls
// `std::process::exit` directly: a failing command returns a propagatable
// exit code that the binary's single `std::process::exit` (in `main.rs`)
// applies. That keeps the layer embed-safe — a host driving aube as a
// library is handed the code instead of being hard-killed. These tests pin
// the *standalone* contract so the indirection stays byte-for-byte: the code
// the user sees on the command line must be unchanged by the refactor.

#[test]
fn run_propagates_a_failing_scripts_exact_exit_code() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-exit",
            "version": "0.0.0",
            "scripts": { "boom": "exit 7" }
        }"#,
    );

    // The child's exit code (7) must surface as aube's own exit code,
    // not be flattened to a generic 1 — this is the path that previously
    // called `std::process::exit(exit_code_from_status(status))` in place.
    sbx.cmd()
        .args(["run", "--no-install", "boom"])
        .assert()
        .code(7);
}

#[test]
fn run_if_present_on_missing_script_exits_zero() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"e2e-ifpresent","version":"0.0.0"}"#);

    // The `--if-present` no-op path returns `Ok(None)` (success) rather
    // than a non-zero code; pin it so the success side of the contract
    // can't silently start exiting non-zero.
    sbx.cmd()
        .args(["run", "--no-install", "--if-present", "nope"])
        .assert()
        .success();
}

#[test]
fn run_failing_pre_script_short_circuits_with_its_code() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-prescript",
            "version": "0.0.0",
            "scripts": { "prebuild": "exit 5", "build": "echo MAIN_RAN" }
        }"#,
    );

    // A failing pre-script propagates its exact code (5) AND stops the
    // chain before the main script runs — the previous in-place exit gave
    // the same ordering, so the short-circuit must be preserved.
    sbx.cmd()
        .args(["run", "--no-install", "build"])
        .assert()
        .code(5)
        .stdout(predicates::str::contains("MAIN_RAN").not());
}

#[test]
fn run_forwards_extra_args_verbatim() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    // A quote-free body takes the direct-exec path on Unix and the shell
    // on Windows, so this pins the same argv contract on both: forwarded
    // args reach the script as written, with nothing expanded or split.
    sbx.write_manifest(
        r#"{
            "name": "e2e-run-args",
            "version": "0.0.0",
            "scripts": { "probe": "node probe.js" }
        }"#,
    );
    fs::write(
        sbx.project.join("probe.js"),
        "console.log(JSON.stringify(process.argv.slice(2)))",
    )
    .unwrap();

    sbx.cmd()
        .args(["run", "--no-install", "probe", "--", "a b", "$HOME", "*"])
        .assert()
        .success()
        .stdout(predicates::str::contains(r#"["a b","$HOME","*"]"#));
}

#[test]
fn run_reports_a_missing_command_like_a_shell() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-run-missing",
            "version": "0.0.0",
            "scripts": { "nope": "aube-definitely-not-a-real-binary" }
        }"#,
    );

    // The body is shaped for direct exec, but the program does not
    // resolve, so it must fall back to the shell rather than inventing an
    // error — which is what keeps 127 (and the shell's own stderr) intact.
    let assert = sbx.cmd().args(["run", "--no-install", "nope"]).assert();
    if cfg!(unix) {
        assert.code(127);
    } else {
        assert.failure();
    }
}

#[test]
fn relocated_workspace_lockfiles_survive_clean_frozen_installs() {
    let _guard = e2e_lock();
    for filename in [
        "aube-lock.yaml",
        "pnpm-lock.yaml",
        "package-lock.json",
        "bun.lock",
    ] {
        for lock_dir in ["locks", "../sibling-locks"] {
            for (linker, gvs) in [
                ("isolated", false),
                ("isolated", true),
                ("hoisted", false),
                ("hoisted", true),
            ] {
                let sbx = Sandbox::new();
                let host = sbx.project.join("vendor/host");
                fs::create_dir_all(&host).unwrap();
                fs::write(
                    host.join("package.json"),
                    r#"{"name":"host","version":"1.0.0","main":"index.js"}"#,
                )
                .unwrap();
                fs::write(host.join("index.js"), "module.exports = 'host ok';").unwrap();
                let leaf = sbx.project.join("leaf");
                fs::create_dir_all(&leaf).unwrap();
                fs::write(
                    leaf.join("package.json"),
                    r#"{"name":"leaf","version":"1.0.0","main":"index.js"}"#,
                )
                .unwrap();
                fs::write(leaf.join("index.js"), "module.exports = 'relocated ok';").unwrap();
                let archive = fs::File::create(sbx.project.join("leaf.tgz")).unwrap();
                let gzip = flate2::write::GzEncoder::new(archive, flate2::Compression::default());
                let mut tar = tar::Builder::new(gzip);
                tar.append_dir_all("package", &leaf).unwrap();
                tar.into_inner().unwrap().finish().unwrap();
                let member = sbx.project.join("apps/app");
                fs::create_dir_all(&member).unwrap();
                sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["apps/*"],"dependencies":{"leaf":"file:./leaf.tgz","host":"file:vendor/host"}}"#);
                fs::write(
                    sbx.project.join("pnpm-workspace.yaml"),
                    "packages: [apps/*]\n",
                )
                .unwrap();
                fs::write(member.join("package.json"), r#"{"name":"app","version":"1.0.0","dependencies":{"leaf":"file:../../leaf.tgz"}}"#).unwrap();
                fs::write(sbx.project.join(".npmrc"), format!("default-lockfile={filename}\nlockfile-dir={lock_dir}\nnode-linker={linker}\nenable-global-virtual-store={gvs}\n")).unwrap();
                let lock = sbx.project.join(lock_dir).join(filename);
                sbx.cmd()
                    .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                    .assert()
                    .success();
                let written = fs::read(&lock).unwrap();
                for frozen in [false, true] {
                    if frozen {
                        fs::remove_dir_all(sbx.project.join("node_modules")).unwrap();
                        let modules = member.join("node_modules");
                        if modules.exists() {
                            fs::remove_dir_all(modules).unwrap();
                        }
                        fs::remove_dir_all(&sbx.store).unwrap();
                        fs::remove_dir_all(&sbx.cache).unwrap();
                        // Member invocation and filtering still use the complete workspace lock.
                        sbx.cmd()
                            .current_dir(&member)
                            .args([
                                "--filter",
                                "app",
                                "install",
                                "--ignore-scripts",
                                "--frozen-lockfile",
                            ])
                            .assert()
                            .success();
                        assert_eq!(
                            fs::read(&lock).unwrap(),
                            written,
                            "frozen install rewrote {filename}"
                        );
                    }
                    for dir in [&sbx.project, &member] {
                        let output = std::process::Command::new("node")
                            .current_dir(dir)
                            .args(["-e", "console.log(require('leaf')); if (process.cwd().endsWith('project') && require('host') !== 'host ok') process.exit(1)"])
                            .output()
                            .unwrap();
                        assert!(
                            output.status.success(),
                            "{filename}, {lock_dir}, {linker}, {gvs}, frozen={frozen}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        assert_eq!(
                            String::from_utf8_lossy(&output.stdout).trim(),
                            "relocated ok"
                        );
                    }
                }
                if matches!(filename, "package-lock.json" | "bun.lock") {
                    // Native roots have no physical owner; retain the manifest guard.
                    fs::write(lock.parent().unwrap().join("package.json"), "{}").unwrap();
                }
                let other = sbx._root.path().join("other");
                fs::create_dir_all(&other).unwrap();
                fs::write(
                    other.join("package.json"),
                    r#"{"name":"other","private":true}"#,
                )
                .unwrap();
                sbx.cmd()
                    .current_dir(&other)
                    .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                    .arg(format!(
                        "--lockfile-dir={}",
                        lock.parent().unwrap().display()
                    ))
                    .assert()
                    .failure()
                    .stderr(wrapped_contains("records importers from other projects"));
                assert_eq!(fs::read(&lock).unwrap(), written);
            }
        }
    }
}

#[test]
fn relocated_workspace_lockfiles_prune_stale_members() {
    let _guard = e2e_lock();
    for filename in [
        "aube-lock.yaml",
        "pnpm-lock.yaml",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "bun.lock",
    ] {
        for lock_dir in ["locks", "../sibling-locks"] {
            for change in ["delete", "rename", "exclude"] {
                let sbx = Sandbox::new();
                let leaf = sbx.project.join("leaf");
                fs::create_dir_all(&leaf).unwrap();
                fs::write(
                    leaf.join("package.json"),
                    r#"{"name":"leaf","version":"1.0.0"}"#,
                )
                .unwrap();
                let archive = fs::File::create(sbx.project.join("leaf.tgz")).unwrap();
                let gzip = flate2::write::GzEncoder::new(archive, flate2::Compression::default());
                let mut tar = tar::Builder::new(gzip);
                tar.append_dir_all("package", &leaf).unwrap();
                tar.into_inner().unwrap().finish().unwrap();
                let member = sbx.project.join("apps/app");
                fs::create_dir_all(&member).unwrap();
                sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["apps/*"],"dependencies":{"leaf":"file:leaf.tgz"}}"#);
                fs::write(
                    sbx.project.join("pnpm-workspace.yaml"),
                    "packages: [apps/*]\n",
                )
                .unwrap();
                fs::write(
                    member.join("package.json"),
                    r#"{"name":"app","version":"1.0.0","dependencies":{"leaf":"file:../../leaf.tgz"}}"#,
                )
                .unwrap();
                fs::write(sbx.project.join(".npmrc"), format!("default-lockfile={filename}\nlockfile-dir={lock_dir}\nenable-global-virtual-store=false\n")).unwrap();
                sbx.cmd()
                    .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                    .assert()
                    .success();
                match change {
                    "delete" => fs::remove_dir_all(&member).unwrap(),
                    "rename" => fs::rename(&member, sbx.project.join("apps/renamed")).unwrap(),
                    "exclude" => {
                        fs::write(
                            sbx.project.join("pnpm-workspace.yaml"),
                            "packages: [nothing/*]\n",
                        )
                        .unwrap();
                        sbx.write_manifest(
                            r#"{"name":"root","private":true,"workspaces":["nothing/*"],"dependencies":{"leaf":"file:leaf.tgz"}}"#,
                        );
                    }
                    _ => unreachable!(),
                }
                sbx.cmd()
                    .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                    .assert()
                    .success();
                let lock_dir = sbx.project.join(lock_dir).canonicalize().unwrap();
                let (graph, kind) = aube_lockfile::parse_lockfile_with_project_root(
                    &lock_dir,
                    &sbx.project,
                    &aube_manifest::PackageJson::default(),
                    aube_lockfile::ParseOptions::default(),
                    None,
                )
                .unwrap();
                let prefix = if matches!(
                    kind,
                    aube_lockfile::LockfileKind::Aube | aube_lockfile::LockfileKind::Pnpm
                ) {
                    pathdiff::diff_paths(&sbx.project, &lock_dir).unwrap()
                } else {
                    std::path::PathBuf::new()
                };
                let old_key = prefix.join("apps/app").to_string_lossy().replace('\\', "/");
                assert!(
                    !graph.importers.contains_key(&old_key),
                    "{filename}, {change}: stale member survived"
                );
                if change == "rename" {
                    let new_key = prefix
                        .join("apps/renamed")
                        .to_string_lossy()
                        .replace('\\', "/");
                    assert!(
                        graph.importers.contains_key(&new_key),
                        "{filename}: renamed member missing, keys: {:?}",
                        graph.importers.keys()
                    );
                }
                let lock = lock_dir.join(filename);
                let before = fs::read(&lock).unwrap();
                sbx.cmd()
                    .args(["install", "--ignore-scripts", "--frozen-lockfile"])
                    .assert()
                    .success();
                assert_eq!(before, fs::read(&lock).unwrap());
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn relocated_native_lockfile_rejects_lock_dir_with_another_manifest_before_writing() {
    let _guard = e2e_lock();
    for filename in ["package-lock.json", "bun.lock"] {
        let sbx = Sandbox::new();
        sbx.write_manifest(r#"{"name":"root","private":true}"#);
        let locks = sbx.project.join("locks");
        fs::create_dir_all(&locks).unwrap();
        fs::write(locks.join("package.json"), r#"{"name":"other"}"#).unwrap();
        fs::write(
            sbx.project.join(".npmrc"),
            format!("default-lockfile={filename}\nlockfile-dir=locks\n"),
        )
        .unwrap();
        sbx.cmd()
            .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
            .assert()
            .failure()
            .stderr(wrapped_contains("another project's package.json"));
        assert!(!locks.join(filename).exists());
    }
}

#[test]
fn relocated_workspace_lockfiles_reject_physically_external_importers() {
    let _guard = e2e_lock();
    for filename in ["aube-lock.yaml", "pnpm-lock.yaml"] {
        for lock_dir in ["locks", "../sibling-locks"] {
            for spelling in ["member-symlink", "alias-parent"] {
                let sbx = Sandbox::new();
                let member = sbx.project.join("apps/app");
                fs::create_dir_all(&member).unwrap();
                sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["apps/*"]}"#);
                fs::write(
                    sbx.project.join("pnpm-workspace.yaml"),
                    "packages: [apps/*]\n",
                )
                .unwrap();
                let leaf = sbx.project.join("leaf");
                fs::create_dir_all(&leaf).unwrap();
                fs::write(
                    leaf.join("package.json"),
                    r#"{"name":"leaf","version":"1.0.0"}"#,
                )
                .unwrap();
                fs::write(
                    member.join("package.json"),
                    r#"{"name":"app","dependencies":{"leaf":"file:../../leaf"}}"#,
                )
                .unwrap();
                fs::write(sbx.project.join(".npmrc"), format!("default-lockfile={filename}\nlockfile-dir={lock_dir}\nenable-global-virtual-store=false\n")).unwrap();
                sbx.cmd()
                    .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                    .assert()
                    .success();
                let outside = sbx._root.path().join("outside");
                fs::create_dir_all(&outside).unwrap();
                fs::rename(&member, outside.join("app")).unwrap();
                std::os::unix::fs::symlink(outside.join("app"), &member).unwrap();
                fs::write(
                    sbx.project.join("pnpm-workspace.yaml"),
                    "packages: [nothing/*]\n",
                )
                .unwrap();
                sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["nothing/*"]}"#);
                let locks = sbx.project.join(lock_dir).canonicalize().unwrap();
                let lock = locks.join(filename);
                if spelling == "alias-parent" {
                    std::os::unix::fs::symlink(outside.join("app"), sbx.project.join("alias"))
                        .unwrap();
                    let prefix = pathdiff::diff_paths(&sbx.project, &locks).unwrap();
                    let old = prefix.join("apps/app").to_string_lossy().into_owned();
                    let new = prefix.join("alias/../ghost").to_string_lossy().into_owned();
                    let text = fs::read_to_string(&lock).unwrap();
                    assert!(text.contains(&format!("  {old}:")));
                    fs::write(
                        &lock,
                        text.replace(&format!("  {old}:"), &format!("  {new}:")),
                    )
                    .unwrap();
                }
                let before = fs::read(&lock).unwrap();
                for mode in ["--no-frozen-lockfile", "--frozen-lockfile"] {
                    sbx.cmd()
                        .args(["install", "--ignore-scripts", mode])
                        .assert()
                        .code(11)
                        .stderr(wrapped_contains("records importers from other projects"));
                    assert_eq!(before, fs::read(&lock).unwrap());
                }
            }
        }
    }
}

#[test]
fn relocated_workspace_lockfiles_allow_member_moves_with_colliding_keys() {
    let _guard = e2e_lock();
    for lock_location in ["locks", "../sibling-locks"] {
        for (filename, absolute) in [
            ("aube-lock.yaml", false),
            ("aube-lock.yaml", true),
            ("pnpm-lock.yaml", false),
            ("pnpm-lock.yaml", true),
        ] {
            let sbx = Sandbox::new();
            let member = sbx.project.join("locks/apps/app");
            fs::create_dir_all(&member).unwrap();
            let leaf = sbx.project.join("leaf");
            fs::create_dir_all(&leaf).unwrap();
            fs::write(
                leaf.join("package.json"),
                r#"{"name":"leaf","version":"1.0.0","main":"index.js"}"#,
            )
            .unwrap();
            fs::write(leaf.join("index.js"), "module.exports = 'leaf ok';").unwrap();
            sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["locks/apps/*"]}"#);
            fs::write(
                sbx.project.join("pnpm-workspace.yaml"),
                "packages: [locks/apps/*]\n",
            )
            .unwrap();
            let specifier = if absolute {
                format!("file:{}", leaf.display())
            } else {
                "file:../../../leaf".into()
            };
            fs::write(
                member.join("package.json"),
                serde_json::to_string(
                    &serde_json::json!({"name":"app","dependencies":{"leaf":specifier}}),
                )
                .unwrap(),
            )
            .unwrap();
            fs::write(sbx.project.join(".npmrc"), format!("default-lockfile={filename}\nlockfile-dir={lock_location}\nenable-global-virtual-store=false\n")).unwrap();
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                .assert()
                .success();
            fs::create_dir_all(sbx.project.join("apps")).unwrap();
            let moved = sbx.project.join("apps/app");
            fs::rename(&member, &moved).unwrap();
            fs::remove_dir(sbx.project.join("locks/apps")).unwrap();
            sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["apps/*"]}"#);
            fs::write(
                sbx.project.join("pnpm-workspace.yaml"),
                "packages: [apps/*]\n",
            )
            .unwrap();
            let specifier = if absolute {
                format!("file:{}", leaf.display())
            } else {
                "file:../../leaf".into()
            };
            fs::write(
                moved.join("package.json"),
                serde_json::to_string(
                    &serde_json::json!({"name":"app","dependencies":{"leaf":specifier}}),
                )
                .unwrap(),
            )
            .unwrap();
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                .assert()
                .success();
            let locks = fs::canonicalize(sbx.project.join(lock_location)).unwrap();
            let graph =
                aube_lockfile::parse_lockfile(&locks, &aube_manifest::PackageJson::default())
                    .unwrap();
            let old_key = pathdiff::diff_paths(&member, &locks).unwrap();
            let new_key = pathdiff::diff_paths(&moved, &locks).unwrap();
            assert!(
                !graph
                    .importers
                    .contains_key(&old_key.to_string_lossy().replace('\\', "/"))
            );
            assert!(
                graph
                    .importers
                    .contains_key(&new_key.to_string_lossy().replace('\\', "/"))
            );
            let lock = locks.join(filename);
            let before = fs::read(&lock).unwrap();
            for path in [
                sbx.project.join("node_modules"),
                moved.join("node_modules"),
                sbx.store.clone(),
                sbx.cache.clone(),
            ] {
                if path.exists() {
                    fs::remove_dir_all(path).unwrap();
                }
            }
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--frozen-lockfile"])
                .assert()
                .success();
            assert_eq!(before, fs::read(&lock).unwrap());
            let output = std::process::Command::new("node")
                .current_dir(&moved)
                .args(["-e", "console.log(require('leaf'))"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "leaf ok");
        }
    }
}

#[test]
fn relocated_legacy_workspace_lockfiles_migrate_on_normal_install() {
    let _guard = e2e_lock();
    for filename in ["aube-lock.yaml", "pnpm-lock.yaml"] {
        for explicit in [false, true] {
            let sbx = Sandbox::new();
            let member = sbx.project.join("apps/app");
            let leaf = sbx.project.join("leaf");
            fs::create_dir_all(&member).unwrap();
            fs::create_dir_all(&leaf).unwrap();
            fs::write(
                leaf.join("package.json"),
                r#"{"name":"leaf","version":"1.0.0","main":"index.js"}"#,
            )
            .unwrap();
            fs::write(leaf.join("index.js"), "module.exports = 'leaf ok';").unwrap();
            sbx.write_manifest(r#"{"name":"root","private":true,"workspaces":["apps/*"]}"#);
            fs::write(
                sbx.project.join("pnpm-workspace.yaml"),
                "packages: [apps/*]\n",
            )
            .unwrap();
            fs::write(
                member.join("package.json"),
                r#"{"name":"app","dependencies":{"leaf":"file:../../leaf"}}"#,
            )
            .unwrap();
            fs::write(sbx.project.join(".npmrc"), format!("default-lockfile={filename}\nlockfile-dir=locks\nenable-global-virtual-store=false\n")).unwrap();
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
                .assert()
                .success();
            let lock = sbx.project.join("locks").join(filename);
            let legacy = fs::read_to_string(&lock)
                .unwrap()
                .replace("../apps/app:", "apps/app:");
            assert!(!legacy.contains("../apps/app:"));
            fs::write(&lock, &legacy).unwrap();
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--frozen-lockfile"])
                .assert()
                .code(11)
                .stderr(wrapped_contains(
                    "older aube wrote mixed workspace coordinates",
                ))
                .stderr(predicates::str::contains("regenerate"));
            assert_eq!(legacy.as_bytes(), fs::read(&lock).unwrap());
            let mut cmd = sbx.cmd();
            cmd.env_remove("CI").args(["install", "--ignore-scripts"]);
            if explicit {
                cmd.arg("--no-frozen-lockfile");
            }
            cmd.assert().success();
            let migrated = fs::read(&lock).unwrap();
            assert!(String::from_utf8_lossy(&migrated).contains("../apps/app:"));
            assert_ne!(legacy.as_bytes(), migrated);
            for path in [
                sbx.project.join("node_modules"),
                member.join("node_modules"),
                sbx.store.clone(),
                sbx.cache.clone(),
            ] {
                if path.exists() {
                    fs::remove_dir_all(path).unwrap();
                }
            }
            sbx.cmd()
                .args(["install", "--ignore-scripts", "--frozen-lockfile"])
                .assert()
                .success();
            assert_eq!(migrated, fs::read(&lock).unwrap());
            let output = std::process::Command::new("node")
                .current_dir(&member)
                .args(["-e", "console.log(require('leaf'))"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "leaf ok");
        }
    }
}

#[test]
fn relocated_bun_local_versions_ignore_shadow_manifests() {
    let _guard = e2e_lock();
    for lock_dir in ["locks", "../sibling-locks"] {
        let sbx = Sandbox::new();
        let host = sbx.project.join("vendor/host");
        fs::create_dir_all(&host).unwrap();
        fs::write(
            host.join("package.json"),
            r#"{"name":"host","version":"1.0.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            host.join("index.js"),
            "module.exports = require('./package.json').version;",
        )
        .unwrap();
        sbx.write_manifest(
            r#"{"name":"root","private":true,"dependencies":{"host":"file:vendor/host"}}"#,
        );
        fs::write(sbx.project.join(".npmrc"), format!("default-lockfile=bun.lock\nlockfile-dir={lock_dir}\nenable-global-virtual-store=false\n")).unwrap();
        sbx.cmd()
            .args(["install", "--ignore-scripts", "--no-frozen-lockfile"])
            .assert()
            .success();
        let lock_dir = sbx.project.join(lock_dir);
        let shadow = lock_dir.join("vendor/host");
        fs::create_dir_all(&shadow).unwrap();
        fs::write(
            shadow.join("package.json"),
            r#"{"name":"host","version":"9.9.9"}"#,
        )
        .unwrap();
        let lock = lock_dir.join("bun.lock");
        let before = fs::read(&lock).unwrap();
        fs::remove_dir_all(sbx.project.join("node_modules")).unwrap();
        fs::remove_dir_all(&sbx.cache).unwrap();
        fs::remove_dir_all(&sbx.store).unwrap();
        sbx.cmd()
            .args(["install", "--ignore-scripts", "--frozen-lockfile"])
            .assert()
            .success()
            .stderr(predicates::str::contains("host@1.0.0"))
            .stderr(predicates::str::contains("9.9.9").not());
        assert_eq!(before, fs::read(&lock).unwrap());
        let output = std::process::Command::new("node")
            .current_dir(&sbx.project)
            .args(["-e", "console.log(require('host'))"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1.0.0");
    }
}

#[test]
fn completion_bin_selects_one_program_and_rejects_install() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    for program in ["aube", "aubr", "aubx"] {
        let output = sbx
            .cmd()
            .args(["completion", "bash", "--bin", program])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let script = String::from_utf8(output).unwrap();
        let registrations: Vec<_> = script
            .lines()
            .filter(|line| line.starts_with("complete "))
            .collect();
        assert_eq!(registrations.len(), 1, "{script}");
        assert!(
            registrations[0].ends_with(&format!("'{program}'")),
            "{script}"
        );
    }
    sbx.cmd()
        .args(["completion", "bash", "--bin", "unknown"])
        .assert()
        .failure();
    sbx.cmd()
        .args(["completion", "bash", "--bin", "aube", "--install"])
        .assert()
        .failure();
    assert!(
        !sbx.home
            .join(".local/share/bash-completion/completions/aube")
            .exists()
    );
}
