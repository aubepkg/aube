use crate::{
    DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph,
    RemoteTarballSource,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parse a yarn berry (v2+) lockfile's pre-read contents.
///
/// Public entry point is [`parse`]; this function is split out so we
/// don't re-read the file just to peek for the `__metadata:` marker.
pub(super) fn parse_berry_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let doc: yaml_serde::Value = yaml_serde::from_str(content)
        .map_err(|e| Error::parse_yaml_err(path, content.to_string(), &e))?;
    let map = doc
        .as_mapping()
        .ok_or_else(|| Error::parse(path, "yarn berry lockfile root must be a mapping"))?;

    // Validate `__metadata.version` — berry has been at major
    // version 3 since yarn 2, 6 from yarn 3.x, and 8 from yarn 4.x.
    // We accept any value >= 3; the shape we care about (block
    // headers, `resolution:` / `checksum:`) hasn't changed across
    // those versions.
    let meta_version = map
        .get("__metadata")
        .and_then(|m| m.as_mapping())
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if meta_version < 3 {
        return Err(Error::parse(
            path,
            format!(
                "yarn berry lockfile has unexpected __metadata.version: {meta_version} (expected >= 3)"
            ),
        ));
    }

    let cache_key = map
        .get("__metadata")
        .and_then(|m| m.as_mapping())
        .and_then(|m| m.get("cacheKey"))
        .and_then(yaml_scalar_as_string)
        .unwrap_or_else(|| DEFAULT_CACHE_KEY.to_string());

    // First pass: walk every top-level block, turning each into a
    // `LockedPackage` keyed by canonical `name@version` and recording
    // every header-spec → dep_path mapping for the second pass.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut patched_dependencies: BTreeMap<String, String> = BTreeMap::new();
    // `(name@version, header)` of blocks skipped as yarn-internal compat
    // patches; their raw text is attached to the owning package below.
    // Ranges the root `name@workspace:.` block recorded; drift is judged
    // against these, not the current manifest.
    let mut root_ranges: BTreeMap<String, String> = BTreeMap::new();
    let mut preserved_blocks: Vec<(String, String)> = Vec::new();
    // Transitive dep values written into each `LockedPackage` in this
    // pass are the raw header specs (e.g. `"foo@npm:^2.0.0"`); the
    // second pass collapses them down to dep_paths.

    for (key, value) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        if key_str.starts_with("__") {
            continue;
        }
        let block = value.as_mapping().ok_or_else(|| {
            Error::parse(
                path,
                format!("yarn berry block '{key_str}' is not a mapping"),
            )
        })?;

        let specs = split_berry_header(key_str);
        if specs.is_empty() {
            continue;
        }

        // Berry writes versions unquoted (`version: 1.0.0`). Depending
        // on the YAML resolver, scalar-looking versions may parse as
        // non-string values, so coerce them back to strings rather than
        // reporting "has no version" against a spec that obviously does.
        let version = block
            .get("version")
            .and_then(yaml_scalar_as_string)
            .ok_or_else(|| {
                Error::parse(path, format!("yarn berry block '{key_str}' has no version"))
            })?;

        let resolution = block
            .get("resolution")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("yarn berry block '{key_str}' has no resolution"),
                )
            })?;
        let (res_name, res_protocol, res_body) = parse_berry_spec(resolution).ok_or_else(|| {
            Error::parse(
                path,
                format!("yarn berry block '{key_str}' has malformed resolution '{resolution}'"),
            )
        })?;

        // Map the resolution protocol onto aube's data model. `npm:` is
        // the default and the only thing with a conventional
        // `name@version` dep_path; everything else is a non-registry
        // source the linker needs to materialize differently.
        let local_source = match res_protocol {
            "npm" => None,
            "workspace" => {
                // The root workspace block lives here (`name@workspace:.`)
                // and is driven by `package.json`, not the lockfile. We
                // rely on the manifest pass below to populate
                // `importers["."]`, and skip emitting workspace blocks
                // as `LockedPackage` entries.
                for spec in &specs {
                    spec_to_dep_path.insert(spec.clone(), format!("{res_name}@{version}"));
                }
                if res_body == "." {
                    root_ranges.extend(collect_dep_map(block, "dependencies"));
                    root_ranges.extend(collect_dep_map(block, "optionalDependencies"));
                }
                continue;
            }
            "patch" => match patch_protocol_path(res_body) {
                PatchSelector::Single(patch_path) => {
                    patched_dependencies.insert(format!("{res_name}@{version}"), patch_path);
                    None
                }
                PatchSelector::Skip => {
                    preserved_blocks.push((format!("{res_name}@{version}"), key_str.to_string()));
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_YARN_BERRY_UNSUPPORTED,
                        "yarn berry patch protocol in block '{}' is not supported — entry skipped",
                        key_str,
                    );
                    continue;
                }
                PatchSelector::Multiple => {
                    return Err(Error::parse(
                        path,
                        format!(
                            "yarn berry patch block '{key_str}' lists multiple patch files in one selector; aube applies one patch per package"
                        ),
                    ));
                }
            },
            "portal" => Some(LocalSource::Portal(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            "exec" => Some(LocalSource::Exec(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            "file" => Some(file_protocol_source(res_body)),
            "link" => Some(LocalSource::Link(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            // Plain HTTP(S) tarball or git-over-HTTPS: berry records
            // the full URL in the spec, which `parse_berry_spec` split
            // into `res_protocol = "https"` and `res_body =
            // "//host/path..."`. Glue them back together with
            // `<protocol>:<body>` to get the original URL, then let
            // `classify_remote` pick tarball vs git based on `.git`
            // in the URL.
            "http" | "https" => {
                let url = format!("{res_protocol}:{res_body}");
                classify_remote(&url, block)
            }
            // Git via a non-HTTP transport. Covers `git:`, `ssh:`, and
            // the compound `git+ssh:` / `git+https:` / `git+file:`
            // variants berry emits for git deps whose commit is pinned
            // after `#`. Rejoin `<protocol>:<body>` into the full URL
            // the linker will hand to `git clone`.
            p if p == "git" || p == "ssh" || p.starts_with("git+") || p.starts_with("ssh+") => {
                let url = format!("{res_protocol}:{res_body}");
                Some(LocalSource::Git(GitSource {
                    url: strip_commit_hash(&url),
                    committish: None,
                    resolved: extract_commit_hash(&url).unwrap_or_default(),
                    integrity: None,
                    subpath: None,
                }))
            }
            _ => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_YARN_BERRY_UNSUPPORTED,
                    "yarn berry unrecognized protocol '{}' in block '{}' — entry skipped",
                    res_protocol,
                    key_str,
                );
                continue;
            }
        };

        // Canonical dep_path: `name@version` for registry packages,
        // whatever `LocalSource::dep_path` returns for non-registry ones.
        // Berry always pairs registry and local deps by `name@version`
        // at the graph layer, so duplicate names with the same version
        // but different protocols collapse — same as the classic writer.
        let dep_path = match &local_source {
            Some(src) => src.dep_path(res_name),
            None => format!("{res_name}@{version}"),
        };

        for spec in &specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Transitive deps: `name: "protocol:range"`. We store the raw
        // header-style spec (`name@protocol:range`) and rewrite to a
        // dep_path in pass two.
        let mut raw_deps = collect_dep_map(block, "dependencies");
        let peer_deps = collect_dep_map(block, "peerDependencies");
        let peer_deps_meta = collect_peer_meta(block);
        let mut optional_deps = collect_dep_map(block, "optionalDependencies");
        // Berry has no `optionalDependencies` section: optional edges sit
        // in `dependencies` and `dependenciesMeta` flags them.
        for name in collect_optional_dep_names(block) {
            if let Some(range) = raw_deps.remove(&name) {
                optional_deps.insert(name, range);
            }
        }

        // Declared ranges — same source as `raw_deps` / `optional_deps`
        // but kept as the bare range string (no `name@` prefix) so
        // writers can slot them straight back into the output.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (n, v) in raw_deps.iter().chain(optional_deps.iter()) {
            declared.insert(n.clone(), v.clone());
        }

        let raw_deps_specs: BTreeMap<String, String> = raw_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();
        let optional_deps_specs: BTreeMap<String, String> = optional_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();

        let bin: BTreeMap<String, String> = block
            .get("bin")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let conditions = block
            .get("conditions")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let checksum = block
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if !packages.contains_key(&dep_path) {
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: res_name.to_string(),
                    version: version.clone(),
                    integrity: None,
                    yarn_checksum: checksum,
                    bin,
                    dependencies: raw_deps_specs,
                    optional_dependencies: optional_deps_specs,
                    peer_dependencies: peer_deps,
                    peer_dependencies_meta: peer_deps_meta,
                    dep_path: dep_path.clone(),
                    local_source,
                    declared_dependencies: declared,
                    extra_meta: conditions
                        .map(|c| {
                            BTreeMap::from([(
                                crate::EXTRA_YARN_BERRY_CONDITIONS.to_string(),
                                serde_json::Value::String(c),
                            )])
                        })
                        .unwrap_or_default(),
                    ..Default::default()
                }
                .with_declared_dependencies_known(),
            );
        }
    }

    for (spec_key, header) in preserved_blocks {
        let Some(pkg) = packages.values_mut().find(|p| p.spec_key() == spec_key) else {
            continue;
        };
        let Some(raw) = raw_block_text(content, &header) else {
            continue;
        };
        let entry = pkg
            .extra_meta
            .entry(crate::EXTRA_YARN_BERRY_PRESERVED_BLOCKS.to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(blocks) = entry {
            blocks.push(serde_json::json!({ "header": header, "raw": raw }));
        }
    }

    // Second pass: resolve raw header specs on each package's
    // `dependencies` / `optional_dependencies` map to canonical dep_paths.
    let mut resolved_deps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut resolved_opts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let resolve = |raw: &BTreeMap<String, String>| {
            let mut out = BTreeMap::new();
            for (name, raw_spec) in raw {
                if let Some(target) = spec_to_dep_path.get(raw_spec) {
                    out.insert(name.clone(), target.clone());
                }
            }
            out
        };
        resolved_deps.insert(dep_path.clone(), resolve(&pkg.dependencies));
        resolved_opts.insert(dep_path.clone(), resolve(&pkg.optional_dependencies));
    }
    for (dep_path, deps) in resolved_deps {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }
    for (dep_path, deps) in resolved_opts {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.optional_dependencies = deps;
        }
    }

    // Build direct deps from the manifest, using the yarn berry
    // convention that a range `"^1.0.0"` corresponds to the spec
    // `"name@npm:^1.0.0"`. If the manifest range already carries a
    // protocol prefix (`workspace:*`, `file:./pkgs/foo`, ...), it's
    // already a valid spec suffix and we try it verbatim first.
    let mut direct: Vec<DirectDep> = Vec::new();
    // One declaration produces one `DirectDep`, even when the same name
    // appears in several manifest sections. See the equivalent guard in
    // `npm::read` for why a duplicate is harmful.
    let mut direct_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push_direct =
        |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
            let candidates = berry_spec_candidates(name, range);
            for candidate in candidates {
                if let Some(dep_path) = spec_to_dep_path.get(&candidate) {
                    if !direct_seen.insert(name.to_string()) {
                        return;
                    }
                    direct.push(DirectDep {
                        name: name.to_string(),
                        dep_path: dep_path.clone(),
                        dep_type,
                        specifier: Some(locked_root_range(&root_ranges, name, range)),
                    });
                    return;
                }
            }
        };
    for (name, range) in &manifest.dependencies {
        push_direct(name, range, DepType::Production, &mut direct);
    }
    for (name, range) in &manifest.dev_dependencies {
        push_direct(name, range, DepType::Dev, &mut direct);
    }
    for (name, range) in &manifest.optional_dependencies {
        push_direct(name, range, DepType::Optional, &mut direct);
    }

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), direct);

    Ok(LockfileGraph {
        importers,
        packages,
        patched_dependencies,
        extra_fields: BTreeMap::from([(
            crate::EXTRA_YARN_BERRY_METADATA.to_string(),
            serde_json::json!({ "version": meta_version, "cacheKey": cache_key }),
        )]),
        ..Default::default()
    })
}

