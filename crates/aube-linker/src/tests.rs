use super::*;
use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use aube_store::Store;

fn setup_store_with_files(dir: &Path) -> (Store, BTreeMap<String, aube_store::PackageIndex>) {
    let store = Store::at(dir.join("store/files"));

    let mut indices = BTreeMap::new();

    // foo@1.0.0 with index.js
    let foo_stored = store
        .import_bytes(b"module.exports = 'foo';", false)
        .unwrap();
    let mut foo_index = PackageIndex::default();
    foo_index.insert("index.js".to_string(), foo_stored);

    // foo also has package.json
    let foo_pkg = store
        .import_bytes(b"{\"name\":\"foo\",\"version\":\"1.0.0\"}", false)
        .unwrap();
    foo_index.insert("package.json".to_string(), foo_pkg);
    indices.insert("foo@1.0.0".to_string(), foo_index);

    // bar@2.0.0 with index.js
    let bar_stored = store
        .import_bytes(b"module.exports = 'bar';", false)
        .unwrap();
    let mut bar_index = PackageIndex::default();
    bar_index.insert("index.js".to_string(), bar_stored);
    indices.insert("bar@2.0.0".to_string(), bar_index);

    (store, indices)
}

fn make_graph() -> LockfileGraph {
    let mut packages = BTreeMap::new();

    let mut foo_deps = BTreeMap::new();
    foo_deps.insert("bar".to_string(), "2.0.0".to_string());

    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: None,
            dependencies: foo_deps,
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: None,
            dependencies: BTreeMap::new(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    LockfileGraph {
        importers,
        packages,
        ..Default::default()
    }
}

#[test]
fn test_detect_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let strategy = Linker::detect_strategy(dir.path());
    // The probe resolves a same-FS success to the OS-specific `auto`
    // strategy and a cross-FS failure to `Copy`. On macOS the same-FS
    // strategy is `ReflinkAuto` (APFS clonefile, with the auto-only
    // hardlink-before-copy fallback), elsewhere `Hardlink`. Whether the
    // tempdir's FS supports the same-mount link depends on the runner, so
    // accept the same-FS strategy or `Copy`, but reject the *other* OS's
    // same-FS strategy — that would mean the cfg gate resolved on the
    // wrong target. The probe must never yield the plain `Reflink`
    // explicit selections use.
    match strategy {
        LinkStrategy::Copy => {}
        LinkStrategy::Reflink => {
            panic!("`auto` probe must resolve same-FS to ReflinkAuto, never plain Reflink")
        }
        #[cfg(target_os = "macos")]
        LinkStrategy::ReflinkAuto => {}
        #[cfg(target_os = "macos")]
        LinkStrategy::Hardlink => panic!("macOS `auto` must resolve same-FS to ReflinkAuto"),
        #[cfg(not(target_os = "macos"))]
        LinkStrategy::Hardlink => {}
        #[cfg(not(target_os = "macos"))]
        LinkStrategy::ReflinkAuto => panic!("non-macOS `auto` must resolve same-FS to Hardlink"),
    }
}

// Representative dep_paths exercising every branch of
// `dep_path_to_filename`: plain, scoped (`/` → `+`), peer-context
// (parens flatten to `_`), uppercase (forces the BLAKE3 short-hash),
// and a long peer graph (overflows max_length → truncate + hash).
const ENCODE_FIXTURES: &[&str] = &[
    "foo@1.0.0",
    "@scope/bar@2.0.0",
    "baz@3.0.0(react@18.2.0)",
    "@ng/Core@17.0.0",
    "@fig/eslint-config-autocomplete@2.0.0(@typescript-eslint+eslint-plugin@7.18.0(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(eslint@8.57.1))(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(@withfig+eslint-plugin-fig-linter@1.4.1)(eslint@8.57.1)(eslint-plugin-compat@4.2.0(eslint@8.57.1))(typescript@5.9.3)",
];

