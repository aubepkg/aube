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
use clap::{Args, Subcommand};
use miette::{IntoDiagnostic, miette};
use std::path::Path;

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

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Do not actually delete anything; report what would be pruned.
    #[arg(long)]
    pub dry_run: bool,
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

fn path() -> miette::Result<()> {
    let store = open_store()?;
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
fn referenced_hashes(
    store: &aube_store::Store,
) -> miette::Result<std::collections::HashSet<String>> {
    let mut seen = std::collections::HashSet::new();
    visit_cached_indices(store, |_, index| {
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
    mut visit: impl FnMut(&Path, aube_store::PackageIndex),
) -> miette::Result<()> {
    let index_dir = store.index_dir();
    if !index_dir.try_exists().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_STORE_INDEX_SCAN_FAILED,
            "failed to inspect store index directory {}: {e}",
            index_dir.display()
        )
    })? {
        return Ok(());
    }
    visit_indices_in_dir(&index_dir, true, &mut visit)
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
    let store = open_store()?;
    let removed_gvs = super::gvs_registry::prune(&store.virtual_store_dir(), args.dry_run)?;
    if removed_gvs > 0 {
        let gvs_verb = if args.dry_run {
            "Would prune"
        } else {
            "Pruned"
        };
        eprintln!(
            "{gvs_verb} {} from the global virtual store",
            pluralizer::pluralize("package", removed_gvs as isize, true)
        );
    }
    let root = store.root().to_path_buf();
    if !root.exists() {
        eprintln!("Store is empty: nothing to prune");
        return Ok(());
    }

    let referenced = referenced_hashes(&store)?;
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;

    // Walk every 2-char shard directory. Store layout is
    // <root>/<shard>/<rest-of-hash>[-exec].
    for shard in std::fs::read_dir(&root).into_diagnostic()?.flatten() {
        let shard_path = shard.path();
        if !shard_path.is_dir() {
            continue;
        }
        let shard_name = match shard_path.file_name().and_then(|s| s.to_str()) {
            Some(s) if s.len() == 2 => s.to_string(),
            _ => continue,
        };
        for file in std::fs::read_dir(&shard_path).into_diagnostic()?.flatten() {
            let file_path = file.path();
            let Some(fname) = file_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Skip the `-exec` marker; it gets removed alongside its target.
            let is_exec_marker = fname.ends_with("-exec");
            let base = fname.strip_suffix("-exec").unwrap_or(fname);
            let hex = format!("{shard_name}{base}");

            if referenced.contains(&hex) {
                continue;
            }

            // On hardlink filesystems, files with nlink > 1 are referenced
            // by at least one virtual-store entry — don't touch them. Exec
            // markers are never hardlinked, so we can't check them directly;
            // instead we delete a marker only when its companion content
            // file is *also* going away, otherwise we'd silently strip the
            // executable bit from a file pnpm still references.
            let content_len = match file.metadata() {
                Ok(meta) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if is_exec_marker {
                            let content_path = shard_path.join(base);
                            if let Ok(content_meta) = std::fs::metadata(&content_path)
                                && content_meta.nlink() > 1
                            {
                                continue;
                            }
                        } else if meta.nlink() > 1 {
                            continue;
                        }
                    }
                    meta.len()
                }
                Err(_) => 0,
            };

            // Only credit the byte counter after the unlink actually
            // succeeds, otherwise a permission-denied failure would
            // inflate the "freed" number in the summary. A dry run has no
            // unlink to check, so it credits every candidate and its total
            // is an upper bound — hence the "up to" in the summary below.
            let unlinked = args.dry_run || std::fs::remove_file(&file_path).is_ok();
            if unlinked && !is_exec_marker {
                removed_files += 1;
                removed_bytes += content_len;
            }
        }
    }

    let (verb, size_prefix) = if args.dry_run {
        ("Would prune", "up to ")
    } else {
        ("Pruned", "")
    };
    eprintln!(
        "{verb} {} ({size_prefix}{:.1} MB) from the store",
        pluralizer::pluralize("file", removed_files as isize, true),
        removed_bytes as f64 / 1_048_576.0
    );
    Ok(())
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
