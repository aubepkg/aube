//! `aube store` — inspect and manage the global content-addressable store.
//!
//! Mirrors `pnpm store`:
//!
//! - `aube store path` — print the store-version directory (aube-owned
//!   by default: `$XDG_DATA_HOME/aube/store/v1/`, falling back to
//!   `~/.local/share/aube/store/v1/`). This directory contains both
//!   `files/` (the CAS shards) and `index/` (the cached package
//!   indexes), so a single backup or Docker BuildKit cache mount of
//!   this path captures the whole store — matching `pnpm store path`'s
//!   granularity (which prints e.g. `~/.pnpm-store/v11/`).
//! - `aube store add <pkg>…` — resolve each spec against the registry, fetch
//!   the tarball, and import it into the global CAS. Pre-warms the store
//!   without touching any project's `node_modules/`.
//! - `aube store prune` — mark global virtual-store entries reachable from
//!   registered projects, remove the rest, then remove unreferenced CAS files.
//!   CAS pruning uses hardlink counts where available and cached package
//!   indexes on reflink filesystems. `--dry-run` reports the same totals
//!   without deleting anything.
//! - `aube store status` — verify every file referenced by a cached package
//!   index still exists in the store and its BLAKE3 hash matches. Exits 0
//!   when everything is consistent, 1 when any corruption is found.
//!
//! None of these subcommands touch `node_modules/`, the lockfile, or the
//! project manifest, so they deliberately skip the project lock and the
//! auto-install check.

use crate::commands::{make_client, packument_full_cache_dir, resolve_version, split_name_spec};
use clap::{Arg, ArgAction, ArgMatches, Args, Command, Error, FromArgMatches, Subcommand};
use miette::{IntoDiagnostic, miette};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Args)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommand,
}

#[derive(Debug, Subcommand)]
pub enum StoreCommand {
    /// Add one or more packages to the global store without linking them
    /// into any project.
    ///
    /// Each argument is a package spec: `lodash`, `lodash@4.17.21`,
    /// `react@next`, or `express@^4`.
    Add {
        /// Package specs to fetch into the store.
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Show the store path.
    Path,
    /// Remove unreferenced packages from the global store.
    ///
    /// Operates on the store printed by `aube store path`; it does not touch
    /// project node_modules directories, manifests, or lockfiles.
    ///
    /// It removes global virtual-store graph entries not referenced by any
    /// registered project. Entries from older aube releases live outside the
    /// registry-managed versioned namespace and are not touched. It then prunes
    /// content-store files.
    ///
    /// On reflink filesystems such as APFS or btrfs, link counts cannot prove
    /// project reachability, so content-store pruning relies on cached package
    /// indexes. Global virtual-store reachability comes from project links.
    Prune(PruneArgs),
    /// Verify the store against cached package indexes.
    ///
    /// Confirms every file referenced by a cached package index is
    /// still present in the store and that its BLAKE3 hash matches.
    /// Exits non-zero when any corruption is detected.
    Status,
}

// `PruneArgs` is part of the published Rust API, so keep its original
// constructible shape while exposing JSON as CLI-only state.
static PRUNE_JSON_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct PruneArgs {
    /// Do not actually delete anything; report what would be pruned.
    pub dry_run: bool,
}

impl FromArgMatches for PruneArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        PRUNE_JSON_REQUESTED.store(matches.get_flag("json"), Ordering::Relaxed);
        Ok(Self {
            dry_run: matches.get_flag("dry_run"),
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        PRUNE_JSON_REQUESTED.store(matches.get_flag("json"), Ordering::Relaxed);
        self.dry_run = matches.get_flag("dry_run");
        Ok(())
    }
}

impl Args for PruneArgs {
    fn augment_args(command: Command) -> Command {
        command
            .arg(
                Arg::new("dry_run")
                    .long("dry-run")
                    .action(ArgAction::SetTrue)
                    .help("Do not actually delete anything; report what would be pruned"),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .requires("dry_run")
                    .help(
                        "Emit the dry-run plan as one machine-readable JSON document (requires --dry-run)",
                    ),
            )
    }