// The link step's serial pre-pass now computes each package's local
// `.aube/<entry>` name and its virtual-store subdir once and threads
// them into the par_iter (and on into `ensure_in_virtual_store`),
// replacing what used to be 2-3 redundant `dep_path_to_filename`
// encodes per package. The hoist is only output-preserving if the
// value carried forward is byte-identical to what each eliminated
// recompute site would have produced.
//
// The eliminated sites all bottomed out at the SAME primitive
// composition: the local `.aube/` link name was
// `dep_path_to_filename(dep_path, max)`, and the shared-store target
// (the par_iter's `global_entry` and the now-removed `subdir` encode
// inside the public `ensure_in_virtual_store`) was
// `dep_path_to_filename(hashes.hashed_dep_path(dep_path), max)`. So the
// non-vacuous check is to recompute each carried value INDEPENDENTLY
// from `dep_path_to_filename` (and `GraphHashes::hashed_dep_path`)
// directly — NOT by re-calling the very wrapper under test — and assert
// the linker's `aube_dir_entry_name` / `virtual_store_subdir` outputs
// equal it. A `dep_path_to_filename(dep_path, max)` is what the linker
// builds at the eliminated `aube_dir.join(self.aube_dir_entry_name(..))`
// and `self.virtual_store.join(self.virtual_store_subdir(..))` sites, so
// if the hoist ever carried a value that diverged from that primitive,
// these fail.

fn linker_for_encode_test(dir: &Path, hashes: Option<GraphHashes>) -> Linker {
    let store = Store::at(dir.join("store/files"));
    let mut linker = Linker::new(&store, LinkStrategy::Copy);
    if let Some(h) = hashes {
        linker = linker.with_graph_hashes(h);
    }
    linker
}

#[test]
fn precomputed_entry_name_and_subdir_match_recompute_unhashed() {
    let dir = tempfile::tempdir().unwrap();
    let linker = linker_for_encode_test(dir.path(), None);

    for dep_path in ENCODE_FIXTURES {
        // The values the pre-pass carries forward into the par_iter and
        // on into `ensure_in_virtual_store_with_subdir`.
        let precomputed_entry = linker.aube_dir_entry_name(dep_path);
        let precomputed_subdir = linker.virtual_store_subdir(dep_path);

        // Independent recompute of what the eliminated recompute sites
        // produced: both the `.aube/` link name and (in the unhashed
        // mode, where `hashed_dep_path` is the identity) the shared-store
        // subdir bottom out at the same bare-dep_path encode. Built from
        // the primitive directly, NOT by re-calling the linker wrapper,
        // so this catches the hoist carrying any value that diverged from
        // what the original `aube_dir.join(aube_dir_entry_name(..))` /
        // `virtual_store.join(virtual_store_subdir(..))` sites encoded.
        let recompute = dep_path_to_filename(dep_path, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH);
        assert_eq!(
            precomputed_entry, recompute,
            "entry name diverged from the bare-dep_path encode for {dep_path}"
        );
        assert_eq!(
            precomputed_subdir, recompute,
            "unhashed subdir diverged from the bare-dep_path encode for {dep_path}"
        );

        // Cross-check: with no hash fold the two carried values must
        // coincide, so a hoist that swapped entry for subdir (or vice
        // versa) in the unhashed path would still produce correct output
        // here — the hashed test below is what pins their separation.
        assert_eq!(
            precomputed_entry, precomputed_subdir,
            "entry name and subdir must coincide in the unhashed mode for {dep_path}"
        );
    }
}

