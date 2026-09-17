use super::run::load_manifest;
use aube_scripts::LifecycleHook;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashSet;

/// `aube rebuild [<pkg>...]` — without args, re-run the root package's
/// preinstall hook, then install / postinstall work for dependency
/// packages allowed by the active `allowBuilds` /
/// `onlyBuiltDependencies` policy, then the root package's install /
/// postinstall / prepare lifecycle hooks.
///
/// With one or more package names, run lifecycle scripts only for the
/// named deps and skip the root hooks. Match is by graph `name`, which
/// is the in-tree alias when one is configured (so `aube rebuild
/// my-alias` works for a manifest entry like
/// `"my-alias": "npm:real-pkg@1.0"`, matching pnpm).
///
/// Unlike the other lifecycle shortcuts, `rebuild` intentionally does not
/// auto-install: `aube install` already runs these same four hooks after
/// linking, so triggering an install here would double-run every script
/// on a stale tree. Users who actually want a fresh install should run
/// `aube install`.
#[derive(Debug, usage_rs::Args)]
pub struct RebuildArgs {
    /// Optional package names. When supplied, only matching deps'
    /// scripts run; the root lifecycle hooks (preinstall, install,
    /// postinstall, prepare) are skipped. Match is by graph `name`,
    /// not by `dep_path`. The active `allowBuilds` /
    /// `onlyBuiltDependencies` policy is bypassed for the named
    /// deps — naming the package is the explicit opt-in.
    #[usage(arg, name = "PACKAGE")]
    pub packages: Vec<String>,
}