    fn augment_args_for_update(command: Command) -> Command {
        Self::augment_args(command)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PruneReport {
    schema_version: u32,
    dry_run: bool,
    mutation_roots: Vec<MutationRoot>,
    actions: Vec<PlannedAction>,
    global_virtual_store: GvsStats,
    content_store: CasStats,
    reclaimable_bytes_upper_bound: u64,
    warnings: Vec<StructuredWarning>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationRoot {
    kind: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedAction {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GvsStats {
    entries: usize,
    bytes_upper_bound: u64,
    stale_project_records: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CasStats {
    files: usize,
    bytes_upper_bound: u64,
}

#[derive(Debug, Serialize)]
struct StructuredWarning {
    code: &'static str,
    message: String,
}

#[derive(Debug, Default)]
struct CasPrunePlan {
    paths: Vec<std::path::PathBuf>,
    files: Vec<super::gvs_registry::CandidateFile>,
}

pub async fn run(args: StoreArgs) -> miette::Result<()> {
    match args.command {
        StoreCommand::Add { packages } => add(packages).await,
        StoreCommand::Path => path(),
        StoreCommand::Prune(a) => prune(a),
        StoreCommand::Status => status(),
    }
}

fn open_store() -> miette::Result<aube_store::Store> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::commands::open_store(&cwd)
}

fn open_store_for_maintenance() -> miette::Result<aube_store::Store> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::commands::open_store_for_maintenance(&cwd)
}

fn path() -> miette::Result<()> {
    let store = open_store_for_maintenance()?;
    println!("{}", store.store_v1_dir().display());
    Ok(())
}

async fn add(specs: Vec<String>) -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);
    let store = crate::commands::open_store(&cwd)?;

    let mut added = 0usize;
    for spec in &specs {
        let (name, version_spec) = split_name_spec(spec);
        let packument = client
            .fetch_packument_full_cached(name, &packument_full_cache_dir())
            .await
            .map_err(|e| match e {
                aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
                other => miette!("failed to fetch {name}: {other}"),
            })?;

        let version = resolve_version(&packument, version_spec).ok_or_else(|| {
            miette!(
                "no matching version for {name}@{}",
                version_spec.unwrap_or("latest")
            )
        })?;

        let tarball_url = packument
            .get("versions")
            .and_then(|v| v.get(&version))
            .and_then(|v| v.get("dist"))
            .and_then(|d| d.get("tarball"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .unwrap_or_else(|| client.tarball_url(name, &version));
        let integrity = packument
            .get("versions")
            .and_then(|v| v.get(&version))
            .and_then(|v| v.get("dist"))
            .and_then(|d| d.get("integrity"))
            .and_then(|i| i.as_str())
            .map(String::from);

        let bytes = client
            .fetch_tarball_bytes(&tarball_url)
            .await
            .map_err(|e| miette!("failed to fetch {name}@{version}: {e}"))?;

        if let Some(expected) = integrity.as_deref() {
            aube_store::verify_integrity(&bytes, expected)
                .map_err(|e| miette!("{name}@{version}: {e}"))?;
        }

        let index = store
            .import_tarball(&bytes)
            .map_err(|e| miette!("failed to import {name}@{version}: {e}"))?;
        // When the packument shipped a `dist.integrity`, the cache
        // filename carries a `+<hex>` suffix that discriminates
        // same-(name, version) tarballs from different sources.
        // Otherwise we fall back to the plain key (proxies that strip
        // integrity still get a warm cache).
        if let Err(e) = store.save_index(name, &version, integrity.as_deref(), &index) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
                "failed to cache index for {name}@{version}: {e}"
            );
        }

        println!("+ {name}@{version}");
        added += 1;
    }

    eprintln!(
        "Added {} to the store",
        pluralizer::pluralize("package", added as isize, true)
    );
    Ok(())
}

/// Collect the set of hex hashes referenced by every cached package index.
/// Pruning must fail closed if this scan is incomplete: a skipped index would
/// otherwise make its live CAS files look unreferenced.
fn referenced_hashes(index_dir: &Path) -> miette::Result<std::collections::HashSet<String>> {
    let mut seen = std::collections::HashSet::new();
    visit_cached_indices_at(index_dir, |_, index| {
        for stored in index.values() {
            seen.insert(stored.hex_hash.clone());
        }
    })?;
    Ok(seen)
}

/// Visit every JSON index at the root and in integrity-keyed subdirectories.
/// A missing index root is an empty cache; every other scan failure is fatal.
fn visit_cached_indices(
    store: &aube_store::Store,
    visit: impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    visit_cached_indices_at(&store.index_dir(), visit)
}

fn visit_cached_indices_at(
    index_dir: &Path,
    mut visit: impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    if !index_dir.try_exists().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
            "failed to inspect store index directory {}: {e}",
            index_dir.display()
        )
    })? {
        return Ok(());
    }
    visit_indices_in_dir(index_dir, true, &mut visit)
}