#[test]
fn precomputed_subdir_matches_recompute_with_graph_hashes() {
    // With graph hashes installed, `virtual_store_subdir` folds the
    // per-dep_path hash into the leaf before encoding, so it diverges
    // from `aube_dir_entry_name` (which never applies the hash). The
    // hoist carries the *subdir* (hashed) into `ensure_in_virtual_store`
    // and the *entry name* (unhashed) into the local `.aube/` link —
    // mixing them up would be a behavior change. Pin that each carried
    // value matches its independent primitive recompute and that the two
    // are distinct under hashing.
    let dir = tempfile::tempdir().unwrap();
    let mut node_hash = std::collections::BTreeMap::new();
    for (i, dep_path) in ENCODE_FIXTURES.iter().enumerate() {
        // A deterministic non-empty hex hash per dep_path.
        node_hash.insert(
            (*dep_path).to_string(),
            format!("{:016x}{:016x}", 0x0123_4567_89ab_cdefu64, i as u64),
        );
    }
    let hashes = GraphHashes { node_hash };
    let linker = linker_for_encode_test(dir.path(), Some(hashes.clone()));

    for dep_path in ENCODE_FIXTURES {
        let entry = linker.aube_dir_entry_name(dep_path);
        let subdir = linker.virtual_store_subdir(dep_path);

        // Independent recompute of the eliminated sites: the `.aube/`
        // link name is the bare-dep_path encode, while the shared-store
        // subdir is the encode of the hash-folded dep_path. Both built
        // from the primitives directly (`dep_path_to_filename` and
        // `hashed_dep_path`), NOT by re-calling the wrappers under test.
        let entry_recompute = dep_path_to_filename(dep_path, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH);
        let subdir_recompute = dep_path_to_filename(
            &hashes.hashed_dep_path(dep_path),
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        );
        assert_eq!(
            entry, entry_recompute,
            "entry name diverged from the bare-dep_path encode for {dep_path}"
        );
        assert_eq!(
            subdir, subdir_recompute,
            "hashed subdir diverged from the hash-folded encode for {dep_path}"
        );

        // And the hash genuinely makes the subdir differ from the entry
        // name, so the two values are not interchangeable — proving the
        // hoist must carry them separately. For short names the subdir
        // gains a visible `-<hex>` leaf suffix the entry lacks; for the
        // long peer-graph fixture the suffix is truncated off, but the
        // entry and subdir still differ because the BLAKE3 short-hash is
        // computed over different inputs (the hash-folded path vs. the
        // bare one), so the trailing hash diverges either way.
        assert_ne!(
            entry, subdir,
            "graph hash should make subdir differ from entry name for {dep_path}"
        );
    }
}

#[test]
fn test_link_all_handles_self_referential_dep_at_different_version() {
    // `react_ujs@3.3.0` (and other publish-script artifacts)
    // declares its own name as a dep at a *different* version
    // (`react_ujs: ^2.7.1`). The transitive-symlink pass would
    // try to create a symlink at `node_modules/react_ujs`,
    // which is exactly where the package's own files live —
    // EEXIST. Skip self-name deps regardless of version so
    // these install cleanly. `require('<self>')` from inside
    // the package then resolves to its own files, matching how
    // npm / pnpm / yarn end up after their hoisting passes.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));

    let mut indices = BTreeMap::new();
    let host_index_js = store.import_bytes(b"/* react_ujs 3.3.0 */", false).unwrap();
    let host_pkg_json = store
        .import_bytes(b"{\"name\":\"react_ujs\",\"version\":\"3.3.0\"}", false)
        .unwrap();
    let mut host_index = PackageIndex::default();
    host_index.insert("index.js".to_string(), host_index_js);
    host_index.insert("package.json".to_string(), host_pkg_json);
    indices.insert("react_ujs@3.3.0".to_string(), host_index);

    let mut host_deps = BTreeMap::new();
    // Self-reference at a different version, the shape that
    // triggered the EEXIST bug.
    host_deps.insert("react_ujs".to_string(), "^2.7.1".to_string());

    let mut packages = BTreeMap::new();
    packages.insert(
        "react_ujs@3.3.0".to_string(),
        LockedPackage {
            name: "react_ujs".to_string(),
            version: "3.3.0".to_string(),
            integrity: None,
            dependencies: host_deps,
            dep_path: "react_ujs@3.3.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "react_ujs".to_string(),
            dep_path: "react_ujs@3.3.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let stats = linker
        .link_all(&project_dir, &graph, &indices)
        .expect("install must succeed despite self-named dep");
    assert_eq!(stats.packages_linked, 1);
    let host_index =
        project_dir.join("node_modules/.aube/react_ujs@3.3.0/node_modules/react_ujs/index.js");
    assert!(host_index.exists(), "host package files must be present");
}

#[test]
fn test_link_all_creates_pnpm_virtual_store() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // .aube virtual store should exist
    assert!(project_dir.join("node_modules/.aube").exists());

    // .aube/foo@1.0.0 should be a symlink to the global virtual store
    let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
    assert!(aube_foo.symlink_metadata().unwrap().is_symlink());

    // foo@1.0.0 content should be accessible through the symlink
    let foo_in_pnpm = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_in_pnpm.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_in_pnpm).unwrap(),
        "module.exports = 'foo';"
    );

    // bar@2.0.0 should also be accessible
    let bar_in_pnpm = project_dir.join("node_modules/.aube/bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_in_pnpm.exists());

    assert_eq!(stats.packages_linked, 2);
    assert!(stats.files_linked >= 3); // foo has 2 files, bar has 1
}