/// The range the lockfile's root workspace block recorded for `name`, in
/// manifest spelling (the `npm:` prefix berry adds is dropped when the
/// manifest range has no protocol). Falls back to the manifest range when
/// the block doesn't list the dep.
fn locked_root_range(root_ranges: &BTreeMap<String, String>, name: &str, range: &str) -> String {
    match root_ranges.get(name) {
        Some(locked) if range_has_protocol(range) => locked.clone(),
        Some(locked) => locked.strip_prefix("npm:").unwrap_or(locked).to_string(),
        None => range.to_string(),
    }
}

/// Raw text of the top-level block whose (unquoted) header is `header`,
/// without its trailing blank line. Yarn separates blocks with a blank
/// line, so the file splits cleanly on `\n\n`.
fn raw_block_text(content: &str, header: &str) -> Option<String> {
    let content = if content.contains('\r') {
        std::borrow::Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(content)
    };
    content.split("\n\n").find_map(|chunk| {
        let chunk = chunk.trim_matches('\n');
        let first = chunk.lines().next()?;
        if first.starts_with('#') || !first.ends_with(':') {
            return None;
        }
        let doc: yaml_serde::Value = yaml_serde::from_str(&format!("{first} ~")).ok()?;
        let key = doc.as_mapping()?.keys().next()?.as_str()?;
        (key == header).then(|| chunk.to_string())
    })
}

