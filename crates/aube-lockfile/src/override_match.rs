//! Minimal override-key matcher for the importer-level drift check.
//!
//! pnpm rewrites an importer's recorded `specifier` when an override
//! fires on a direct dep — so a manifest that reads `"plist": "^3.0.4"`
//! with override `"plist@<3.0.5": ">=3.0.5"` produces a lockfile that
//! records `specifier: ">=3.0.5"`. `--frozen-lockfile` must apply the
//! same override to the manifest spec before comparing, otherwise
//! every pnpm-written lockfile with overrides reads stale on the next
//! frozen install.
//!
//! The full pnpm/yarn override grammar (parent chains `foo>bar`, yarn
//! wildcards `**/foo`) lives in `aube-resolver::override_rule`. Direct
//! deps of an importer have no ancestor chain by construction, so this
//! matcher only handles the two key shapes that can fire here:
//!
//! - bare name: `lodash`, `@babel/core`
//! - name + version range: `lodash@<4.17.21`, `@scope/pkg@^1`
//!
//! Keys with parent-chain syntax are ignored — they can't match a
//! direct-dep override application.
//!
//! Kept inside aube-lockfile (rather than reaching into aube-resolver)
//! to avoid a cross-crate dep cycle: aube-resolver already depends on
//! aube-lockfile.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct DirectOverrideRule {
    pub name: String,
    pub version_req: Option<String>,
    pub replacement: String,
}

/// Parse and compile a raw `name → replacement` map into rules. Keys
/// with parent-chain selectors (`foo>bar`, `**/foo`, `parent/foo`) are
/// dropped — they only match transitive deps.
///
/// Output is sorted so version-keyed rules come before bare-name rules
/// for the same package. Mirrors pnpm's "more specific selector wins"
/// behavior: when a manifest has both `"plist": "9.9.9"` and
/// `"plist@<3": "2.0.0"`, pnpm picks the version-keyed one for any
/// matching range, and the lockfile records that replacement. A
/// bare-first iteration order would always shadow the version-keyed
/// rule and produce a false `Stale`.
pub(crate) fn compile(raw: &BTreeMap<String, String>) -> Vec<DirectOverrideRule> {
    let mut rules: Vec<DirectOverrideRule> = raw
        .iter()
        .filter_map(|(k, v)| {
            parse_key(k).map(|(n, r)| DirectOverrideRule {
                name: n,
                version_req: r,
                replacement: v.clone(),
            })
        })
        .collect();
    rules.sort_by_key(|r| r.version_req.is_none());
    rules
}

/// Find the first rule whose target matches `(name, spec)` and return
/// its replacement spec. A rule matches when (a) the target name is
/// equal and (b) either the rule has no version req, or the manifest
/// spec's lower-bound version satisfies the rule's req — same probe
/// `aube-resolver::override_rule` uses.
pub(crate) fn apply<'a>(
    rules: &'a [DirectOverrideRule],
    name: &str,
    spec: &str,
) -> Option<&'a str> {
    // An `npm:`/`jsr:` alias is matched on its trailing version range,
    // as the resolver does.
    let spec = strip_alias_prefix(spec);
    rules.iter().find_map(|rule| {
        if rule.name != name {
            return None;
        }
        match rule.version_req.as_deref() {
            None => Some(rule.replacement.as_str()),
            Some(req) if range_could_satisfy(spec, req) => Some(rule.replacement.as_str()),
            _ => None,
        }
    })
}

/// The importer specifier pnpm records for an applied `link:`/`file:`
/// override: the override's root-relative path re-expressed relative to
/// `importer` (a root-relative importer path, `.` for the root), in
/// forward-slash form. `link:./vendor/x` becomes `link:../vendor/x` for
/// importer `pkg-a` and `link:vendor/x` for the root.
///
/// `None` when the value isn't a relative `link:`/`file:` path (absolute
/// and `~` paths name the same place from anywhere) or the importer lies
/// outside the root, where the relative path would have to name the
/// root's own directory.
pub fn importer_relative_override(spec: &str, importer: &str) -> Option<String> {
    let (protocol, path) = ["link:", "file:"]
        .into_iter()
        .find_map(|protocol| spec.strip_prefix(protocol).map(|path| (protocol, path)))?;
    // A leading separator is absolute on every platform, as Node's
    // `path.isAbsolute` reads it on Windows too.
    if path.is_empty()
        || std::path::Path::new(path).is_absolute()
        || path.starts_with(['/', '\\'])
        || path.starts_with("~/")
        || path.starts_with("~\\")
    {
        return None;
    }
    let base = aube_util::path::normalize_lexical(std::path::Path::new(importer));
    if base
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let target = aube_util::path::normalize_lexical(std::path::Path::new(path));
    let relative = pathdiff::diff_paths(&target, &base)?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    Some(if relative.is_empty() {
        format!("{protocol}.")
    } else {
        format!("{protocol}{relative}")
    })
}