#[test]
fn test_link_file_fresh_reports_missing_cas_shard_and_invalidates_cache() {
    // Reproduces jdx/aube#393: a partially corrupt CAS leaves the
    // cached package index pointing at a missing shard. Materialize
    // must distinguish "source CAS file missing" from a generic ENOENT
    // and drop the stale index JSON so the next install re-imports
    // the tarball.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    // Persist foo's index so invalidate_cached_index has something
    // to remove. Real installs save indices via the fetch path.
    let foo_index = indices.get("foo@1.0.0").unwrap();
    store.save_index("foo", "1.0.0", None, foo_index).unwrap();
    let cached_path = store.index_dir().join("foo@1.0.0.json");
    assert!(
        cached_path.exists(),
        "test setup: index cache must be written"
    );

    // Delete the CAS shard for foo's package.json (matches the
    // failure mode in #393 where one shard is missing while others
    // remain).
    let pkgjson_store_path = foo_index.get("package.json").unwrap().store_path.clone();
    std::fs::remove_file(&pkgjson_store_path).unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();
    let err = linker
        .link_all(&project_dir, &graph, &indices)
        .expect_err("link must fail when a referenced CAS shard is gone");
    assert!(
        matches!(&err, Error::MissingStoreFile { rel_path, .. } if rel_path == "package.json"),
        "expected MissingStoreFile {{ rel_path: \"package.json\" }}, got {err:?}"
    );

    // Side effect: cached index dropped, so the next install will
    // miss load_index and re-fetch instead of looping on the same
    // dead shard reference.
    assert!(
        !cached_path.exists(),
        "stale index cache must be invalidated on MissingStoreFile"
    );
}

#[test]
#[cfg(unix)]
fn test_link_file_fresh_hardlink_short_circuits_when_source_missing() {
    // Hardlink path used to silently fall through to `std::fs::copy`
    // on ENOENT and emit a misleading "hardlink failed, falling back
    // to copy" trace, even though the real cause was the source
    // shard going missing. Short-circuit returns MissingStoreFile
    // directly so traces stay accurate.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));
    let stored = store.import_bytes(b"hello", false).unwrap();
    // Capture the path before we move `stored` into link_file_fresh.
    let store_path = stored.store_path.clone();
    std::fs::remove_file(&store_path).unwrap();

    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("hello.txt");

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Hardlink, true);
    let err = linker
        .link_file_fresh(&stored, "hello.txt", &dst)
        .expect_err("source missing must fail");
    assert!(
        matches!(
            &err,
            Error::MissingStoreFile { store_path: p, rel_path } if p == &store_path && rel_path == "hello.txt"
        ),
        "expected MissingStoreFile from Hardlink branch, got {err:?}"
    );
}

