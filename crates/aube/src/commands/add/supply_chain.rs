use super::manifest::collect_workspace_versions;
use super::spec::parse_pkg_spec;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) async fn run_cli_name_gates(
    cwd: &Path,
    packages: &[String],
    allow_low_downloads: bool,
    prompt: crate::commands::add_supply_chain::LowDownloadPrompt,
) -> miette::Result<()> {
    let project_dir = supply_chain_project_dir(cwd);
    let manifest = crate::commands::load_manifest_or_default(&project_dir)?;
    let registry_inputs = registry_bound_inputs_for_supply_chain(&project_dir, packages);
    let (
        advisory_check,
        low_download_threshold,
        minimum_package_age_minutes,
        mut allowed_unpopular,
        lockfile_dir,
    ) = crate::commands::with_settings_ctx(&project_dir, |ctx| {
        let policy = if aube_settings::resolved::paranoid(ctx) {
            aube_settings::resolved::AdvisoryCheck::Required
        } else {
            aube_settings::resolved::advisory_check(ctx)
        };
        Ok::<_, miette::Report>((
            policy,
            aube_settings::resolved::low_download_threshold(ctx),
            aube_settings::resolved::minimum_package_age(ctx),
            aube_settings::resolved::allowed_unpopular_packages(ctx).unwrap_or_default(),
            crate::commands::install::resolve_active_lockfile_dir(&project_dir, &manifest, ctx)?,
        ))
    })?;
    let locked_registry_names = locked_registry_names(&lockfile_dir, &manifest);
    allowed_unpopular.extend(
        locked_registry_names
            .iter()
            .map(|name| glob::Pattern::escape(name)),
    );
    let registry_client = crate::commands::make_client(&project_dir);
    let full_packument_cache = crate::commands::packument_full_cache_dir_for_cwd(&project_dir);
    crate::commands::add_supply_chain::run_gates(
        &registry_inputs.name_only_advisory_names,
        &registry_inputs.exact_advisory_pairs,
        &registry_inputs.download_names,
        advisory_check,
        low_download_threshold,
        crate::commands::add_supply_chain::ReputationPolicy {
            allow: allow_low_downloads,
            prompt,
            minimum_package_age_minutes,
            registry_client: &registry_client,
            full_packument_cache: &full_packument_cache,
        },
        &allowed_unpopular,
    )
    .await
}

