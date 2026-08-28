use super::bin_linking::{LinkAllBinsInput, link_all_bins};
use super::sweep::invalidate_changed_aube_entries;
use super::{InstallPhaseTimings, lifecycle::resolve_link_strategy};
use super::{delta, gvs};
use crate::commands::inject;
use crate::state;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;

pub(super) struct LinkPhaseInput<'a> {
    pub(super) cwd: &'a std::path::Path,
    pub(super) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(super) store: &'a aube_store::Store,
    pub(super) graph_for_link: &'a aube_lockfile::LockfileGraph,
    pub(super) package_indices: &'a BTreeMap<String, aube_store::PackageIndex>,
    pub(super) ws_dirs: &'a BTreeMap<String, std::path::PathBuf>,
    pub(super) manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(super) manifest: &'a aube_manifest::PackageJson,
    pub(super) build_policy: &'a aube_scripts::BuildPolicy,
    pub(super) node_version: Option<&'a str>,
    pub(super) prewarm_graph_hashes:
        Option<&'a std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
    pub(super) aube_dir: &'a std::path::Path,
    pub(super) modules_dir_name: &'a str,
    pub(super) virtual_store_dir_max_length: usize,
    pub(super) link_concurrency_setting: Option<usize>,
    pub(super) use_global_virtual_store_override: Option<bool>,
    pub(super) planned_gvs: bool,
    pub(super) has_workspace: bool,
    pub(super) dep_selection_filtered: bool,
    pub(super) workspace_filter_empty: bool,
    pub(super) ignore_scripts: bool,
    pub(super) prog_ref: Option<&'a crate::progress::InstallProgress>,
    pub(super) phase_timings: &'a mut InstallPhaseTimings,
}

pub(super) struct LinkPhaseOutput {
    pub(super) stats: aube_linker::LinkStats,
    pub(super) node_linker: aube_linker::NodeLinker,
    pub(super) virtual_store_only: bool,
    pub(super) current_leaf_hashes: Option<BTreeMap<String, String>>,
    pub(super) current_subtree_hashes: Option<BTreeMap<String, String>>,
    pub(super) patch_hashes: BTreeMap<String, String>,
}