fn visit_indices_in_dir(
    dir: &Path,
    visit_subdirs: bool,
    visit: &mut impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
            "failed to list store index directory {}: {e}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to read an entry in store index directory {}: {e}",
                dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to inspect store index path {}: {e}",
                path.display()
            )
        })?;
        if metadata.is_dir() {
            if visit_subdirs {
                visit_indices_in_dir(&path, false, visit)?;
            }
            continue;
        }
        if !metadata.is_file() || path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let content = std::fs::read(&path).map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to read store index {}: {e}",
                path.display()
            )
        })?;
        let index = serde_json::from_slice(&content).map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
                "failed to parse store index {}: {e}",
                path.display()
            )
        })?;
        visit(&path, index);
    }
    Ok(())
}

fn prune(args: PruneArgs) -> miette::Result<()> {
    let json = PRUNE_JSON_REQUESTED.swap(false, Ordering::Relaxed);
    let store = open_store_for_maintenance()?;
    let maintenance_lock = store
        .lock_for_maintenance()
        .into_diagnostic()
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_STORE_PRUNE_LOCK_FAILED,
                "failed to lock the store for pruning: {e}"
            )
        })?;
    let _gvs_lock = super::gvs_registry::lock_for_prune(&store.virtual_store_dir(), json)?;
    let gvs_plan = super::gvs_registry::plan_prune(&store.virtual_store_dir())?;
    let current_index_dir = store.index_dir();
    let legacy_index_dir = store.legacy_index_dir();
    let mut referenced = referenced_hashes(&current_index_dir)?;
    if legacy_index_dir != current_index_dir {
        referenced.extend(referenced_hashes(&legacy_index_dir)?);
    }
    let cas_plan = plan_cas_prune(store.root(), &referenced, &gvs_plan)?;
    let report = build_prune_report(&store, &gvs_plan, &cas_plan);

    if json {
        let output = serde_json::to_string_pretty(&report).into_diagnostic()?;
        println!("{output}");
        return Ok(());
    }

    if !args.dry_run {
        if store.legacy_index_migration_needed() {
            store.migrate_legacy_index_for_maintenance(&maintenance_lock);
        }
        super::gvs_registry::apply_prune(&store.virtual_store_dir(), &gvs_plan)?;
        for path in &cas_plan.paths {
            std::fs::remove_file(path).map_err(|e| {
                miette!(
                    code = aube_codes::errors::ERR_AUBE_STORE_PRUNE_FAILED,
                    "failed to prune store file {}: {e}",
                    path.display()
                )
            })?;
        }
    }

    let verb = if args.dry_run {
        "Would prune"
    } else {
        "Pruned"
    };
    if !gvs_plan.entries.is_empty() {
        eprintln!(
            "{verb} {} ({:.1} MB) from the global virtual store",
            pluralizer::pluralize("package", gvs_plan.entries.len() as isize, true),
            gvs_plan.bytes() as f64 / 1_048_576.0
        );
    }
    if !gvs_plan.stale_records.is_empty() {
        eprintln!(
            "{verb} {} from the global virtual store registry",
            pluralizer::pluralize(
                "stale project record",
                gvs_plan.stale_records.len() as isize,
                true
            )
        );
    }
    if !cas_plan.files.is_empty() {
        let size_prefix = if args.dry_run { "up to " } else { "" };
        eprintln!(
            "{verb} {} ({size_prefix}{:.1} MB) from the store",
            pluralizer::pluralize("file", cas_plan.files.len() as isize, true),
            candidate_bytes(&cas_plan.files) as f64 / 1_048_576.0
        );
    }
    if gvs_plan.entries.is_empty() && gvs_plan.stale_records.is_empty() && cas_plan.files.is_empty()
    {
        eprintln!("Nothing to prune");
    }
    for path in &gvs_plan.vanished_files {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
            path = %path.display(),
            "global virtual-store file disappeared while building the prune plan"
        );
    }
    Ok(())
}