/// The version range of an `npm:`/`jsr:` alias spec (`npm:foo@^1` →
/// `^1`), or `spec` itself. Mirrors `aube-resolver`'s helper of the same
/// name.
fn strip_alias_prefix(spec: &str) -> &str {
    for prefix in ["npm:", "jsr:"] {
        if let Some(rest) = spec.strip_prefix(prefix) {
            return match rest.rfind('@') {
                Some(at) if at > 0 => &rest[at + 1..],
                _ => rest,
            };
        }
    }
    spec
}

/// Extract the final package target from any supported pnpm/yarn override key.
/// Unlike [`compile`], this accepts ancestor chains because catalog expansion
/// needs the target name even when the override cannot apply to an importer.
pub(crate) fn target_package_name(key: &str) -> Option<String> {
    let target = split_segments(key)?.pop()?;
    parse_segment(target).map(|(name, _)| name)
}

fn parse_key(key: &str) -> Option<(String, Option<String>)> {
    if key.is_empty() {
        return None;
    }
    // Multi-segment selectors are parent-chain rules (`foo>bar`,
    // `**/foo`, `parent/foo`) — they only fire on transitive deps.
    let segments = split_segments(key)?;
    if segments.len() != 1 {
        return None;
    }
    parse_segment(segments[0])
}