/// RAII guard that serializes `FORCE_REFLINK_FAILURE` across the parallel
/// test runner and always restores the flag — even if the body panics.
///
/// `FORCE_REFLINK_FAILURE` is a process-wide `AtomicBool`, so two reflink
/// fallback tests running concurrently could otherwise observe each other's
/// `store(false)` mid-`link_file_fresh` (a reflink-capable FS would then take
/// the real clonefile path and skew the inode assertion), and a bare manual
/// reset would leak the `true` state to the next test on a panic. Holding the
/// `Mutex` for the whole forced-failure window mutually excludes the tests; the
/// `Drop` impl makes the reset panic-safe.
#[cfg(unix)]
struct ForcedReflinkFailure {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl ForcedReflinkFailure {
    fn engage() -> Self {
        use std::sync::atomic::Ordering;
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // Recover from a poisoned lock: a prior test panicking inside the guard
        // is exactly the case this exists for, and the `Drop` reset still ran.
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::materialize::FORCE_REFLINK_FAILURE.store(true, Ordering::Relaxed);
        Self { _guard: guard }
    }
}

#[cfg(unix)]
impl Drop for ForcedReflinkFailure {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        crate::materialize::FORCE_REFLINK_FAILURE.store(false, Ordering::Relaxed);
    }
}

/// Materialize one file under `strategy` with the reflink attempt forced
/// to fail, then report whether the result is a hardlink (same inode as
/// the source) or a copy (distinct inode). Isolates the clonefile-failure
/// fallback so the split between `ReflinkAuto` and explicit `Reflink` is
/// observable on any filesystem, including reflink-capable APFS/btrfs CI.
#[cfg(unix)]
fn realized_inode_matches_source_on_reflink_failure(strategy: LinkStrategy) -> bool {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));
    // >16 KiB so the macOS small-file copy shortcut does not pre-empt the
    // reflink path under test.
    let content = vec![b'x'; 32 * 1024];
    let stored = store.import_bytes(&content, false).unwrap();
    let store_path = stored.store_path.clone();

    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("payload.bin");

    let linker = Linker::new_with_gvs(&store, strategy, true);
    let result = {
        // Hold the guard across the materialize so a sibling test can't flip
        // the global flag mid-call; the flag is restored on drop, panic-safe.
        let _forced = ForcedReflinkFailure::engage();
        linker.link_file_fresh(&stored, "payload.bin", &dst)
    };
    result.expect("a reflink strategy must still materialize the file via its fallback");

    // Data integrity holds whichever fallback fired.
    assert_eq!(std::fs::read(&dst).unwrap(), content);

    std::fs::metadata(&store_path).unwrap().ino() == std::fs::metadata(&dst).unwrap().ino()
}

#[test]
#[cfg(unix)]
fn test_reflink_auto_falls_back_to_hardlink_not_copy() {
    // `auto` on a same-FS macOS target resolves to `ReflinkAuto`; the
    // probe already proved the target shares a mount, so a clonefile
    // failure (non-APFS same-FS volume, e.g. HFS+) must degrade to a
    // zero-cost hardlink before a per-file copy.
    assert!(
        realized_inode_matches_source_on_reflink_failure(LinkStrategy::ReflinkAuto),
        "ReflinkAuto must fall back to a hardlink (same inode), not a copy, on reflink failure"
    );
}

#[test]
#[cfg(unix)]
fn test_explicit_reflink_falls_back_to_copy_not_hardlink() {
    // Explicit `clone` / `clone-or-copy` map to `Reflink`, whose
    // documented contract is reflink with a plain *copy* fallback. They
    // must NOT take the auto-only hardlink step on a clonefile failure —
    // the result is a distinct inode (copy), never the source's inode.
    assert!(
        !realized_inode_matches_source_on_reflink_failure(LinkStrategy::Reflink),
        "explicit Reflink must fall back to a copy (distinct inode), not a hardlink"
    );
}

#[test]
fn test_link_all_creates_top_level_entries() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Top-level foo/ should exist (it's a direct dep)
    let foo_top = project_dir.join("node_modules/foo/index.js");
    assert!(foo_top.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_top).unwrap(),
        "module.exports = 'foo';"
    );

    // bar should NOT be top-level (it's only a transitive dep)
    let bar_top = project_dir.join("node_modules/bar/index.js");
    assert!(!bar_top.exists());

    assert_eq!(stats.top_level_linked, 1);
}