fn plan_cas_prune(
    root: &Path,
    referenced: &HashSet<String>,
    gvs_plan: &super::gvs_registry::GvsPrunePlan,
) -> miette::Result<CasPrunePlan> {
    if !root.try_exists().into_diagnostic()? {
        return Ok(CasPrunePlan::default());
    }
    let mut removed_gvs_links: HashMap<super::gvs_registry::FileIdentity, u64> = HashMap::new();
    for file in &gvs_plan.files {
        *removed_gvs_links.entry(file.identity.clone()).or_default() += 1;
    }
    let mut plan = CasPrunePlan::default();
    let mut content_paths = HashSet::new();
    let mut markers = Vec::new();
    let root_entries = read_dir_complete(root)?;
    for entry in &root_entries {
        let path = entry.path();
        let is_stream_temp = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".aube-stream-"));
        if !is_stream_temp {
            continue;
        }
        let metadata = entry.metadata().into_diagnostic()?;
        if metadata.is_file() {
            plan.paths.push(path.clone());
            plan.files.push(super::gvs_registry::CandidateFile {
                identity: candidate_identity(&path, &metadata),
                bytes: metadata.len(),
            });
        }
    }
    // Walk every 2-char shard directory. Store layout is
    // <root>/<shard>/<rest-of-hash>[-exec].
    for shard in root_entries {
        let shard_path = shard.path();
        if !shard.file_type().into_diagnostic()?.is_dir() {
            continue;
        }
        let Some(shard_name) = shard_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if shard_name.len() != 2 {
            continue;
        }
        for file in read_dir_complete(&shard_path)? {
            let file_path = file.path();
            let metadata = file.metadata().into_diagnostic()?;
            if !metadata.is_file() {
                continue;
            }
            let Some(fname) = file_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(base) = fname.strip_suffix("-exec") {
                let content_path = shard_path.join(base);
                markers.push((file_path, content_path));
                continue;
            }
            let hex = format!("{shard_name}{fname}");
            if referenced.contains(&hex) {
                continue;
            }
            let identity = candidate_identity(&file_path, &metadata);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let removed_links = removed_gvs_links.get(&identity).copied().unwrap_or(0);
                if metadata.nlink() > removed_links + 1 {
                    continue;
                }
            }
            content_paths.insert(file_path.clone());
            plan.paths.push(file_path);
            plan.files.push(super::gvs_registry::CandidateFile {
                identity,
                bytes: metadata.len(),
            });
        }
    }
    for (marker, content) in markers {
        if content_paths.contains(&content) {
            plan.paths.push(marker);
        }
    }
    Ok(plan)
}

fn candidate_identity(
    _path: &Path,
    metadata: &std::fs::Metadata,
) -> super::gvs_registry::FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        super::gvs_registry::FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        super::gvs_registry::FileIdentity::Path(_path.to_path_buf())
    }
}

fn read_dir_complete(path: &Path) -> miette::Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(path)
        .into_diagnostic()?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
}

