use crate::{
    DepType, DirectDep, EXTRA_PRESERVE_TARBALL_URL, EXTRA_YARN_RESOLVED, Error, LocalSource,
    LockedPackage, LockfileGraph,
};
use std::collections::BTreeMap;
use std::path::Path;

/// Parse a yarn classic (v1) lockfile's pre-read contents.
pub(super) fn parse_classic_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let blocks = tokenize_blocks(content).map_err(|e| Error::parse(path, e))?;

    // spec_to_dep_path maps each specifier (e.g. "is-odd@^3.0.0") to its
    // resolved dep_path ("is-odd@3.0.1"). Used for resolving direct deps
    // from package.json ranges and transitive dep references.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for block in &blocks {
        let version = block
            .fields
            .get("version")
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("yarn.lock block {:?} has no version", block.specs),
                )
            })?
            .clone();

        // All specs in the key map to the same resolved package.
        // Extract the package name from the first spec.
        let name = parse_spec_name(&block.specs[0]).ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "could not parse package name from yarn.lock spec '{}'",
                    block.specs[0]
                ),
            )
        })?;
        // npm-protocol alias: `<alias>@npm:<real-name>@<version>`. `name`
        // stays the alias (matches the npm parser's convention — it keys
        // node_modules/<alias>/ and is what consumers refer to); the real
        // registry name lives in `alias_of` so registry_name() returns it.
        // Scan every spec — our writer emits the canonical `name@version`
        // first and the npm-alias spec alongside it, so checking only
        // specs[0] would miss the alias on round-trips.
        let alias_of = block
            .specs
            .iter()
            .find_map(|s| parse_npm_alias_real_name(s))
            .filter(|real| real.as_str() != name);

        let dep_path = format!("{name}@{version}");

        for spec in &block.specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Only insert the first occurrence; dedup is fine because yarn.lock
        // already guarantees unique (name, version) entries.
        if !packages.contains_key(&dep_path) {
            // Yarn records the declared ranges on each block's
            // `dependencies:` subsection exactly as they appear in the
            // package's own manifest — preserve them so re-emit keeps
            // the original specifiers.
            let declared: BTreeMap<String, String> = block
                .dependencies
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect();
            let tarball_url = block
                .fields
                .get("resolved")
                .and_then(|url| yarn_resolved_tarball_url(url));
            let mut extra_meta: BTreeMap<String, serde_json::Value> = tarball_url
                .as_deref()
                .filter(|url| yarn_resolved_tarball_url_needs_preserve(url))
                .map(|_| {
                    BTreeMap::from([(
                        EXTRA_PRESERVE_TARBALL_URL.to_string(),
                        serde_json::Value::Bool(true),
                    )])
                })
                .unwrap_or_default();
            if let Some(resolved) = block.fields.get("resolved") {
                extra_meta.insert(
                    EXTRA_YARN_RESOLVED.to_string(),
                    serde_json::Value::String(resolved.clone()),
                );
            }
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: name.clone(),
                    version: version.clone(),
                    integrity: block.fields.get("integrity").cloned(),
                    tarball_url,
                    // Store raw "name@range" pairs for now; resolve below.
                    dependencies: block
                        .dependencies
                        .iter()
                        .map(|(n, r)| (n.clone(), format!("{n}@{r}")))
                        .collect(),
                    dep_path,
                    declared_dependencies: declared,
                    alias_of: alias_of.clone(),
                    extra_meta,
                    ..Default::default()
                },
            );
        }
    }

    // Second pass: resolve transitive dep references to dep_paths.
    let mut resolved: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw_spec) in &pkg.dependencies {
            if let Some(resolved_path) = spec_to_dep_path.get(raw_spec) {
                deps.insert(
                    name.clone(),
                    crate::npm::dep_path_tail(name, resolved_path).to_string(),
                );
            }
        }
        resolved.insert(dep_path.clone(), deps);
    }
    for (dep_path, deps) in resolved {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }

    // Build direct deps from the manifest, cross-referencing against spec_to_dep_path.
    let mut direct: Vec<DirectDep> = Vec::new();
    // One declaration produces one `DirectDep`, even when the same name
    // appears in several manifest sections. See the equivalent guard in
    // `npm::read` for why a duplicate is harmful.
    let mut direct_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push_direct =
        |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
            let spec = format!("{name}@{range}");
            if let Some(dep_path) = spec_to_dep_path.get(&spec) {
                if !direct_seen.insert(name.to_string()) {
                    return;
                }
                direct.push(DirectDep {
                    name: name.to_string(),
                    dep_path: dep_path.clone(),
                    dep_type,
                    specifier: None,
                });
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
        ..Default::default()
    })
}