/// `__metadata.cacheKey` yarn 4 writes; used when the graph didn't come
/// from a berry lockfile.
const DEFAULT_CACHE_KEY: &str = "10c0";

/// `__metadata.version` written when the graph didn't come from a berry
/// lockfile.
const DEFAULT_METADATA_VERSION: u32 = 8;

/// Split a berry block header like `"foo@npm:^1.0.0, foo@npm:^2.0.0"`
/// into individual specs. The YAML layer already unquoted the key;
/// berry separates multiple specs inside the single key string with
/// `", "` (space required). Leading/trailing whitespace tolerated.
pub(super) fn split_berry_header(header: &str) -> Vec<String> {
    header
        .split(", ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a berry spec `<name>@<protocol>:<rest>` into its three parts.
/// Scoped names (`@scope/pkg`) are handled by skipping the leading `@`.
/// Returns `None` if the spec is malformed.
pub(super) fn parse_berry_spec(spec: &str) -> Option<(&str, &str, &str)> {
    let (name, after_at) = if let Some(rest) = spec.strip_prefix('@') {
        // Scoped: `@scope/pkg@<protocol>:<rest>` — find the second `@`.
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let full_name_len = 1 + slash + 1 + at;
        (&spec[..full_name_len], &spec[full_name_len + 1..])
    } else {
        let at = spec.find('@')?;
        (&spec[..at], &spec[at + 1..])
    };
    let colon = after_at.find(':')?;
    let protocol = &after_at[..colon];
    let body = &after_at[colon + 1..];
    Some((name, protocol, body))
}

/// Build the ordered list of berry spec strings to try when matching
/// a manifest entry against the lockfile's spec-to-dep-path index.
/// First we try the raw `name@range` (covers cases where the manifest
/// already carries a protocol prefix like `workspace:*`); failing
/// that, fall back to `name@npm:range` which is the default berry
/// adds when the user writes an un-prefixed semver range.
fn berry_spec_candidates(name: &str, range: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    out.push(format!("{name}@{range}"));
    if !range_has_protocol(range) {
        out.push(format!("{name}@npm:{range}"));
    }
    out
}

/// True when a manifest range carries a berry protocol prefix like
/// `npm:...`, `workspace:...`, `file:...`, `link:...`, `portal:...`,
/// `patch:...`, `exec:...`, `git:...`, `git+ssh:...`, `http:`,
/// `https:`. Used to decide whether to prepend `npm:` when building
/// the spec candidate.
pub(super) fn range_has_protocol(range: &str) -> bool {
    let Some(colon) = range.find(':') else {
        return false;
    };
    let head = &range[..colon];
    // Yarn berry's protocol heads are alphabetic with optional `+`
    // separators for compound transports (`git+ssh`, `git+https`,
    // `git+file`). Digits and other punctuation are not valid
    // protocol chars, which also rules out Windows drive letters
    // (single-letter heads are technically still valid protocols,
    // but yarn itself doesn't emit any and the `file:` spelling
    // handles those deps).
    !head.is_empty() && head.chars().all(|c| c.is_ascii_alphabetic() || c == '+')
}

/// Render a scalar YAML value as its source-text-equivalent string.
///
/// Berry emits scalar fields unquoted in several places. YAML parsers
/// may resolve integer-, float-, or boolean-looking tokens to typed
/// values; returning those as strings preserves the graph edge instead
/// of silently dropping an otherwise valid lockfile entry.
fn yaml_scalar_as_string(v: &yaml_serde::Value) -> Option<String> {
    match v {
        yaml_serde::Value::String(s) => Some(s.clone()),
        yaml_serde::Value::Number(n) => Some(n.to_string()),
        yaml_serde::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Extract a `BTreeMap<name, value>` from a sub-mapping like
/// `dependencies:` or `peerDependencies:`. Missing section or
/// non-mapping values return empty. Values go through
/// `yaml_scalar_as_string` for the same reason `version` does — a
/// bare `dep: 5` would otherwise silently drop the edge instead of
/// recording `"5"` as the range.
fn collect_dep_map(block: &yaml_serde::Mapping, key: &str) -> BTreeMap<String, String> {
    block
        .get(key)
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), yaml_scalar_as_string(v)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// Names `dependenciesMeta` flags `optional: true`.
fn collect_optional_dep_names(block: &yaml_serde::Mapping) -> Vec<String> {
    let Some(meta) = block.get("dependenciesMeta").and_then(|v| v.as_mapping()) else {
        return Vec::new();
    };
    meta.iter()
        .filter(|(_, v)| v.get("optional").and_then(|o| o.as_bool()) == Some(true))
        .filter_map(|(k, _)| k.as_str().map(str::to_string))
        .collect()
}

/// Pull `peerDependenciesMeta` into our structured form. Only the
/// `optional` flag round-trips through aube's model; other keys in
/// the meta block (if any) are ignored.
fn collect_peer_meta(block: &yaml_serde::Mapping) -> BTreeMap<String, crate::PeerDepMeta> {
    block
        .get("peerDependenciesMeta")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str()?.to_string();
                    let meta = v.as_mapping()?;
                    let optional = meta
                        .get("optional")
                        .and_then(|o| o.as_bool())
                        .unwrap_or(false);
                    Some((name, crate::PeerDepMeta { optional }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a `file:`-protocol body like `./vendor/foo` or
/// `./vendor/foo-1.0.0.tgz#hash` to a [`LocalSource`]. The fragment
/// (`#...`) that berry appends to pin the imported checksum is
/// stripped — aube's `LocalSource` records the path only, and the
/// checksum round-trips via `yarn_checksum`.
fn file_protocol_source(body: &str) -> LocalSource {
    let path = PathBuf::from(strip_hash_fragment(body));
    let is_tarball = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tgz") || e.eq_ignore_ascii_case("gz"));
    if is_tarball {
        LocalSource::Tarball(path)
    } else {
        LocalSource::Directory(path)
    }
}

/// Classification of a Yarn `patch:` selector's real-file list.
pub(super) enum PatchSelector {
    /// Exactly one real project patch file to apply.
    Single(String),
    /// No real patch — all `builtin<…>` compat shims, or empty. Skip the entry.
    Skip,
    /// Two or more real patch files in one selector. aube stores one patch
    /// path per package and can't represent this, so it's a hard error rather
    /// than a partial apply (which would materialize a package that doesn't
    /// match the lockfile).
    Multiple,
}

/// Classify Yarn's `patch:` protocol body into a [`PatchSelector`].
///
/// Berry encodes patched npm packages as
/// `name@patch:name@npm%3Aversion#<selector>::version=...&hash=...`.
/// Aube applies a single patch through its existing git-diff materializer.
///
/// The selector is a `&`-joined list of patch paths (Yarn's
/// `patchUtils.ts` grammar). `builtin<…>` segments are Yarn's internal
/// compat shims with no project file to apply — aube ignores them (the
/// package still resolves via its plain `npm:` block). `::`-params are split
/// off first because they contain their own `&`. One real patch →
/// [`PatchSelector::Single`]; all-builtin/empty → [`PatchSelector::Skip`];
/// multiple real patches → [`PatchSelector::Multiple`] (Yarn permits this but
/// its CLI never emits it).
pub(super) fn patch_protocol_path(body: &str) -> PatchSelector {
    let Some((_, after_hash)) = body.split_once('#') else {
        return PatchSelector::Skip;
    };
    let selector = after_hash
        .split_once("::")
        .map(|(p, _)| p)
        .unwrap_or(after_hash);
    let mut reals = selector.split('&').filter_map(|path| {
        // Strip `!`-delimited flags up to the last `!` (Yarn 4.0+).
        let path = path.rsplit_once('!').map_or(path, |(_, p)| p);
        // Strip the `~/` project-root prefix (Yarn 4.0+ `onProject`), or the
        // legacy bare-`~` optional flag (Yarn 3.x, `~builtin<…>`). They never
        // collide: a project path is `~/…` (slash), the flag is `~`+non-slash.
        let path = if let Some(rest) = path.strip_prefix("~/") {
            rest
        } else if let Some(rest) = path.strip_prefix('~') {
            rest
        } else {
            path
        };
        let is_builtin = path.starts_with("builtin<") && path.ends_with('>');
        (!path.is_empty() && !is_builtin).then_some(path)
    });
    let Some(first) = reals.next() else {
        return PatchSelector::Skip;
    };
    if reals.next().is_some() {
        return PatchSelector::Multiple;
    }
    PatchSelector::Single(first.to_string())
}

fn patch_spec_path(spec: &str) -> Option<String> {
    let (_, protocol, body) = parse_berry_spec(spec)?;
    if protocol != "patch" {
        return None;
    }
    match patch_protocol_path(body) {
        PatchSelector::Single(p) => Some(p),
        PatchSelector::Skip | PatchSelector::Multiple => None,
    }
}

fn patch_spec_matches(
    spec: &str,
    pkg: &LockedPackage,
    patched_dependencies: &BTreeMap<String, String>,
) -> bool {
    let Some(path) = patch_spec_path(spec) else {
        return false;
    };
    patched_dependencies
        .get(&pkg.spec_key())
        .is_some_and(|expected| expected == &path)
}

fn strip_hash_fragment(s: &str) -> &str {
    s.split_once('#').map(|(a, _)| a).unwrap_or(s)
}

/// If the URL has a `#<commit>` suffix, return `<commit>`. Used for
/// git-over-http berry specs that pin the resolved commit after `#`.
fn extract_commit_hash(url: &str) -> Option<String> {
    url.split_once('#')
        .and_then(|(_, b)| crate::normalize_git_fragment(b))
}

fn strip_commit_hash(url: &str) -> String {
    strip_hash_fragment(url).to_string()
}

/// Classify a remote URL in berry's resolution field as either a git
/// repo (if it has a commit hash suffix or a `.git` path) or a plain
/// tarball download. Checksum / integrity lives on the `checksum:`
/// field and round-trips through `yarn_checksum`.
fn classify_remote(url: &str, _block: &yaml_serde::Mapping) -> Option<LocalSource> {
    if url.ends_with(".git") || url.contains(".git#") {
        Some(LocalSource::Git(GitSource {
            url: strip_commit_hash(url),
            committish: None,
            resolved: extract_commit_hash(url).unwrap_or_default(),
            integrity: None,
            subpath: None,
        }))
    } else {
        Some(LocalSource::RemoteTarball(RemoteTarballSource {
            url: strip_commit_hash(url),
            integrity: String::new(),
            git_hosted: false,
        }))
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn berry lockfile
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn berry (v2+) lockfile.
///
/// `__metadata` is re-stamped from the parsed lockfile; a graph that
/// didn't come from berry gets yarn 4's `version: 8` / `cacheKey: 10c0`
/// (accepted by yarn 3.x too; yarn 2.x is functionally extinct). The
/// block shape — one entry per canonical `(name, version)` with a
/// comma-separated header of all specifiers that resolve to it — is
/// identical to the classic writer's, just reformatted as YAML with
/// `resolution:` / `checksum:` / `languageName` / `linkType` fields.
///
/// ## What round-trips
///
/// `yarn_checksum` (parsed from `checksum:`), patch specs, peer-dep
/// metadata, and all resolved transitive edges make it through parse →
/// write → parse unchanged.
///
/// ## What doesn't
///
/// - Peer-contextualized variants collapse onto a single `name@version`
///   block; berry's native encoding uses `virtual:` keys to keep them
///   distinct but aube's graph model doesn't, matching our pnpm/npm
///   writers.
/// - Packages the resolver produced fresh (no berry parse to source
///   `yarn_checksum` from) are written without a `checksum:` field.
///   Yarn's default `checksumBehavior: throw` populates missing
///   checksums on the next install against its own cache.
pub fn write_berry(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants to one entry per canonical
    // `(name, version)` — same as the classic writer. The canonical
    // key is `name@version`; we need it to look up a package's extra
    // specifiers in the same map.
    let canonical = crate::build_canonical_map(graph);

    // `extra_specs[canonical_key]` is the set of descriptors
    // (e.g. `"foo@npm:^1.0.0"`) something in the graph requests. Yarn
    // keys a block by exactly those, so the header is that set; the
    // exact `foo@npm:1.2.3` form is only a fallback for graphs that
    // carry no declared ranges.
    let mut extra_specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut patch_specs: BTreeMap<String, String> = BTreeMap::new();
    let manifest_ranges: Vec<(String, String)> = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter())
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect();
    let format_berry_spec = |dep_name: &str, range: &str| {
        if range_has_protocol(range) {
            format!("{dep_name}@{range}")
        } else {
            format!("{dep_name}@npm:{range}")
        }
    };
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        let Some((_, range)) = manifest_ranges.iter().find(|(n, _)| n == &dep.name) else {
            continue;
        };
        let manifest_spec = format_berry_spec(&dep.name, range);
        let pkg = canonical.get(&canonical_key).copied().unwrap();
        if patch_spec_matches(&manifest_spec, pkg, &graph.patched_dependencies) {
            patch_specs.insert(canonical_key.clone(), manifest_spec.clone());
        }
        extra_specs
            .entry(canonical_key)
            .or_default()
            .insert(manifest_spec);
    }
    // Harvest transitive declared ranges, same shape as the classic
    // writer. Berry specs always carry a protocol (`npm:`, `workspace:`,
    // `patch:` …); bare ranges get the default `npm:` prefix.
    for pkg in canonical.values() {
        for (dep_name, range) in &pkg.declared_dependencies {
            let Some(resolved_value) = pkg.dependencies.get(dep_name) else {
                continue;
            };
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            let Some(target_pkg) = canonical.get(&target) else {
                continue;
            };
            let manifest_spec = format_berry_spec(dep_name, range);
            if patch_spec_matches(&manifest_spec, target_pkg, &graph.patched_dependencies) {
                patch_specs.insert(target.clone(), manifest_spec.clone());
            }
            extra_specs.entry(target).or_default().insert(manifest_spec);
        }
    }

    let mut out = String::with_capacity(canonical.len().saturating_mul(256).max(4096));
    out.push_str("# This file is generated by running \"yarn install\" inside your project.\n");
    out.push_str("# Manual changes might be lost - proceed with caution!\n\n");
    let stored = graph.extra_fields.get(crate::EXTRA_YARN_BERRY_METADATA);
    let metadata_version = stored
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::from(DEFAULT_METADATA_VERSION));
    let cache_key = stored
        .and_then(|m| m.get("cacheKey"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_CACHE_KEY);
    out.push_str(&format!(
        "__metadata:\n  version: {metadata_version}\n  cacheKey: {}\n\n",
        if !cache_key.is_empty() && cache_key.bytes().all(|b| b.is_ascii_digit()) {
            cache_key.to_string()
        } else {
            quote_yaml_scalar_if_needed(cache_key)
        }
    ));

    // Yarn sorts blocks by header, so collect them and emit in that order
    // rather than in the graph's `name@version` order.
    let mut blocks: Vec<(String, String)> = Vec::with_capacity(canonical.len() + 1);
    if let Some(block) = workspace_block(manifest, &format_berry_spec) {
        blocks.push(block);
    }

    for (canonical_key, pkg) in &canonical {
        let mut out = String::with_capacity(256);
        let exact_spec = berry_exact_spec(pkg, &graph.patched_dependencies, &patch_specs);
        let header_specs: Vec<String> = match extra_specs.get(canonical_key) {
            Some(requested) => requested.iter().cloned().collect(),
            None => vec![exact_spec.clone()],
        };
        // Header and `resolution:` both carry spec strings that may
        // contain `:`, `/`, `@`, `#`, or — for `patch:` / file-path
        // sources — backslashes and quotes. Route both through
        // `quote_yaml_scalar` so escaping matches the rest of the
        // writer. For the multi-spec header we quote the joined
        // `", "`-separated list as one string, same as berry.
        let header_inner = header_specs.join(", ");
        out.push_str(&quote_yaml_scalar(&header_inner));
        out.push_str(":\n");

        // Scalar fields: version, resolution.
        out.push_str("  version: ");
        out.push_str(&quote_yaml_scalar_if_needed(&pkg.version));
        out.push('\n');
        out.push_str("  resolution: ");
        out.push_str(&quote_yaml_scalar(&exact_spec));
        out.push('\n');

        // Dependencies / peerDependencies / peerDependenciesMeta /
        // optionalDependencies: nested YAML mappings with
        // `name: "npm:<range-or-version>"` values. Resolved
        // transitive deps collapse to the exact version of the target
        // block (the resolver produced the graph, so the key always
        // exists in `canonical`).
        // Berry has no `optionalDependencies` section: optional edges are
        // folded into `dependencies` and flagged in `dependenciesMeta`.
        let all_deps: BTreeMap<String, String> = pkg
            .dependencies
            .iter()
            .chain(&pkg.optional_dependencies)
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        write_berry_dep_map(
            &mut out,
            "dependencies",
            &all_deps,
            &pkg.declared_dependencies,
            &canonical,
            &graph.patched_dependencies,
            &patch_specs,
        );
        write_berry_peer_deps(&mut out, &pkg.peer_dependencies);
        let optional_names: Vec<&String> = pkg
            .optional_dependencies
            .iter()
            .filter(|(n, v)| canonical.contains_key(&crate::npm::child_canonical_key(n, v)))
            .map(|(n, _)| n)
            .collect();
        if !optional_names.is_empty() {
            out.push_str("  dependenciesMeta:\n");
            for name in optional_names {
                out.push_str("    ");
                out.push_str(&quote_yaml_key(name));
                out.push_str(":\n      optional: true\n");
            }
        }
        write_berry_peer_meta(&mut out, &pkg.peer_dependencies_meta);
        if !pkg.bin.is_empty() {
            out.push_str("  bin:\n");
            for (name, bin_path) in &pkg.bin {
                out.push_str("    ");
                out.push_str(&quote_yaml_key(name));
                out.push_str(": ");
                out.push_str(&quote_yaml_scalar_if_needed(bin_path));
                out.push('\n');
            }
        }

        if let Some(checksum) = &pkg.yarn_checksum {
            out.push_str("  checksum: ");
            out.push_str(&quote_yaml_scalar_if_needed(checksum));
            out.push('\n');
        }
        if let Some(conditions) = pkg
            .extra_meta
            .get(crate::EXTRA_YARN_BERRY_CONDITIONS)
            .and_then(|v| v.as_str())
        {
            out.push_str("  conditions: ");
            out.push_str(&quote_conditions(conditions));
            out.push('\n');
        }
        out.push_str("  languageName: node\n");
        // `linkType: soft` means "just symlink, don't materialize into
        // the virtual store" — what berry uses for local directory refs
        // (`link:`, `portal:`, and `workspace:`). `hard` is the default
        // for registry packages and generated/cache-backed sources.
        let link_type = match &pkg.local_source {
            Some(LocalSource::Link(_) | LocalSource::Portal(_)) => "soft",
            _ => "hard",
        };
        out.push_str("  linkType: ");
        out.push_str(link_type);
        out.push('\n');
        blocks.push((header_inner, out));

        let preserved = pkg
            .extra_meta
            .get(crate::EXTRA_YARN_BERRY_PRESERVED_BLOCKS)
            .and_then(|v| v.as_array());
        for block in preserved.into_iter().flatten() {
            if let (Some(header), Some(raw)) = (
                block.get("header").and_then(|v| v.as_str()),
                block.get("raw").and_then(|v| v.as_str()),
            ) {
                blocks.push((header.to_string(), format!("{raw}\n")));
            }
        }
    }

    blocks.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (_, text)) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(text);
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}

/// The root `name@workspace:.` block yarn records for the project itself.
/// Returns `(header, text)`; yarn names an unnamed root `root-workspace-0b6124`.
fn workspace_block(
    manifest: &aube_manifest::PackageJson,
    format_spec: &dyn Fn(&str, &str) -> String,
) -> Option<(String, String)> {
    let name = manifest.name.as_deref().unwrap_or("root-workspace-0b6124");
    let header = format!("{name}@workspace:.");
    let mut out = String::new();
    out.push_str(&quote_yaml_scalar(&header));
    out.push_str(":\n  version: 0.0.0-use.local\n  resolution: ");
    out.push_str(&quote_yaml_scalar(&header));
    out.push('\n');
    let mut write_section = |section: &str, deps: Vec<(&String, &String)>| {
        if deps.is_empty() {
            return;
        }
        out.push_str("  ");
        out.push_str(section);
        out.push_str(":\n");
        for (dep_name, range) in deps {
            let spec = format_spec(dep_name, range);
            let body = spec.strip_prefix(&format!("{dep_name}@")).unwrap_or(&spec);
            out.push_str("    ");
            out.push_str(&quote_yaml_key(dep_name));
            out.push_str(": ");
            out.push_str(&quote_yaml_scalar(body));
            out.push('\n');
        }
    };
    // A name in several sections is one entry. Yarn folds optional dependencies
    // into `dependencies` and flags them in `dependenciesMeta`.
    // Same precedence as the package headers' `manifest_ranges` lookup in
    // `write_berry`, so the workspace entry always names a descriptor that
    // has a block.
    let mut deps: BTreeMap<&String, &String> = manifest.optional_dependencies.iter().collect();
    deps.extend(manifest.dev_dependencies.iter());
    deps.extend(manifest.dependencies.iter());
    write_section("dependencies", deps.into_iter().collect());
    write_section(
        "peerDependencies",
        manifest.peer_dependencies.iter().collect(),
    );
    if !manifest.optional_dependencies.is_empty() {
        out.push_str("  dependenciesMeta:\n");
        for dep_name in manifest.optional_dependencies.keys() {
            out.push_str("    ");
            out.push_str(&quote_yaml_key(dep_name));
            out.push_str(":\n      optional: true\n");
        }
    }
    out.push_str("  languageName: unknown\n  linkType: soft\n");
    Some((header, out))
}

/// The canonical header-spec used for a berry block's first
/// specifier and its `resolution:` field. Registry packages take the
/// form `name@npm:version`; non-registry sources use the protocol
/// recorded in `local_source`.
fn berry_exact_spec(
    pkg: &LockedPackage,
    patched_dependencies: &BTreeMap<String, String>,
    patch_specs: &BTreeMap<String, String>,
) -> String {
    let spec_key = pkg.spec_key();
    match (&pkg.local_source, patched_dependencies.get(&spec_key)) {
        (None, Some(_)) if patch_specs.contains_key(&spec_key) => patch_specs[&spec_key].clone(),
        (None, Some(path)) => {
            format!(
                "{}@patch:{}@npm%3A{}#{}",
                pkg.name, pkg.name, pkg.version, path
            )
        }
        (None, None) => format!("{}@npm:{}", pkg.name, pkg.version),
        (Some(src), _) => format!("{}@{}", pkg.name, src.specifier()),
    }
}

fn write_berry_dep_map(
    out: &mut String,
    section: &str,
    deps: &BTreeMap<String, String>,
    declared: &BTreeMap<String, String>,
    canonical: &BTreeMap<String, &LockedPackage>,
    patched_dependencies: &BTreeMap<String, String>,
    patch_specs: &BTreeMap<String, String>,
) {
    // Only emit edges whose target survives in `canonical`; the
    // graph-level filter layer (e.g. `--prod` prune) may have dropped
    // packages that dev-only edges still reference.
    //
    // Prefer the declared range from the package's own manifest (what
    // berry itself writes — `chalk: "npm:^4.1.0"`) over the resolved
    // pin. Falls back to `npm:<version>` when the declared range is
    // unknown (e.g. a pnpm-sourced graph being re-emitted as yarn).
    let resolved: Vec<(&str, String)> = deps
        .iter()
        .filter_map(|(n, v)| {
            let key = crate::npm::child_canonical_key(n, v);
            let target = canonical.get(&key)?;
            let spec_body = match &target.local_source {
                None => {
                    let body = declared.get(n).cloned().unwrap_or_else(|| {
                        if patched_dependencies.contains_key(&target.spec_key()) {
                            let exact = berry_exact_spec(target, patched_dependencies, patch_specs);
                            exact
                                .strip_prefix(&format!("{n}@"))
                                .unwrap_or(&exact)
                                .to_string()
                        } else {
                            crate::npm::dep_value_as_version(n, v).to_string()
                        }
                    });
                    // Declared ranges may already carry a protocol
                    // (`npm:^4`, `workspace:*`, `patch:…`) — don't
                    // double-prefix those. Bare ranges like `^4.1.0`
                    // get the default `npm:` protocol.
                    if body.contains(':') {
                        body
                    } else {
                        format!("npm:{body}")
                    }
                }
                Some(src) => src.specifier(),
            };
            Some((n.as_str(), spec_body))
        })
        .collect();
    if resolved.is_empty() {
        return;
    }
    out.push_str("  ");
    out.push_str(section);
    out.push_str(":\n");
    for (name, body) in resolved {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_yaml_scalar(&body));
        out.push('\n');
    }
}

fn write_berry_peer_deps(out: &mut String, peer: &BTreeMap<String, String>) {
    if peer.is_empty() {
        return;
    }
    out.push_str("  peerDependencies:\n");
    for (name, range) in peer {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_peer_range(range));
        out.push('\n');
    }
}

/// Yarn writes a peer range bare unless YAML needs it quoted, so
/// `^1.7.1 || ^2.0.0-alpha.4` stays plain while `*` and `>=1 <2: x` don't.
fn quote_peer_range(s: &str) -> String {
    let first_ok = s.chars().next().is_some_and(|c| {
        !matches!(
            c,
            '-' | '?'
                | ':'
                | ','
                | ']'
                | '['
                | '{'
                | '}'
                | '#'
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '\''
                | '"'
                | '%'
                | '@'
                | '`'
                | ' '
                | '\t'
                | '\r'
                | '\n'
        )
    });
    let rest_ok = s.chars().enumerate().all(|(i, c)| {
        i == 0
            || !matches!(
                c,
                ',' | ']' | '[' | '{' | '}' | ':' | '#' | '\t' | '\r' | '\n'
            ) && !(c == ' ' && s[i..].trim_start_matches(' ').is_empty())
    });
    if first_ok && rest_ok && !s.chars().all(|c| c.is_ascii_digit()) && !is_yaml_special_word(s) {
        s.to_string()
    } else {
        quote_yaml_scalar(s)
    }
}

fn write_berry_peer_meta(out: &mut String, meta: &BTreeMap<String, crate::PeerDepMeta>) {
    if meta.is_empty() {
        return;
    }
    out.push_str("  peerDependenciesMeta:\n");
    for (name, m) in meta {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(":\n");
        out.push_str("      optional: ");
        out.push_str(if m.optional { "true" } else { "false" });
        out.push('\n');
    }
}

/// Quote a YAML scalar so it round-trips through a standard YAML
/// parser regardless of punctuation in the content (`:`, `^`, `*`,
/// `@`, protocol prefixes, leading digits). Berry itself quotes
/// liberally; we do too. Double quotes are backslash-escaped.
fn quote_yaml_scalar(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A `conditions:` value (`os=darwin & cpu=arm64`) is written bare like
/// yarn does when it only holds characters that can't change the YAML
/// structure; anything else is quoted and escaped.
fn quote_conditions(s: &str) -> String {
    let bare = s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.ends_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '=' | '&' | '|' | '!' | '.' | '-' | '_')
        });
    if bare {
        s.to_string()
    } else {
        quote_yaml_scalar(s)
    }
}

/// Like [`quote_yaml_scalar`], but leaves plain tokens (versions such as
/// `4.6.20`, cache keys and checksums such as `10c0` / `10c0/2095be31`,
/// bin paths such as `bin/tsc`) bare the way yarn writes them.
fn quote_yaml_scalar_if_needed(s: &str) -> String {
    let plain = s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '/' | '_'))
        && !s.chars().all(|c| c.is_ascii_digit())
        && !is_yaml_special_word(s);
    if plain {
        s.to_string()
    } else {
        quote_yaml_scalar(s)
    }
}

/// Words YAML reads as a boolean or null rather than a string.
fn is_yaml_special_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
    )
}

/// Dependency names are written bare when YAML allows it, like yarn does;
/// scoped names start with `@`, a reserved YAML indicator, and stay quoted.
fn quote_yaml_key(s: &str) -> String {
    let bare = s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !is_yaml_special_word(s);
    if bare {
        s.to_string()
    } else {
        quote_yaml_scalar(s)
    }
}