fn build_prune_report(
    store: &aube_store::Store,
    gvs_plan: &super::gvs_registry::GvsPrunePlan,
    cas_plan: &CasPrunePlan,
) -> PruneReport {
    let mut mutation_roots = vec![
        mutation_root("store", store.store_v1_dir()),
        mutation_root("contentStore", store.root().to_path_buf()),
        mutation_root("packageIndex", store.index_dir()),
        mutation_root("globalVirtualStore", store.virtual_store_dir()),
        mutation_root(
            "projectRegistry",
            store
                .virtual_store_dir()
                .join(super::gvs_registry::PROJECTS_DIR),
        ),
        mutation_root("maintenanceLock", store.maintenance_lock_path()),
        mutation_root(
            "globalVirtualStoreLock",
            store
                .virtual_store_dir()
                .join(super::gvs_registry::LOCK_FILE),
        ),
    ];
    let mut actions = Vec::new();
    if store.legacy_index_migration_needed() {
        mutation_roots.push(mutation_root(
            "legacyPackageIndex",
            store.legacy_index_dir(),
        ));
        actions.push(PlannedAction {
            kind: "migrateLegacyPackageIndex",
            from: Some(json_path(store.legacy_index_dir())),
            to: Some(json_path(store.index_dir())),
            count: 1,
        });
    }
    actions.extend([
        PlannedAction {
            kind: "pruneGlobalVirtualStoreEntries",
            from: None,
            to: None,
            count: gvs_plan.entries.len(),
        },
        PlannedAction {
            kind: "removeStaleProjectRecords",
            from: None,
            to: None,
            count: gvs_plan.stale_records.len(),
        },
        PlannedAction {
            kind: "pruneContentStoreFiles",
            from: None,
            to: None,
            count: cas_plan.files.len(),
        },
    ]);
    let mut unique = HashMap::new();
    for file in gvs_plan.files.iter().chain(&cas_plan.files) {
        unique.entry(file.identity.clone()).or_insert(file.bytes);
    }
    PruneReport {
        schema_version: 1,
        dry_run: true,
        mutation_roots,
        actions,
        global_virtual_store: GvsStats {
            entries: gvs_plan.entries.len(),
            bytes_upper_bound: gvs_plan.bytes(),
            stale_project_records: gvs_plan.stale_records.len(),
        },
        content_store: CasStats {
            files: cas_plan.files.len(),
            bytes_upper_bound: candidate_bytes(&cas_plan.files),
        },
        reclaimable_bytes_upper_bound: unique.into_values().sum(),
        warnings: gvs_plan
            .vanished_files
            .iter()
            .map(|path| StructuredWarning {
                code: aube_codes::warnings::WARN_AUBE_STORE_PRUNE_ENTRY_DISAPPEARED,
                message: format!(
                    "global virtual-store file {} disappeared while building the prune plan",
                    path.display()
                ),
            })
            .collect(),
    }
}

fn candidate_bytes(files: &[super::gvs_registry::CandidateFile]) -> u64 {
    let mut identities = HashSet::new();
    files
        .iter()
        .filter(|file| identities.insert(file.identity.clone()))
        .map(|file| file.bytes)
        .sum()
}

fn mutation_root(kind: &'static str, path: std::path::PathBuf) -> MutationRoot {
    let resolved = resolve_physical_path(&path);
    let resolved_path = resolved.filter(|resolved| resolved != &path).map(json_path);
    MutationRoot {
        kind,
        path: json_path(path),
        resolved_path,
    }
}

fn resolve_physical_path(path: &Path) -> Option<std::path::PathBuf> {
    let mut existing = path;
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
    let mut resolved = std::fs::canonicalize(existing).ok()?;
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

fn json_path(path: std::path::PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

fn status() -> miette::Result<()> {
    let store = open_store()?;
    let mut checked = 0usize;
    let mut broken: Vec<String> = Vec::new();
    visit_cached_indices(&store, |path, index| {
        checked += 1;
        let pkg_label = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace("__", "/"))
            .unwrap_or_else(|| path.display().to_string());
        let mut pkg_ok = true;
        for (rel, stored) in &index {
            if !verify_stored_file(&stored.store_path, &stored.hex_hash) {
                broken.push(format!("{pkg_label}: {rel}"));
                pkg_ok = false;
            }
        }
        if pkg_ok {
            tracing::debug!("store ok: {pkg_label}");
        }
    })?;

    if checked == 0 {
        eprintln!("Store is consistent (no cached indices found)");
        return Ok(());
    }

    if broken.is_empty() {
        eprintln!(
            "Store is consistent: {} verified",
            pluralizer::pluralize("package", checked as isize, true)
        );
        Ok(())
    } else {
        // Corruption lines go to stdout so operators can pipe them into
        // `wc -l`, `grep`, etc. while the summary/failure goes to stderr
        // via miette. Mirrors how `store add` emits data on stdout.
        for line in &broken {
            println!("corrupt: {line}");
        }
        Err(miette!(
            "store contains {} corrupted {}",
            broken.len(),
            pluralizer::pluralize("file", broken.len() as isize, false)
        ))
    }
}

/// Stream the file at `path` through BLAKE3 and compare to the expected
/// hex digest. Missing files count as a mismatch.
fn verify_stored_file(path: &Path, expected_hex: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if std::io::copy(&mut f, &mut hasher).is_err() {
        return false;
    }
    let actual = hasher.finalize().to_hex().to_string();
    actual == expected_hex
}