fn supply_chain_project_dir(cwd: &Path) -> std::path::PathBuf {
    crate::dirs::find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn locked_registry_names(
    lockfile_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> BTreeSet<String> {
    match aube_lockfile::parse_lockfile_with_kind(lockfile_dir, manifest) {
        Ok((graph, _)) => graph
            .packages
            .values()
            .filter(|pkg| pkg.local_source.is_none())
            .map(|pkg| pkg.registry_name().to_string())
            .collect(),
        Err(aube_lockfile::Error::NotFound(_)) => BTreeSet::new(),
        Err(err) => {
            tracing::debug!("could not read package names from active lockfile: {err}");
            BTreeSet::new()
        }
    }
}

#[derive(Default)]
struct RegistryBoundSupplyChainInputs {
    name_only_advisory_names: Vec<String>,
    exact_advisory_pairs: Vec<(String, String)>,
    download_names: Vec<String>,
}

fn registry_bound_inputs_for_supply_chain(
    cwd: &Path,
    packages: &[String],
) -> RegistryBoundSupplyChainInputs {
    let mut inputs = RegistryBoundSupplyChainInputs {
        name_only_advisory_names: Vec::with_capacity(packages.len()),
        exact_advisory_pairs: Vec::with_capacity(packages.len()),
        download_names: Vec::with_capacity(packages.len()),
    };
    let workspace_versions = collect_workspace_versions(cwd);
    // Scope→registry overrides + the default registry tell us which
    // names route through public npmjs. Anything else (a swapped-out
    // default registry, an `@myorg:registry=https://internal/`
    // override) has no signal in the OSV `MAL-*` database or the
    // npmjs weekly-downloads API — skip those names so private
    // packages don't trip the gates on a public-registry collision.
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    for raw in packages {
        let Ok(spec) = parse_pkg_spec(raw) else {
            // Parse failures get a richer diagnostic from
            // `update_manifest_for_add` later — we don't want to
            // double-report or block the gate on something that
            // would already fail.
            continue;
        };
        if spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.jsr_name.is_some()
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range)
        {
            continue;
        }
        // A bare `aube add my-pkg` against a local workspace sibling
        // resolves locally — no public registry round-trip happens,
        // so the OSV / downloads probes have nothing to say.
        if workspace_versions.contains_key(&spec.name) {
            continue;
        }
        if !npm_config.is_public_npmjs(&spec.name) {
            // `redact_url` strips any embedded userinfo (`https://tok@host/`
            // — uncommon but a registry URL can legally carry it) so a
            // token doesn't slip into observability pipelines that ingest
            // debug-level structured logs.
            tracing::debug!(
                "skipping supply-chain gates for {}: routes through non-public registry {}",
                spec.name,
                aube_util::url::redact_url(npm_config.registry_for(&spec.name))
            );
            continue;
        }
        // Scoped names (`@scope/name`) stay in the list. OSV's batch
        // API supports scoped queries — skipping them here would let
        // a `MAL-*` advisory against `@scope/evil` slip past the
        // hard block. The downloads probe already folds scoped
        // packages into `DownloadCount::Unknown` (npm's downloads
        // API doesn't index them), so the prompt naturally skips
        // them — no per-name special case needed in the gate.
        inputs.download_names.push(spec.name.clone());
        if spec.has_explicit_range && is_full_exact_version(&spec.range) {
            inputs.exact_advisory_pairs.push((spec.name, spec.range));
        } else {
            inputs.name_only_advisory_names.push(spec.name);
        }
    }
    inputs.name_only_advisory_names.sort();
    inputs.name_only_advisory_names.dedup();
    inputs.exact_advisory_pairs.sort();
    inputs.exact_advisory_pairs.dedup();
    inputs.download_names.sort();
    inputs.download_names.dedup();
    inputs
}

fn is_full_exact_version(range: &str) -> bool {
    let suffix_start = range.find(['-', '+']).unwrap_or(range.len());
    let core = &range[..suffix_start];
    let mut parts = core.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    [major, minor, patch]
        .into_iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && node_semver::Version::parse(range).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_bound_inputs_use_versioned_osv_for_exact_versions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs = registry_bound_inputs_for_supply_chain(tmp.path(), &["nx@23.0.0".into()]);

        assert_eq!(
            inputs.exact_advisory_pairs,
            vec![("nx".to_string(), "23.0.0".to_string())],
        );
        assert!(inputs.name_only_advisory_names.is_empty());
        assert_eq!(inputs.download_names, vec!["nx".to_string()]);
    }

    #[test]
    fn registry_bound_inputs_keep_ranges_and_tags_name_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs = registry_bound_inputs_for_supply_chain(
            tmp.path(),
            &[
                "nx@^23".into(),
                "pkg-major@4".into(),
                "pkg-minor@1.2".into(),
                "react".into(),
                "vite@latest".into(),
            ],
        );

        assert_eq!(
            inputs.name_only_advisory_names,
            vec![
                "nx".to_string(),
                "pkg-major".to_string(),
                "pkg-minor".to_string(),
                "react".to_string(),
                "vite".to_string(),
            ],
        );
        assert!(inputs.exact_advisory_pairs.is_empty());
        assert_eq!(
            inputs.download_names,
            vec![
                "nx".to_string(),
                "pkg-major".to_string(),
                "pkg-minor".to_string(),
                "react".to_string(),
                "vite".to_string(),
            ],
        );
    }

    #[test]
    fn full_exact_version_requires_major_minor_patch() {
        assert!(is_full_exact_version("1.2.3"));
        assert!(is_full_exact_version("1.2.3-beta.1"));
        assert!(is_full_exact_version("1.2.3+build.7"));
        assert!(!is_full_exact_version("1"));
        assert!(!is_full_exact_version("1.2"));
        assert!(!is_full_exact_version("^1.2.3"));
        assert!(!is_full_exact_version("latest"));
    }

    #[test]
    fn registry_bound_inputs_version_alias_checks_real_package() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs =
            registry_bound_inputs_for_supply_chain(tmp.path(), &["nx-stable@npm:nx@23.0.0".into()]);

        assert_eq!(
            inputs.exact_advisory_pairs,
            vec![("nx".to_string(), "23.0.0".to_string())],
        );
        assert_eq!(inputs.download_names, vec!["nx".to_string()]);
    }

    #[test]
    fn active_lockfile_packages_are_trusted_for_download_gate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write package.json");
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "tiny-package@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "tiny-package".to_string(),
                version: "1.0.0".to_string(),
                ..Default::default()
            },
        );
        aube_lockfile::write_lockfile(tmp.path(), &graph, &aube_manifest::PackageJson::default())
            .expect("write lockfile");

        assert_eq!(
            locked_registry_names(tmp.path(), &aube_manifest::PackageJson::default()),
            BTreeSet::from(["tiny-package".to_string()])
        );
    }

    #[test]
    fn lockfile_alias_trusts_the_registry_package_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write package.json");
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "tiny-alias@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "tiny-alias".to_string(),
                alias_of: Some("tiny-package".to_string()),
                version: "1.0.0".to_string(),
                ..Default::default()
            },
        );
        aube_lockfile::write_lockfile(tmp.path(), &graph, &aube_manifest::PackageJson::default())
            .expect("write lockfile");

        assert_eq!(
            locked_registry_names(tmp.path(), &aube_manifest::PackageJson::default()),
            BTreeSet::from(["tiny-package".to_string()])
        );
    }

    #[test]
    fn workspace_member_uses_root_lockfile_packages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let member = tmp.path().join("packages/app");
        std::fs::create_dir_all(&member).expect("create member");
        std::fs::write(
            tmp.path().join("package.json"),
            "{\"workspaces\":[\"packages/*\"]}\n",
        )
        .expect("write root package.json");
        std::fs::write(member.join("package.json"), "{}\n").expect("write member package.json");
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "tiny-package@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "tiny-package".to_string(),
                version: "1.0.0".to_string(),
                ..Default::default()
            },
        );
        let manifest =
            aube_manifest::PackageJson::from_path(&tmp.path().join("package.json")).unwrap();
        aube_lockfile::write_lockfile(tmp.path(), &graph, &manifest).expect("write lockfile");

        let project_dir = supply_chain_project_dir(&member);
        assert_eq!(project_dir, tmp.path());
        assert_eq!(
            locked_registry_names(&project_dir, &manifest),
            BTreeSet::from(["tiny-package".to_string()])
        );
    }

    #[test]
    fn configured_lockfile_dir_is_used_for_trusted_packages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lockfile_dir = tmp.path().join("locks");
        std::fs::create_dir_all(&lockfile_dir).expect("create lockfile dir");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write package.json");
        std::fs::write(tmp.path().join(".npmrc"), "lockfile-dir=locks\n").expect("write .npmrc");
        let manifest = aube_manifest::PackageJson::default();
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "tiny-package@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "tiny-package".to_string(),
                version: "1.0.0".to_string(),
                ..Default::default()
            },
        );
        aube_lockfile::write_lockfile(&lockfile_dir, &graph, &manifest).expect("write lockfile");

        let active_dir = crate::commands::with_settings_ctx(tmp.path(), |ctx| {
            crate::commands::install::resolve_active_lockfile_dir(tmp.path(), &manifest, ctx)
        })
        .expect("resolve active lockfile dir");
        assert_eq!(active_dir, lockfile_dir.canonicalize().unwrap());
        assert_eq!(
            locked_registry_names(&active_dir, &manifest),
            BTreeSet::from(["tiny-package".to_string()])
        );
    }
}