#[derive(Debug)]
struct Block {
    /// Specifier keys: each is a "name@range" string.
    specs: Vec<String>,
    /// Flat scalar fields (version, resolved, integrity, etc.)
    fields: BTreeMap<String, String>,
    /// Nested dependencies section: name -> range
    dependencies: BTreeMap<String, String>,
}

/// Tokenize the yarn.lock body into blocks. This is a line-based parser that
/// recognizes:
/// - Comments (`# …`) and blank lines
/// - Header lines ending in `:` (block keys)
/// - Fields indented with 2 spaces
/// - A special nested `dependencies:` section indented with 4 spaces
fn tokenize_blocks(content: &str) -> Result<Vec<Block>, String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;
    let mut in_deps = false;

    for (lineno, raw_line) in content.lines().enumerate() {
        let line_num = lineno + 1;

        // Strip trailing whitespace but preserve leading indentation
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        // Top-level: block header (one or more comma-separated specs ending in `:`)
        if indent == 0 {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            in_deps = false;

            let header = line.trim_end_matches(':').trim();
            if !line.ends_with(':') {
                return Err(format!(
                    "line {line_num}: expected block header ending in ':', got '{line}'"
                ));
            }

            let specs = parse_header_specs(header).map_err(|e| format!("line {line_num}: {e}"))?;
            current = Some(Block {
                specs,
                fields: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            });
            continue;
        }

        let block = current.as_mut().ok_or_else(|| {
            format!("line {line_num}: unexpected indented content before any block header")
        })?;

        if indent == 2 {
            in_deps = false;
            let body = line.trim_start();

            // Check for nested section markers (e.g. `dependencies:`)
            if body.ends_with(':') {
                let section = body.trim_end_matches(':').trim();
                if section == "dependencies"
                    || section == "optionalDependencies"
                    || section == "peerDependencies"
                {
                    // Only track `dependencies:` for our resolution graph; ignore others.
                    in_deps = section == "dependencies";
                    continue;
                }
                // Unknown 2-space section header — ignore.
                continue;
            }

            let (key, value) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse '{body}'"))?;
            block.fields.insert(key, value);
        } else if indent >= 4 && in_deps {
            let body = line.trim_start();
            let (name, range) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse dep '{body}'"))?;
            block.dependencies.insert(name, range);
        }
        // Deeper indents outside `dependencies:` are ignored.
    }

    if let Some(b) = current.take() {
        blocks.push(b);
    }

    Ok(blocks)
}

/// Parse a header like `"foo@^1.0.0", "foo@^1.1.0"` or `foo@^1.0.0` into specs.
fn parse_header_specs(header: &str) -> Result<Vec<String>, String> {
    let mut specs = Vec::new();
    for raw in header.split(',') {
        let s = raw.trim();
        let unquoted = unquote_yarn_scalar(s);
        if unquoted.is_empty() {
            return Err(format!("empty spec in header '{header}'"));
        }
        specs.push(unquoted.to_string());
    }
    if specs.is_empty() {
        return Err(format!("no specs parsed from header '{header}'"));
    }
    Ok(specs)
}

/// Split a body line like `version "1.2.3"` or `foo "^1.0.0"` into (key, value).
/// Values may be quoted or unquoted.
fn split_key_value(line: &str) -> Option<(String, String)> {
    let (key, rest) = line.split_once(char::is_whitespace)?;
    let value = rest.trim();
    Some((
        unquote_yarn_scalar(key).to_string(),
        unquote_yarn_scalar(value).to_string(),
    ))
}