#[test]
fn test_link_all_transitive_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo's node_modules/bar should be a symlink (inside the global virtual store)
    // The path resolves through the .aube symlink into the global store
    let bar_symlink = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_symlink.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_link_all_cleans_existing_node_modules() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    let nm = project_dir.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("stale-file.txt"), "old").unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Old file should be gone
    assert!(!nm.join("stale-file.txt").exists());
    // New structure should exist
    assert!(nm.join(".aube").exists());
}

#[test]
fn test_link_all_nested_node_modules_for_direct_deps() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo is a direct dep with bar as a transitive dep.
    // The top-level node_modules/foo is a symlink to .aube/foo@1.0.0/node_modules/foo,
    // and bar lives as a sibling at .aube/foo@1.0.0/node_modules/bar (also a symlink
    // pointing to .aube/bar@2.0.0/node_modules/bar). Node's directory walk from inside
    // foo finds bar this way without aube creating any nested node_modules.
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    let bar_sibling = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_sibling.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_global_virtual_store_is_populated() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Global virtual store should contain materialized packages
    let foo_global = virtual_store.join("foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_global.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_global).unwrap(),
        "module.exports = 'foo';"
    );

    let bar_global = virtual_store.join("bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_global.exists());
}

#[test]
fn test_global_virtual_store_gets_hidden_hoist() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let mut graph = make_graph();
    graph
        .packages
        .get_mut("foo@1.0.0")
        .unwrap()
        .dependencies
        .clear();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    let project_hidden = project_dir.join("node_modules/.aube/node_modules/bar");
    assert!(project_hidden.symlink_metadata().unwrap().is_symlink());

    let global_hidden = virtual_store.join("node_modules/bar");
    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

    let from_real_store = virtual_store.join("foo@1.0.0/node_modules/bar/index.js");
    assert!(
        !from_real_store.exists(),
        "bar is not a declared sibling of foo in this fixture"
    );
    let fallback = virtual_store.join("node_modules/bar/index.js");
    assert_eq!(
        std::fs::read_to_string(fallback).unwrap(),
        "module.exports = 'bar';"
    );
}