/// Split `key` on pnpm `>` chain separators (and yarn `/` ancestors),
/// while keeping `>` characters that belong to a version comparator
/// (`>=`, `>1.0.0`, `> 1`) attached to the segment they qualify.
/// pnpm treats `>` as a parent delimiter unless the preceding byte is
/// a space, `|`, or `@`; this matches its `/[^ |@]>/` boundary rule.
/// Mirrors `aube-resolver::override_rule::split_segments`.
fn split_segments(key: &str) -> Option<Vec<&str>> {
    let bytes = key.as_bytes();
    let mut pnpm_parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' && i == 0 {
            return None;
        }
        if bytes[i] == b'>' && !matches!(bytes[i - 1], b' ' | b'|' | b'@') {
            if start == i {
                return None;
            }
            pnpm_parts.push(&key[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if start >= bytes.len() {
        return None;
    }
    pnpm_parts.push(&key[start..]);

    // Split each pnpm segment on Yarn `/` ancestors except the
    // scope-introducing slash. This second pass is required even when
    // the key contains `>`: it may be a comparator in a slash-form
    // selector such as `parent/lodash@>=4`.
    let mut out: Vec<&str> = Vec::new();
    for part in pnpm_parts {
        split_slash_segments(part, &mut out)?;
    }
    Some(out)
}

fn split_slash_segments<'a>(part: &'a str, out: &mut Vec<&'a str>) -> Option<()> {
    let bytes = part.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            let current = &part[start..i];
            let scope = current.starts_with('@') && !current[1..].contains('/');
            if !scope {
                if current.is_empty() {
                    return None;
                }
                out.push(current);
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail = &part[start..];
    if tail.is_empty() {
        return None;
    }
    out.push(tail);
    Some(())
}

/// Parse a single segment `name[@range]` (scoped or unscoped) into its
/// (name, req) pair. Wildcards (`**`) are rejected — they're a
/// parent-chain construct that has no meaning at the importer level.
fn parse_segment(seg: &str) -> Option<(String, Option<String>)> {
    if seg == "**" {
        return None;
    }
    if let Some(after_at) = seg.strip_prefix('@') {
        let slash = after_at.find('/')?;
        let rest = &after_at[slash + 1..];
        if rest.is_empty() {
            return None;
        }
        if let Some(at) = rest.find('@') {
            let pkg_tail = &rest[..at];
            let req = &rest[at + 1..];
            if pkg_tail.is_empty() || req.is_empty() {
                return None;
            }
            Some((
                format!("@{}/{}", &after_at[..slash], pkg_tail),
                Some(req.to_string()),
            ))
        } else {
            Some((format!("@{after_at}"), None))
        }
    } else if let Some(at) = seg.find('@') {
        if at == 0 {
            return None;
        }
        let name = &seg[..at];
        let req = &seg[at + 1..];
        if name.is_empty() || req.is_empty() {
            return None;
        }
        Some((name.to_string(), Some(req.to_string())))
    } else {
        Some((seg.to_string(), None))
    }
}

/// Lower-bound probe. Mirrors `aube-resolver::override_rule::range_could_satisfy`
/// without the cross-crate dep. A range whose extractable lower bound
/// satisfies the req counts as a hit. A spec that isn't a semver range
/// (`link:`, `file:`, a git URL, a dist-tag) matches only a req spelled
/// the same, as in pnpm. Semver ranges we can't take a lower bound from
/// fall through to "probably matches" so a user override is never
/// silently dropped.
///
/// Exclusive `>X.Y.Z` is special-cased: trimming the prefix yields the
/// boundary itself, which fails any `<X.Y.Z` req and would otherwise
/// fall through to "probably matches" — spuriously firing the override
/// for two ranges with empty intersection. The caller signals the
/// exclusive form via a separate try with `bumped_lower_bound` first.
fn range_could_satisfy(task_range: &str, req: &str) -> bool {
    let declared = task_range.trim();
    if !declared.is_empty() && node_semver::Range::parse(declared).is_err() {
        return declared == req.trim();
    }
    let Ok(r) = node_semver::Range::parse(req) else {
        return true;
    };
    if let Ok(v) = node_semver::Version::parse(task_range)
        && v.satisfies(&r)
    {
        return true;
    }
    let trimmed = task_range.trim();
    let exclusive = trimmed.starts_with('>') && !trimmed.starts_with(">=");
    if let Some(candidate) = lower_bound_version(trimmed)
        && let Ok(mut v) = node_semver::Version::parse(&candidate)
    {
        if exclusive {
            // Probe just above the exclusive boundary: `>3.0.5` covers
            // every version above 3.0.5, so use 3.0.5 + minimum patch
            // bump as the representative point.
            v.patch += 1;
        }
        return v.satisfies(&r);
    }
    true
}

fn lower_bound_version(range: &str) -> Option<String> {
    let s = range
        .trim()
        .trim_start_matches(['^', '~', '=', '>', 'v', ' ']);
    let end = s.find([' ', ',', '<', '|', '>']).unwrap_or(s.len());
    let v = &s[..end];
    if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn bare_name_matches_any_spec() {
        let rules = compile(&map(&[("lodash", "4.17.21")]));
        assert_eq!(apply(&rules, "lodash", "^4.17.0"), Some("4.17.21"));
        assert_eq!(apply(&rules, "lodash", "*"), Some("4.17.21"));
        assert_eq!(apply(&rules, "other", "^1"), None);
    }

    #[test]
    fn scoped_bare_name() {
        let rules = compile(&map(&[("@babel/core", "7.20.0")]));
        assert_eq!(apply(&rules, "@babel/core", "^7"), Some("7.20.0"));
    }

    #[test]
    fn version_qualified_filters_by_range() {
        let rules = compile(&map(&[("plist@<3.0.5", ">=3.0.5")]));
        assert_eq!(apply(&rules, "plist", "^3.0.4"), Some(">=3.0.5"));
        assert_eq!(apply(&rules, "plist", "^4.0.0"), None);
    }

    #[test]
    fn scoped_with_range() {
        let rules = compile(&map(&[("@scope/pkg@^1", "1.5.0")]));
        assert_eq!(apply(&rules, "@scope/pkg", "^1.0.0"), Some("1.5.0"));
        assert_eq!(apply(&rules, "@scope/pkg", "^2.0.0"), None);
    }

    #[test]
    fn parent_chain_keys_dropped() {
        let rules = compile(&map(&[
            ("foo>bar", "1.0.0"),
            ("**/foo", "1.0.0"),
            ("parent/foo", "1.0.0"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn target_name_supports_pnpm_and_yarn_ancestor_selectors() {
        assert_eq!(target_package_name("parent>foo").as_deref(), Some("foo"));
        assert_eq!(target_package_name("parent/foo").as_deref(), Some("foo"));
        assert_eq!(target_package_name("**/foo").as_deref(), Some("foo"));
        assert_eq!(
            target_package_name("parent/@scope/foo@^1").as_deref(),
            Some("@scope/foo")
        );
        assert_eq!(
            target_package_name("parent/lodash@>=4.0.0").as_deref(),
            Some("lodash")
        );
        assert_eq!(
            target_package_name("parent/@scope/foo@>1.0.0").as_deref(),
            Some("@scope/foo")
        );
        assert_eq!(
            target_package_name("parent@^1>123numeric").as_deref(),
            Some("123numeric")
        );
    }

    #[test]
    fn empty_or_malformed_keys_dropped() {
        let rules = compile(&map(&[
            ("", "1"),
            ("@scope", "1"),
            ("foo@", "1"),
            ("@", "1"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn version_keyed_rule_wins_over_bare_when_both_match() {
        // pnpm picks the more specific selector, so a manifest with
        // both `"plist": "9.9.9"` and `"plist@<3": "2.0.0"` and a dep
        // spec of `^2.0.0` (covered by `<3`) should resolve to
        // `2.0.0`. Bare-first iteration order would silently shadow
        // the version-keyed rule and produce a false `Stale`.
        let rules = compile(&map(&[("plist", "9.9.9"), ("plist@<3", "2.0.0")]));
        assert_eq!(apply(&rules, "plist", "^2.0.0"), Some("2.0.0"));
        // Spec outside the version-keyed rule's range falls through to
        // the bare rule.
        assert_eq!(apply(&rules, "plist", "^4.0.0"), Some("9.9.9"));
    }

    #[test]
    fn key_with_gte_comparator_parses() {
        // `lodash@>=4.17.21` is a single segment whose `>=` is a
        // comparator, not a chain separator. Pre-fix, the parser
        // rejected any key containing `>` and silently dropped this.
        let rules = compile(&map(&[("lodash@>=4.17.21", "4.18.0")]));
        assert_eq!(apply(&rules, "lodash", "4.17.21"), Some("4.18.0"));
        // Lower-bound probe is conservative — concrete version that
        // doesn't satisfy the req falls through, which is fine.
        assert_eq!(apply(&rules, "lodash", "4.0.0"), None);
    }

    #[test]
    fn key_with_gt_comparator_parses() {
        let rules = compile(&map(&[("lodash@>1.0.0", "1.5.0")]));
        assert_eq!(apply(&rules, "lodash", "1.2.0"), Some("1.5.0"));
    }

    #[test]
    fn exclusive_gt_spec_against_lt_req_does_not_overlap() {
        // `>3.0.5` and `<3.0.5` have empty intersection. A naive
        // lower-bound probe would extract `3.0.5` from `>3.0.5` and
        // see it fail `<3.0.5` (boundary excluded), then fall through
        // to "probably matches". The fix is to bump the candidate
        // above the exclusive boundary before satisfaction-probing.
        assert!(!range_could_satisfy(">3.0.5", "<3.0.5"));
        // Sanity: the inclusive case still doesn't overlap.
        assert!(range_could_satisfy(">3.0.5", ">=3.0.5"));
    }

    #[test]
    fn range_rule_skips_non_semver_specs_like_the_resolver() {
        let rules = compile(&map(&[("x@^1", "link:./vendor/x")]));
        assert_eq!(apply(&rules, "x", "link:./other"), None);
        assert_eq!(apply(&rules, "x", "latest"), None);
        assert_eq!(apply(&rules, "x", "^1.2.0"), Some("link:./vendor/x"));
        // An alias is matched on its version range.
        assert_eq!(apply(&rules, "x", "npm:y@^1.0.0"), Some("link:./vendor/x"));
        assert_eq!(apply(&rules, "x", "npm:y@^2.0.0"), None);
    }

    #[test]
    fn importer_relative_override_matches_pnpm() {
        let rel = importer_relative_override;
        assert_eq!(
            rel("link:./vendor/x", "pkg-a").as_deref(),
            Some("link:../vendor/x")
        );
        assert_eq!(
            rel("link:./vendor/x", "packages/a").as_deref(),
            Some("link:../../vendor/x")
        );
        assert_eq!(
            rel("link:./vendor/x", ".").as_deref(),
            Some("link:vendor/x")
        );
        assert_eq!(
            rel("file:./vendor/x", "pkg-a").as_deref(),
            Some("file:../vendor/x")
        );
        assert_eq!(rel("link:./pkg-a", "pkg-a").as_deref(), Some("link:."));
        assert_eq!(
            rel("link:../outside", "pkg-a").as_deref(),
            Some("link:../../outside")
        );
        // Not re-anchored: an absolute or `~` path, a non-local value, or an
        // importer outside the root.
        assert_eq!(rel("link:/abs/x", "pkg-a"), None);
        assert_eq!(rel("link:~/x", "pkg-a"), None);
        assert_eq!(rel("^1.0.0", "pkg-a"), None);
        assert_eq!(rel("link:./vendor/x", "../sibling"), None);
    }
}