fn unquote_yarn_scalar(value: &str) -> &str {
    if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn yarn_resolved_tarball_url(resolved: &str) -> Option<String> {
    let url = resolved.split_once('#').map_or(resolved, |(url, _)| url);
    if crate::parse_git_spec(url).is_some() {
        return None;
    }
    if !yarn_resolved_tarball_url_needs_preserve(url) {
        return None;
    }
    LocalSource::looks_like_remote_tarball_url(url).then(|| url.to_string())
}

fn yarn_resolved_tarball_url_needs_preserve(url: &str) -> bool {
    let Some(host) = http_url_host(url) else {
        return false;
    };
    !matches!(host.as_str(), "registry.npmjs.org" | "registry.yarnpkg.com")
}

fn http_url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    Some(
        host_port
            .split_once(':')
            .map_or(host_port, |(host, _)| host)
            .to_ascii_lowercase(),
    )
}

/// Extract the package name from a spec like `foo@^1.0.0` or `@scope/pkg@^1.0.0`.
pub(super) fn parse_spec_name(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix('@') {
        // Scoped package: find the '@' that comes after the '/'
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = spec.find('@')?;
        Some(spec[..at].to_string())
    }
}

/// Detect a yarn npm-protocol alias spec like
/// `<alias>@npm:<real-name>@<version-or-range>` and return the real
/// registry name. Returns `None` for non-aliased specs (the common case).
///
/// Yarn lets a consumer rename a dep on import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under `node_modules/react-loadable/`.
/// The lockfile records the alias as the spec key; without surfacing the
/// real name into [`LockedPackage::alias_of`], every registry/store call
/// site would hit the alias-qualified URL and 404.
pub(super) fn parse_npm_alias_real_name(spec: &str) -> Option<String> {
    let after_alias = if let Some(rest) = spec.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        &after_slash[at + 1..]
    } else {
        let at = spec.find('@')?;
        &spec[at + 1..]
    };
    let after_protocol = after_alias.strip_prefix("npm:")?;
    if let Some(rest) = after_protocol.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = after_protocol.find('@')?;
        Some(after_protocol[..at].to_string())
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn.lock v1
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn v1 lockfile.
///
/// yarn v1 is flat — unlike npm or bun, there's no nested install
/// path. Every `(name, version)` pair gets exactly one block whose
/// header is a comma-separated list of every spec that resolves to
/// it, mirroring yarn's own output: the manifest ranges of direct
/// deps and the declared ranges of every consuming package. A pin
/// (`name@version`) only appears when something references the entry
/// by pin — a manifest that pins, or a dependency rendered from its
/// resolved version because the source lockfile carried no declared
/// ranges — or when no range points at the entry at all.
///
/// Peer-contextualized variants collapse to a single `name@version`
/// entry (yarn v1's data model has no peer context). `resolved` keeps
/// the verbatim string (with its `#sha1` fragment) read from an
/// earlier yarn.lock; entries without one get the registry tarball
/// URL minus the fragment, which yarn tolerates and fills in later.
pub fn write_classic(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants: one entry per canonical (name, version).
    let canonical = crate::build_canonical_map(graph);

    // Keyed by canonical key; values are every spec string that resolves
    // to it. Identical ranges from multiple consumers collapse.
    let mut specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let root_importer_specs = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter());
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        // Look up the range the manifest currently uses for this dep.
        let range = root_importer_specs
            .clone()
            .find(|(n, _)| n.as_str() == dep.name.as_str())
            .map(|(_, r)| r.clone());
        if let Some(range) = range {
            specs
                .entry(canonical_key)
                .or_default()
                .insert(format!("{}@{range}", dep.name));
        }
    }
    // Each package's `declared_dependencies[name] = range` is the range
    // its own manifest uses; the canonical the range resolves to is
    // whatever the resolver already placed under `pkg.dependencies[name]`.
    // Without a declared range the dependency is rendered as its pin,
    // so the pin has to be a header spec for reparse to find it.
    for pkg in canonical.values() {
        for (dep_name, resolved_value) in &pkg.dependencies {
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            if !canonical.contains_key(&target) {
                continue;
            }
            let spec = match pkg.declared_dependencies.get(dep_name) {
                Some(range) => format!("{dep_name}@{range}"),
                None => format!(
                    "{dep_name}@{}",
                    crate::npm::dep_value_as_version(dep_name, resolved_value)
                ),
            };
            specs.entry(target).or_default().insert(spec);
        }
    }

    let mut blocks: Vec<(String, String)> = Vec::with_capacity(canonical.len());
    for (canonical_key, pkg) in &canonical {
        let header_specs: Vec<&str> = match specs.get(canonical_key) {
            Some(set) if !set.is_empty() => set.iter().map(String::as_str).collect(),
            _ => vec![canonical_key.as_str()],
        };
        let sort_key = header_specs.join(", ");
        let mut out = String::with_capacity(256);
        let header: Vec<String> = header_specs.iter().map(|s| yarn_scalar(s)).collect();
        out.push_str(&header.join(", "));
        out.push_str(":\n");

        out.push_str("  version ");
        out.push_str(&yarn_scalar(&pkg.version));
        out.push('\n');

        if let Some(resolved) = yarn_resolved_field(pkg) {
            out.push_str("  resolved ");
            out.push_str(&yarn_scalar(&resolved));
            out.push('\n');
        }

        if let Some(integ) = &pkg.integrity {
            out.push_str("  integrity ");
            out.push_str(&yarn_scalar(integ));
            out.push('\n');
        }

        // `  dependencies:` block — prefer the declared range from the
        // package's own manifest (what yarn itself writes) over the
        // resolved pin. Falls back to the pin when the source
        // lockfile didn't carry declared ranges (e.g. pnpm → yarn).
        let nonempty_deps: BTreeMap<&str, String> = pkg
            .dependencies
            .iter()
            .filter_map(|(n, v)| {
                let key = crate::npm::child_canonical_key(n, v);
                if !canonical.contains_key(&key) {
                    return None;
                }
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| crate::npm::dep_value_as_version(n, v).to_string());
                Some((n.as_str(), rendered))
            })
            .collect();
        if !nonempty_deps.is_empty() {
            out.push_str("  dependencies:\n");
            for (dep_name, dep_version) in &nonempty_deps {
                out.push_str("    ");
                out.push_str(&yarn_scalar(dep_name));
                out.push(' ');
                out.push_str(&yarn_scalar(dep_version));
                out.push('\n');
            }
        }

        blocks.push((sort_key, out));
    }
    blocks.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::with_capacity(canonical.len().saturating_mul(256).max(4096));
    out.push_str("# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n");
    out.push_str("# yarn lockfile v1\n\n\n");
    for (i, (_, block)) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(block);
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}