#[test]
fn test_global_virtual_store_hidden_hoist_prunes_only_dead_entries() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let hidden = virtual_store.join("node_modules");
    std::fs::create_dir_all(&hidden).unwrap();
    let dotfile = hidden.join(".sentinel");
    std::fs::write(&dotfile, "shared").unwrap();
    let stale = hidden.join("stale");
    std::fs::write(&stale, "old").unwrap();
    let stale_scope = hidden.join("@stale-scope");
    std::fs::write(&stale_scope, "old").unwrap();
    let external_target = virtual_store.join("external@1.0.0/node_modules/external");
    std::fs::create_dir_all(&external_target).unwrap();
    let external_link = hidden.join("external");
    sys::create_dir_link(
        &pathdiff::diff_paths(&external_target, &hidden).unwrap(),
        &external_link,
    )
    .unwrap();
    let dead_link = hidden.join("dead");
    sys::create_dir_link(
        Path::new("../missing@1.0.0/node_modules/missing"),
        &dead_link,
    )
    .unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert_eq!(std::fs::read_to_string(dotfile).unwrap(), "shared");
    assert!(!stale.exists());
    assert!(stale_scope.symlink_metadata().is_err());
    assert!(external_link.symlink_metadata().unwrap().is_symlink());
    assert!(dead_link.symlink_metadata().is_err());
    assert!(hidden.join("bar").symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_global_virtual_store_hidden_hoist_disabled_keeps_live_shared_links() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    let global_hidden = virtual_store.join("node_modules/bar");
    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

    Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
        .with_hoist(false)
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_second_install_reuses_global_store() {
    let dir = tempfile::tempdir().unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    // First install
    let project1 = dir.path().join("project1");
    std::fs::create_dir_all(&project1).unwrap();
    let stats1 = linker.link_all(&project1, &graph, &indices).unwrap();
    assert_eq!(stats1.packages_linked, 2);
    assert_eq!(stats1.packages_cached, 0);

    // Second install with same deps — should reuse global virtual store
    let project2 = dir.path().join("project2");
    std::fs::create_dir_all(&project2).unwrap();
    let stats2 = linker.link_all(&project2, &graph, &indices).unwrap();
    assert_eq!(stats2.packages_linked, 0);
    assert_eq!(stats2.packages_cached, 2);
    assert_eq!(stats2.files_linked, 0); // no CAS linking needed

    // Both projects should work
    let foo1 = project1.join("node_modules/foo/index.js");
    let foo2 = project2.join("node_modules/foo/index.js");
    assert!(foo1.exists());
    assert!(foo2.exists());
    assert_eq!(
        std::fs::read_to_string(&foo1).unwrap(),
        std::fs::read_to_string(&foo2).unwrap()
    );
}

#[test]
fn gvs_shareable_source_dep_without_index_errors_loudly() {
    // A git / remote-tarball dep is keyed in the GVS under a
    // content-addressed path whose hash folds in the dep's content
    // fingerprint. That fingerprint comes from its package index, so
    // under the global virtual store the dep MUST have an index — a
    // missing one would otherwise leave the dep unmaterialized and
    // dangle every dependent's sibling symlink. The loop used to
    // silently `continue`; assert it now fails loudly with a
    // `MissingPackageIndex` diagnostic instead (the registry pass
    // already does, but git/tarball deps have no `load_index` fallback
    // because their indices aren't persisted by coordinate).
    use aube_lockfile::{GitSource, LocalSource};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));

    let git = LocalSource::Git(GitSource {
        url: "https://github.com/request/request.git".to_string(),
        committish: None,
        resolved: "0123456789abcdef0123456789abcdef01234567".to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = git.dep_path("request");

    let mut packages = BTreeMap::new();
    packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "request".to_string(),
            version: "2.88.0".to_string(),
            integrity: None,
            dependencies: BTreeMap::new(),
            dep_path: dep_path.clone(),
            local_source: Some(git),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "request".to_string(),
            dep_path: dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    // Deliberately omit `dep_path` from the indices map — the contract
    // violation the fetch driver normally prevents.
    let indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let err = linker
        .link_all(&project_dir, &graph, &indices)
        .expect_err("a shareable source dep with no index must error, not dangle");
    assert!(
        matches!(err, Error::MissingPackageIndex(ref dp) if dp == &dep_path),
        "expected MissingPackageIndex({dep_path}), got: {err:?}"
    );
}

/// Regression: a version bump keeps the same top-level name
/// (`foo`) but must repoint `node_modules/foo` at the new
/// `.aube/foo@<new>` entry. The old `.aube/foo@<old>/` is left
/// on disk (no one sweeps the virtual store by name), so a
/// plain `path.exists()` check would see a still-resolving
/// stale symlink and keep it. The target-aware
/// `reconcile_top_level_link` compares the expected target
/// string and rewrites the link.
#[test]
fn test_link_all_repoints_symlink_after_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: foo@1.0.0 as the root's direct dep.
    let mut indices_v1 = BTreeMap::new();
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_index = PackageIndex::default();
    foo_v1_index.insert("index.js".to_string(), foo_v1);
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_index);

    let mut graph_v1 = LockfileGraph::default();
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    assert_eq!(
        std::fs::read_to_string(foo_link.join("index.js")).unwrap(),
        "module.exports = 'foo@1';"
    );

    // Install 2: foo upgraded to 2.0.0. The `.aube/foo@1.0.0/`
    // tree stays on disk (nothing prunes the virtual store by
    // name), so the old `node_modules/foo` symlink still
    // resolves — a naive "does the target exist?" check would
    // keep it.
    let mut indices_v2 = BTreeMap::new();
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_index = PackageIndex::default();
    foo_v2_index.insert("index.js".to_string(), foo_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_index);

    let mut graph_v2 = LockfileGraph::default();
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();

    // The top-level symlink must now resolve to foo@2.0.0's
    // bytes, not foo@1.0.0's.
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';"
    );
}