pub async fn run(
    args: RebuildArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() {
        return run_filtered(&args, &filter).await;
    }
    let selected = if args.packages.is_empty() {
        None
    } else {
        Some(args.packages.iter().cloned().collect::<HashSet<String>>())
    };

    let cwd = crate::dirs::project_root()?;
    let manifest = load_manifest(&cwd)?;
    let files = crate::commands::FileSources::load(&cwd);
    let (workspace, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let settings_ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    super::configure_script_settings(&settings_ctx, Some("rebuild"));

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(graph) => Some(graph),
        Err(aube_lockfile::Error::NotFound(_)) => None,
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    // Selective rebuild needs a graph to match names against. Without
    // the lockfile the unmatched-name check below never runs, root
    // hooks are skipped (selected.is_some()), and the command would
    // exit Ok with no scripts run and no diagnostic — invisible in CI.
    if selected.is_some() && graph.is_none() {
        return Err(miette!(
            "no lockfile found at {} — run `{}` before targeting specific packages",
            cwd.display(),
            aube_util::cmd("install")
        ));
    }

    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);
    if selected.is_none() {
        aube_scripts::run_root_hook(
            &cwd,
            &modules_dir_name,
            &manifest,
            LifecycleHook::PreInstall,
        )
        .await
        .map_err(|e| miette!("{}", e))?;
    }

    if let Some(graph) = graph {
        if let Some(selected) = selected.as_ref() {
            let known: HashSet<&str> = graph.packages.values().map(|p| p.name.as_str()).collect();
            let unmatched: Vec<&str> = selected
                .iter()
                .filter(|n| !known.contains(n.as_str()))
                .map(String::as_str)
                .collect();
            if !unmatched.is_empty() {
                let mut sorted = unmatched;
                sorted.sort_unstable();
                return Err(miette!(
                    "no installed dependency matches: {}",
                    sorted.join(", ")
                ));
            }
        }

        let (policy, warnings) =
            super::install::build_policy_from_sources(&manifest, &workspace, false);
        for warning in warnings {
            eprintln!("warn: {warning}");
        }

        if selected.is_some() || policy.has_any_allow_rule() {
            let child_concurrency =
                aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
            let (jail_policy, jail_policy_warnings) =
                super::install::JailBuildPolicy::from_settings(&settings_ctx, &workspace);
            for warning in jail_policy_warnings {
                eprintln!("warn: {warning}");
            }
            // The generated accessor already reads `nodeLinker` from
            // `raw_workspace`, which is the same map `workspace.node_linker`
            // is parsed out of — no need for a separate fallback on the
            // typed struct field.
            let node_linker_setting = aube_settings::resolved::node_linker(&settings_ctx);
            let hoisting_limits = crate::commands::settings_hoisting_limits_to_linker(
                aube_settings::resolved::hoisting_limits(&settings_ctx),
            );
            let hoisted_placements = match node_linker_setting {
                aube_settings::resolved::NodeLinker::Pnp => {
                    return Err(miette!(
                        "node-linker=pnp is not supported by aube; use `isolated` (default) or `hoisted`"
                    ));
                }
                aube_settings::resolved::NodeLinker::Hoisted => {
                    Some(match crate::state::read_hoisted_placements(&cwd) {
                        Some(placements) => placements,
                        None => aube_linker::HoistedPlacements::from_graph(
                            &cwd,
                            &graph,
                            &modules_dir_name,
                            hoisting_limits,
                        )?,
                    })
                }
                aube_settings::resolved::NodeLinker::Isolated => None,
            };
            let side_effects_cache_root =
                if aube_settings::resolved::side_effects_cache(&settings_ctx) {
                    let store = super::open_store(&cwd)?;
                    Some(super::install::side_effects_cache_root(&store))
                } else {
                    None
                };
            // Re-emit the whole bin surface through the same entry
            // point `install` uses, rather than only the per-dep
            // shims. Linking is idempotent, so on an already-wired
            // tree this is a no-op — but sharing the entry point is
            // what keeps `rebuild` from drifting: every rule
            // `link_all_bins` establishes in order (an importer's
            // direct deps, its own `bin`, each workspace member's,
            // then the dependency pass) applies here too. Linking only
            // the dependency pass meant `rebuild` never reconciled an
            // importer's own `bin`, and left the dependency pass to
            // infer precedence that the importer passes normally
            // establish.
            let isolated = !matches!(
                node_linker_setting,
                aube_settings::resolved::NodeLinker::Hoisted
            );
            let canonicalize_package_dir = cfg!(windows)
                && isolated
                && super::install::detect_existing_global_virtual_store(
                    &cwd,
                    &aube_dir,
                    &modules_dir_name,
                    &super::global_virtual_store_dir(&cwd),
                )
                .unwrap_or(false);
            let node_linker = match node_linker_setting {
                aube_settings::resolved::NodeLinker::Hoisted => aube_linker::NodeLinker::Hoisted,
                // `Pnp` already returned above.
                _ => aube_linker::NodeLinker::Isolated,
            };
            let workspace_plan =
                super::install::discover_workspace_plan(&cwd, &manifest, &settings_ctx, &filter)?;
            let link_bins = |preserved: Option<&super::install::PreservedBinLinks>,
                             capture_managed: bool| {
                super::install::link_all_bins(super::install::LinkAllBinsInput {
                    project_dir: &cwd,
                    settings_ctx: &settings_ctx,
                    modules_dir_name: &modules_dir_name,
                    aube_dir: &aube_dir,
                    graph: &graph,
                    virtual_store_dir_max_length: super::resolve_virtual_store_dir_max_length(
                        &settings_ctx,
                    ),
                    placements: hoisted_placements.as_ref(),
                    ws_dirs: &workspace_plan.ws_dirs,
                    manifests: &workspace_plan.manifests,
                    manifest: &manifest,
                    node_linker,
                    has_workspace: workspace_plan.has_workspace,
                    link_dependency_bins: true,
                    capture_managed,
                    preserved,
                })
            };
            let managed_bin_links = link_bins(None, true)?;
            super::install::run_dep_lifecycle_scripts(
                &cwd,
                &modules_dir_name,
                &aube_dir,
                &graph,
                &policy,
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
                canonicalize_package_dir,
                child_concurrency,
                hoisted_placements.as_ref(),
                side_effects_cache_root
                    .as_deref()
                    .map(|root| {
                        // `rebuild` means "run scripts again"; readonly
                        // cache may not write, but it must not restore and
                        // skip the script work either.
                        if aube_settings::resolved::side_effects_cache_readonly(&settings_ctx) {
                            super::install::SideEffectsCacheConfig::Disabled
                        } else {
                            super::install::SideEffectsCacheConfig::SaveOnlyOverwrite(root)
                        }
                    })
                    .unwrap_or(super::install::SideEffectsCacheConfig::Disabled),
                &jail_policy,
                selected.as_ref(),
            )
            .await?;
            let preserved = super::install::remove_managed_bin_links(&managed_bin_links)?;
            let refreshed_bin_links = link_bins(Some(&preserved), false)?;
            super::install::remove_unclaimed_preserved_bin_links(
                &managed_bin_links,
                &preserved,
                &refreshed_bin_links,
            )?;
        }
    }

    if selected.is_none() {
        for hook in [
            LifecycleHook::Install,
            LifecycleHook::PostInstall,
            LifecycleHook::Prepare,
        ] {
            aube_scripts::run_root_hook(&cwd, &modules_dir_name, &manifest, hook)
                .await
                .map_err(|e| miette!("{}", e))?;
        }
    }

    Ok(())
}

async fn run_filtered(
    args: &RebuildArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "rebuild")?;
    let result = async {
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            Box::pin(run(
                RebuildArgs {
                    packages: args.packages.clone(),
                },
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}