pub(super) fn run_link_phase(input: LinkPhaseInput<'_>) -> miette::Result<LinkPhaseOutput> {
    let LinkPhaseInput {
        cwd,
        settings_ctx,
        store,
        graph_for_link,
        package_indices,
        ws_dirs,
        manifests,
        manifest,
        build_policy,
        node_version,
        prewarm_graph_hashes,
        aube_dir,
        modules_dir_name,
        virtual_store_dir_max_length,
        link_concurrency_setting,
        use_global_virtual_store_override,
        planned_gvs,
        has_workspace,
        dep_selection_filtered,
        workspace_filter_empty,
        ignore_scripts,
        prog_ref,
        phase_timings,
    } = input;

    // 6. Link node_modules
    let phase_start = std::time::Instant::now();
    // Resolve `packageImportMethod`. CLI override wins, then
    // `.npmrc` / `pnpm-workspace.yaml`, then `auto` (detect). Unknown
    // CLI values hard-error (preserving the explicit `--package-import-method`
    // diagnostic). Settings-file values flow through the generated typed
    // accessor, which collapses unknown values to `None` so they behave
    // like an absent setting.
    super::control::check_cancelled()?;
    let strategy = resolve_link_strategy(cwd, settings_ctx, planned_gvs)?;
    if let Some(p) = prog_ref {
        p.set_phase("linking");
    }
    tracing::debug!("Link strategy: {strategy:?}");

    let shamefully_hoist = aube_settings::resolved::shamefully_hoist(settings_ctx);
    let public_hoist_pattern = aube_settings::resolved::public_hoist_pattern(settings_ctx);
    let hoist = aube_settings::resolved::hoist(settings_ctx);
    let hoist_pattern = aube_settings::resolved::hoist_pattern(settings_ctx);
    let hoist_workspace_packages = aube_settings::resolved::hoist_workspace_packages(settings_ctx);
    let hoisting_limits = crate::commands::settings_hoisting_limits_to_linker(
        aube_settings::resolved::hoisting_limits(settings_ctx),
    );
    let dedupe_direct_deps = aube_settings::resolved::dedupe_direct_deps(settings_ctx);
    let virtual_store_only = aube_settings::resolved::virtual_store_only(settings_ctx);
    // Resolve the layout mode. CLI override wins, then `.npmrc` /
    // `pnpm-workspace.yaml`, then default (Isolated). `pnp` is a
    // hard error regardless of source — we don't ship a PnP runtime,
    // so accepting it would silently mislead. The CLI path hard-errors
    // on an unknown value so typos surface immediately; settings-file
    // values with an unknown spelling fall through to the generated
    // default today, so a `.npmrc` typo degrades to `isolated`
    // without a warning. Worth revisiting if that ever bites.
    let reject_pnp =
        miette!("node-linker=pnp is not supported by aube; use `isolated` (default) or `hoisted`");
    let node_linker_cli = aube_settings::values::string_from_cli("nodeLinker", settings_ctx.cli);
    let node_linker = if let Some(cli) = node_linker_cli.as_deref() {
        let trimmed = cli.trim();
        if trimmed.eq_ignore_ascii_case("pnp") {
            return Err(reject_pnp);
        }
        trimmed.parse::<aube_linker::NodeLinker>().map_err(|_| {
            miette!("unknown --node-linker value `{cli}`; expected `isolated` or `hoisted`")
        })?
    } else {
        match aube_settings::resolved::node_linker(settings_ctx) {
            aube_settings::resolved::NodeLinker::Pnp => return Err(reject_pnp),
            aube_settings::resolved::NodeLinker::Hoisted => aube_linker::NodeLinker::Hoisted,
            aube_settings::resolved::NodeLinker::Isolated => aube_linker::NodeLinker::Isolated,
        }
    };
    tracing::debug!("node-linker: {:?}", node_linker);

    let mut linker = aube_linker::Linker::new(store, strategy)
        .with_shamefully_hoist(shamefully_hoist)
        .with_public_hoist_pattern(&public_hoist_pattern)
        .with_hoist(hoist)
        .with_hoist_pattern(&hoist_pattern)
        .with_hoist_workspace_packages(hoist_workspace_packages)
        .with_hoisting_limits(hoisting_limits)
        .with_dedupe_direct_deps(dedupe_direct_deps)
        .with_virtual_store_dir_max_length(virtual_store_dir_max_length)
        .with_node_linker(node_linker)
        .with_link_concurrency(link_concurrency_setting)
        .with_virtual_store_only(virtual_store_only)
        .with_modules_dir_name(modules_dir_name.to_string())
        .with_aube_dir_override(aube_dir.to_path_buf());
    if planned_gvs && node_linker == aube_linker::NodeLinker::Isolated {
        linker = linker
            .with_project_local_dep_paths(gvs::legacy_vite_project_local_closure(graph_for_link));
    }
    if let Some(enabled) = use_global_virtual_store_override {
        linker = linker.with_use_global_virtual_store(enabled);
    }

    // Patches for delta-fingerprint folding and linker injection.
    // Hoisted ahead of subtree-hash so re-patched packages land in
    // the `changed` bucket and side-effects skip can't trust a stale
    // marker.
    let (patches_for_linker, patch_hashes) =
        crate::patches::load_patches_for_linker(cwd, &graph_for_link.patched_dependencies)?;

    // Compute leaf + subtree hashes together when both are needed.
    // Linker invalidation reads `current_subtree_hashes`; the late
    // state writeback reads the leaf map. Sharing the BLAKE3 leaf
    // pass cuts a duplicate `compute_package_hashes` traversal.
    let (current_leaf_hashes, current_subtree_hashes) = if !virtual_store_only
        && matches!(node_linker, aube_linker::NodeLinker::Isolated)
        && !dep_selection_filtered
        && workspace_filter_empty
    {
        let (leaf, subtree) =
            delta::compute_leaf_and_subtree_hashes(graph_for_link, &patch_hashes, cwd);
        (Some(leaf), Some(subtree))
    } else {
        (None, None)
    };
    if !linker.uses_global_virtual_store()
        && let Some(current_subtree_hashes) = current_subtree_hashes.as_ref()
        && let Some(prior_subtrees) = state::read_state_subtree_hashes(cwd)
    {
        let touched = delta::changed_subtree_roots(&prior_subtrees, current_subtree_hashes);
        let invalidated =
            invalidate_changed_aube_entries(aube_dir, &touched, virtual_store_dir_max_length);
        if invalidated > 0 {
            tracing::debug!("delta: invalidated {invalidated} changed .aube entry/entries");
        }
    }

    // 6a. Pre-compute content-addressed virtual-store hashes.
    //     Only necessary when linking into the shared global virtual
    //     store — in per-project mode (`CI=1`) the `.aube/<dep_path>`
    //     directories are already isolated so there's nothing to
    //     address. Folding engine state into the subdir name for any
    //     build-allowed package (plus every ancestor in its dep
    //     graph) keeps two projects resolving the same `(integrity,
    //     deps)` under different node / arch combos from stomping on
    //     each other; pure-JS packages with no build-allowed
    //     descendants get engine-agnostic hashes and stay shared.
    let patch_hash_fn = |name: &str, version: &str| -> Option<String> {
        let key = format!("{name}@{version}");
        patch_hashes.get(&key).cloned()
    };

    if linker.uses_global_virtual_store() {
        // Source-backed deps that get shared globally (git / remote
        // tarball) carry no registry integrity, so their graph-hash
        // identity is just their `<url>#<commit>` coordinate. Two
        // installs of the same coordinate can still materialize
        // different bytes — most commonly a git dep whose `prepare`
        // built `dist/` versus the same commit installed under
        // `--ignore-scripts` (raw checkout). Fingerprint the actual
        // imported tree and fold it into the hash so those two land at
        // distinct GVS paths instead of the first writer's tree leaking
        // into the second project.
        let mut content_hashes: aube_util::collections::FxMap<String, String> =
            aube_util::collections::FxMap::default();
        for (dep_path, pkg) in &graph_for_link.packages {
            let is_shareable_source = pkg
                .local_source
                .as_ref()
                .is_some_and(|s| s.is_globally_shareable());
            if !is_shareable_source {
                continue;
            }
            // The fingerprint *defines* this dep's GVS path, and the
            // linker keys its dependents' sibling symlinks off the same
            // hash. Silently dropping a dep whose index is absent would
            // compute the parent's path with a fingerprint-less hash
            // while the dep itself was materialized at the
            // fingerprinted path — a dangling sibling and a runtime
            // `Cannot find module`. The fetch driver guarantees every
            // in-graph source dep is imported (and thus indexed), so a
            // miss is a contract violation, not a recoverable cache
            // gap: there is no `store.load_index` fallback because
            // git/tarball indices aren't persisted by coordinate (a
            // prepared tree and its raw `--ignore-scripts` checkout
            // would collide). Fail loudly to keep the invariant honest.
            let index = package_indices.get(dep_path).ok_or_else(|| {
                miette!(
                    code = aube_codes::errors::ERR_AUBE_MISSING_PACKAGE_INDEX,
                    "internal: globally-shared source dependency {dep_path} is in the link \
                     graph but missing from package_indices; cannot fingerprint its content \
                     for the global virtual store"
                )
            })?;
            content_hashes.insert(
                dep_path.clone(),
                aube_store::index_content_fingerprint(index),
            );
        }
        let content_hash_fn =
            |dep_path: &str| -> Option<String> { content_hashes.get(dep_path).cloned() };

        // Reuse the prewarm task's `compute_graph_hashes` output when
        // the link-phase graph matches what the prewarm hashed. The
        // prewarm hashed the unfiltered post-resolve graph; if no
        // dep-selection or workspace filter applied, `graph_for_link`
        // == that graph by node count + key set, so the cached
        // hashes are byte-identical to a fresh compute. Falling
        // through to a fresh compute keeps the contract simple
        // whenever the graphs diverge.
        //
        // The prewarm runs concurrently with fetch and so can't see the
        // not-yet-imported source-dep trees; when any globally-shared
        // source dep is present its content fingerprint is missing from
        // the prewarm hashes, so skip the reuse and recompute here where
        // every index is available.
        let cached_hashes = prewarm_graph_hashes.filter(|arc| {
            content_hashes.is_empty()
                && arc.node_hash.len() == graph_for_link.packages.len()
                && graph_for_link
                    .packages
                    .keys()
                    .all(|k| arc.node_hash.contains_key(k))
        });
        let graph_hashes = if let Some(arc) = cached_hashes {
            arc.as_ref().clone()
        } else {
            let engine = node_version.map(aube_lockfile::graph_hash::engine_name_default);
            let allow = |pkg: &aube_lockfile::LockedPackage| {
                super::package_build_is_allowed(build_policy, pkg)
            };
            aube_lockfile::graph_hash::compute_graph_hashes_full(
                graph_for_link,
                &allow,
                engine.as_ref(),
                &patch_hash_fn,
                &content_hash_fn,
            )
        };
        linker = linker.with_graph_hashes(graph_hashes);
    }
    if !patches_for_linker.is_empty() {
        linker = linker.with_patches(patches_for_linker);
    }
    if linker.uses_global_virtual_store() {
        super::super::gvs_registry::register_project(&store.virtual_store_dir(), cwd, aube_dir)
            .wrap_err("failed to register project with global virtual store")?;
    }
    let stats = if has_workspace {
        linker
            .link_workspace(cwd, graph_for_link, package_indices, ws_dirs)
            .map_err(|error| (error, "failed to link workspace node_modules"))
    } else {
        linker
            .link_all(cwd, graph_for_link, package_indices)
            .map_err(|error| (error, "failed to link node_modules"))
    };
    let stats = match stats {
        Ok(stats) => stats,
        Err((error, context)) => {
            if linker.uses_global_virtual_store()
                && let Err(cleanup_error) = super::super::gvs_registry::unregister_if_unreferenced(
                    &store.virtual_store_dir(),
                    cwd,
                    aube_dir,
                )
            {
                tracing::debug!("failed to clean up GVS project registration: {cleanup_error}");
            }
            return Err(error).into_diagnostic().wrap_err(context);
        }
    };
    if linker.uses_global_virtual_store() {
        super::super::gvs_registry::register_project(&store.virtual_store_dir(), cwd, aube_dir)
            .wrap_err("failed to record project entries in the global virtual store")?;
    }
    tracing::debug!(
        "phase:link {:.1?} ({} files)",
        phase_start.elapsed(),
        stats.files_linked
    );
    phase_timings.record("link", phase_start.elapsed());

    // Keep the exact hoisted placement map in a sidecar even for filtered
    // installs, which intentionally do not replace the main freshness state.
    // Replanning the full graph later can associate a root-level package with
    // the wrong conflicting version.
    if !virtual_store_only {
        state::write_hoisted_placements(cwd, stats.hoisted_placements.as_ref())
            .into_diagnostic()
            .wrap_err("failed to record hoisted package placements")?;
    }

    // Apply `dependenciesMeta.<name>.injected` overrides. Only runs in
    // workspace + isolated mode: hoisted layouts don't have a
    // `.aube/<dep_path>/` virtual store for `apply_injected` to
    // sibling-link against, and hoisted resolution already walks the
    // consumer's root-level tree so the peer-context guarantee
    // injection is meant to give is already in place. Timed
    // separately so the `phase:link` metric isn't polluted with copy
    // work. Skipped under `virtualStoreOnly` — the workspace member
    // trees that `apply_injected` writes into don't exist.
    if has_workspace
        && matches!(node_linker, aube_linker::NodeLinker::Isolated)
        && !virtual_store_only
    {
        let inject_start = std::time::Instant::now();
        let injected_count = inject::apply_injected(
            cwd,
            modules_dir_name,
            aube_dir,
            virtual_store_dir_max_length,
            graph_for_link,
            manifests,
            ws_dirs,
        )?;
        if injected_count > 0 {
            tracing::debug!(
                "phase:inject {:.1?} ({injected_count} workspace deps injected)",
                inject_start.elapsed()
            );
        }
        phase_timings.record("inject", inject_start.elapsed());
    }

    // 7. Link .bin entries before dependency lifecycle scripts so builds can
    //    invoke their own dependencies. Approved builds get a refresh pass in
    //    finalize because a lifecycle may replace its bin target or bin map.
    let phase_start = std::time::Instant::now();
    if !virtual_store_only {
        link_all_bins(LinkAllBinsInput {
            project_dir: cwd,
            settings_ctx,
            modules_dir_name,
            aube_dir,
            graph: graph_for_link,
            virtual_store_dir_max_length,
            placements: stats.hoisted_placements.as_ref(),
            ws_dirs,
            manifests,
            manifest,
            node_linker,
            has_workspace,
            link_dependency_bins: !ignore_scripts && build_policy.has_any_allow_rule(),
            reset_existing: false,
        })?;
        tracing::debug!("phase:link_bins {:.1?}", phase_start.elapsed());
        phase_timings.record("link_bins", phase_start.elapsed());
    }
    Ok(LinkPhaseOutput {
        stats,
        node_linker,
        virtual_store_only,
        current_leaf_hashes,
        current_subtree_hashes,
        patch_hashes,
    })
}