/// Regression: `shamefully_hoist` hoists transitive deps to the
/// top-level `node_modules/<name>`. When the hoisted version
/// changes between installs (transitive bump), the previous
/// implementation kept the stale symlink because
/// `keep_or_reclaim_broken_symlink` only checked "does target
/// resolve?" and the old `.aube/<old-dep-path>/` was still on
/// disk. `reconcile_top_level_link` + the explicit
/// direct-dep/claimed tracking in `hoist_remaining_into` together
/// fix this.
#[test]
fn test_shamefully_hoist_repoints_after_transitive_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: root → bar@1.0.0 → foo@1.0.0 (transitive).
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_idx = PackageIndex::default();
    foo_v1_idx.insert("index.js".to_string(), foo_v1);
    let bar_v1 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx = PackageIndex::default();
    bar_v1_idx.insert("index.js".to_string(), bar_v1);
    let mut indices_v1 = BTreeMap::new();
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_idx);
    indices_v1.insert("bar@1.0.0".to_string(), bar_v1_idx);

    let mut graph_v1 = LockfileGraph::default();
    let mut bar_deps_v1 = BTreeMap::new();
    bar_deps_v1.insert("foo".to_string(), "1.0.0".to_string());
    graph_v1.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v1,
            ..Default::default()
        },
    );
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy).with_shamefully_hoist(true);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@1';",
        "install 1 should hoist foo@1.0.0"
    );

    // Install 2: bar@1.0.0 → foo@2.0.0 (transitive bump). The
    // stale `.aube/foo@1.0.0/` tree is still on disk (nothing
    // sweeps the virtual store by name), so the old hoisted
    // symlink would still resolve — the old `exists?` check
    // would silently keep it.
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_idx = PackageIndex::default();
    foo_v2_idx.insert("index.js".to_string(), foo_v2);
    let mut indices_v2 = BTreeMap::new();
    // Reuse bar's materialized index from v1.
    let bar_v1_for_v2 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx_v2 = PackageIndex::default();
    bar_v1_idx_v2.insert("index.js".to_string(), bar_v1_for_v2);
    indices_v2.insert("bar@1.0.0".to_string(), bar_v1_idx_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_idx);

    let mut graph_v2 = LockfileGraph::default();
    let mut bar_deps_v2 = BTreeMap::new();
    bar_deps_v2.insert("foo".to_string(), "2.0.0".to_string());
    graph_v2.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v2,
            ..Default::default()
        },
    );
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';",
        "install 2 should repoint the hoisted symlink to foo@2.0.0"
    );
}

// ---------------------------------------------------------------
// `validate_index_key` rejects every shape of index key that
// would make `base.join(key)` escape `base`. Primary defence is
// in `aube-store::import_tarball`; this is the last-chance guard
// before the linker actually writes to disk.
// ---------------------------------------------------------------

#[test]
fn validate_index_key_accepts_normal_keys() {
    validate_index_key("index.js").unwrap();
    validate_index_key("lib/sub/a.js").unwrap();
    validate_index_key("package.json").unwrap();
    validate_index_key("a/b/c/d/e/f.js").unwrap();
}

#[cfg(not(windows))]
#[test]
fn validate_index_key_accepts_posix_colon_filename() {
    validate_index_key("dist/__mocks__/package-json:version.d.ts").unwrap();
}

#[test]
fn validate_index_key_rejects_empty() {
    assert!(matches!(
        validate_index_key(""),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_leading_slash() {
    assert!(matches!(
        validate_index_key("/etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("\\evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_parent_dir() {
    assert!(matches!(
        validate_index_key("../../etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib/../../../etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_nul_and_backslash() {
    assert!(matches!(
        validate_index_key("lib\0evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib\\..\\etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[cfg(windows)]
#[test]
fn validate_index_key_rejects_windows_drive() {
    assert!(matches!(
        validate_index_key("C:Windows"),
        Err(Error::UnsafeIndexKey(_))
    ));
}