/// The `resolved` value yarn would have written for `pkg`: the verbatim
/// string from the yarn.lock it was read from, else its tarball URL, else
/// the default registry's tarball URL for a plain registry package.
fn yarn_resolved_field(pkg: &LockedPackage) -> Option<String> {
    if let Some(resolved) = pkg
        .extra_meta
        .get(EXTRA_YARN_RESOLVED)
        .and_then(|v| v.as_str())
    {
        return Some(resolved.to_string());
    }
    if let Some(url) = &pkg.tarball_url {
        return Some(url.clone());
    }
    if pkg.local_source.is_some() {
        return None;
    }
    let name = pkg.registry_name();
    let base = name.rsplit('/').next().unwrap_or(name);
    Some(format!(
        "https://registry.yarnpkg.com/{name}/-/{base}-{}.tgz",
        pkg.version
    ))
}

/// Quote `value` the way yarn v1's stringifier does: bare unless it
/// starts with a non-letter, starts with `true`/`false`, or contains
/// whitespace or any of `:\",[]`.
fn yarn_scalar(value: &str) -> String {
    let needs_quotes = value.starts_with("true")
        || value.starts_with("false")
        || !value.starts_with(|c: char| c.is_ascii_alphabetic())
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ':' | '\\' | '"' | ',' | '[' | ']'));
    if needs_quotes {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}
