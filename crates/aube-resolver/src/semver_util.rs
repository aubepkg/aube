use aube_registry::Packument;

/// Outcome of [`pick_version`]. Distinguishes "nothing in the range
/// at all" from "the cutoff filtered every otherwise-satisfying
/// version" so the caller can surface a meaningful strict-mode error
/// instead of pretending the range itself was wrong.
#[derive(Debug)]
pub enum PickResult<'a> {
    Found(&'a aube_registry::VersionMetadata),
    NoMatch,
    /// Strict mode (or any caller treating the cutoff as a hard wall):
    /// at least one version satisfied the range, but all of them were
    /// filtered out by the cutoff.
    AgeGated,
}

#[cfg(test)]
impl<'a> PickResult<'a> {
    pub(crate) fn unwrap(self) -> &'a aube_registry::VersionMetadata {
        match self {
            PickResult::Found(m) => m,
            other => panic!("expected PickResult::Found, got {other:?}"),
        }
    }
}

/// Single-package version pick for `aube add`'s manifest step, honoring
/// `minimumReleaseAge` with the same dist-tag preference, exemption, and
/// strict/lenient fallback semantics [`pick_version`] applies inside full
/// resolution. Without it, `add` writes the freshly published version into
/// the manifest as a pinned spec, which the resolver's lenient fallback then
/// honors — bypassing the very gate `minimumReleaseAge` exists to provide.
///
/// `registry_name` keys the `minimumReleaseAgeExclude` match: the real
/// registry identity, not a user-facing alias. Pass `None` for
/// `minimum_release_age` to get today's ungated pick (dist-tag preference,
/// then highest satisfying).
///
/// A gated `latest` range is normalized to `*` here, at the API boundary,
/// so no caller can reintroduce the bypass: [`pick_version`]'s internal
/// dist-tag fallback turns `latest` into the tagged version's exact range,
/// whose lenient fallback would admit a fresh publish — the very thing the
/// gate exists to block. `*` keeps the dist-tag preference for a mature
/// `latest`, steers a gated one to the newest version clearing the cutoff,
/// and (unlike the tag) still resolves when `dist-tags.latest` is missing.
pub fn pick_version_for_add<'a>(
    packument: &'a Packument,
    registry_name: &str,
    range: &str,
    minimum_release_age: Option<&crate::MinimumReleaseAge>,
) -> PickResult<'a> {
    let cutoff = minimum_release_age.and_then(|m| m.cutoff());
    let range = registry_alias_range(range);
    let range = if range == "latest" && cutoff.is_some() {
        "*"
    } else {
        range
    };
    let strict = minimum_release_age.is_some_and(|m| m.strict);
    let exclude = minimum_release_age.map(|m| &m.exclude);
    let is_age_exempt = |ver: &str, parsed: Option<&node_semver::Version>| {
        exclude.is_some_and(|ex| match parsed {
            Some(v) => ex.matches(registry_name, v),
            None => match node_semver::Version::parse(ver) {
                Ok(v) => ex.matches(registry_name, &v),
                Err(_) => ex.matches_name_only(registry_name),
            },
        })
    };
    pick_version(
        packument,
        range,
        None,
        false,
        cutoff.as_deref(),
        None,
        strict,
        is_age_exempt,
    )
}

/// Return the version tail from an `npm:` alias spec. Resolution preprocesses
/// this protocol before picking; report-only callers use this entry point
/// directly, so normalize it here as well.
fn registry_alias_range(range: &str) -> &str {
    if let Some(rest) = range.strip_prefix("npm:") {
        match rest.rfind('@') {
            Some(at) if at > 0 => &rest[at + 1..],
            _ => "latest",
        }
    } else {
        range
    }
}

/// Pick the best version from a packument that satisfies the given range.
///
/// `pick_lowest` flips the scan order — used by
/// `resolution-mode=time-based` for direct deps. `cutoff` filters out
/// versions whose registry publish time is later than the cutoff
/// (lexicographic compare on ISO-8601 UTC strings, which sort
/// correctly). When the packument has no `time` entry for a version
/// (e.g. abbreviated corgi payload in `Highest` mode), the cutoff is
/// ignored and the version stays eligible.
///
/// `strict` controls fallback when the cutoff filters out every
/// satisfying version: with `strict=true` we return `None` and the
/// caller errors out; with `strict=false` (the pnpm default) we make a
/// second pass that picks the *lowest* satisfying version ignoring the
/// cutoff. The lowest-satisfying fallback is pnpm's deliberate choice
/// — the oldest version in the range is least likely to be the freshly
/// pushed compromise that triggered the filter in the first place.
///
/// `is_age_exempt` lets the caller wave a specific version past the
/// cutoff — used to honor `minimumReleaseAgeExclude` (bare names, name
/// globs, and exact-version unions). It receives the candidate version
/// string plus its already-parsed form when the caller has one (every
/// hot-path call site here does, so the exemption check needn't reparse),
/// and returns `true` to treat that version as if it cleared the cutoff.
/// Pass `|_, _| false` when no exemptions apply.
///
/// `exempt_cutoff` is the time-based hard wall applied to exempt
/// versions: a version waved past the age-gate by `is_age_exempt` must
/// still clear `exempt_cutoff` (the time-based resolution cutoff). Pass
/// `None` to fully bypass the cutoff for exempt versions (no time-based
/// wall in effect).
///
/// Deprecated versions stay eligible but never win over a
/// non-deprecated one in the same range — see [`outranks`].
#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn pick_version<'a>(
    packument: &'a Packument,
    range_str: &str,
    locked: Option<&str>,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    strict: bool,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> PickResult<'a> {
    match pick_version_key(
        VersionIndex {
            versions: &packument.versions,
            dist_tags: &packument.dist_tags,
            time: &packument.time,
        },
        range_str,
        locked,
        pick_lowest,
        cutoff,
        exempt_cutoff,
        strict,
        is_age_exempt,
    ) {
        VersionPick::Found(key) => match packument.versions.get(key) {
            Some(metadata) => PickResult::Found(metadata),
            None => PickResult::NoMatch,
        },
        VersionPick::NoMatch => PickResult::NoMatch,
        VersionPick::AgeGated => PickResult::AgeGated,
    }
}

/// Information needed to rank releases, without their dependency metadata.
pub(crate) trait VersionCandidate {
    fn is_deprecated(&self) -> bool;
}

impl VersionCandidate for aube_registry::VersionMetadata {
    fn is_deprecated(&self) -> bool {
        self.deprecated.is_some()
    }
}

pub(crate) struct VersionIndex<'a, V> {
    pub(crate) versions: &'a std::collections::BTreeMap<String, V>,
    pub(crate) dist_tags: &'a std::collections::BTreeMap<String, String>,
    pub(crate) time: &'a std::collections::BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VersionPick<'a> {
    Found(&'a str),
    NoMatch,
    AgeGated,
}

/// Select a registry map key before loading full metadata for that release.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pick_version_key<'a, V: VersionCandidate>(
    packument: VersionIndex<'a, V>,
    range_str: &str,
    locked: Option<&str>,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    strict: bool,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> VersionPick<'a> {
    // Handle dist-tag references. If the requested range is a tag
    // name and the packument has that tag, use the tagged version
    // as the effective range. Special case `latest`: some registries
    // serve packuments where dist-tags.latest is absent (fresh
    // publish race, all versions deprecated, private mirror bug).
    // Old code then tried to parse "latest" as a semver range,
    // failed, returned NoMatch. Caller could not tell whether the
    // range was genuinely unsatisfiable or the tag was just missing.
    // npm and pnpm fall back to the highest non-prerelease version.
    // Do the same so `aube install foo` does not silently fail on a
    // packument that just happens to lack the tag.
    let range = match node_semver::Range::parse(normalize_range(range_str)) {
        Ok(r) => r,
        Err(_) => {
            // Reject protocol-prefixed ranges that survived workspace /
            // catalog / npm-alias preprocessing. An attacker can register
            // a dist-tag literally named `workspace:*` or `catalog:` on
            // a package they publish; without this gate the dist-tag
            // fallback below would resolve the protocol spec to whatever
            // version they pinned (dependency-confusion class). npm's
            // own dist-tag rules forbid colon in tag names but the
            // registry does not enforce that.
            if looks_like_protocol_range(range_str) {
                return VersionPick::NoMatch;
            }
            let effective_range = if let Some(tagged_version) = packument.dist_tags.get(range_str) {
                tagged_version.clone()
            } else if range_str == "latest" {
                match highest_stable_key(packument.versions.keys()) {
                    Some(v) => v,
                    None => return VersionPick::NoMatch,
                }
            } else {
                return VersionPick::NoMatch;
            };
            match node_semver::Range::parse(normalize_range(&effective_range)) {
                Ok(r) => r,
                Err(_) => return VersionPick::NoMatch,
            }
        }
    };

    // Does `ver` clear `effective` (the cutoff that applies to it)?
    // `None` => no wall, keep the version. Missing time => keep it: we'd
    // rather risk a slightly newer transitive than fail to resolve the
    // range entirely.
    let passes_effective_cutoff = |ver: &str, effective: Option<&str>| -> bool {
        let Some(c) = effective else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            None => true,
        }
    };

    // A version's effective cutoff: exempt versions answer to the
    // time-based wall (`exempt_cutoff`) only; everyone else answers to
    // the merged `cutoff`.
    let passes_cutoff = |ver: &str, parsed: Option<&node_semver::Version>| -> bool {
        let effective = if is_age_exempt(ver, parsed) {
            exempt_cutoff
        } else {
            cutoff
        };
        passes_effective_cutoff(ver, effective)
    };

    // Prefer locked version if it satisfies and clears the cutoff.
    if let Some(locked_ver) = locked
        && let Ok(v) = node_semver::Version::parse(locked_ver)
        && v.satisfies(&range)
        && passes_cutoff(locked_ver, Some(&v))
        && let Some((key, _)) = packument.versions.get_key_value(locked_ver)
    {
        return VersionPick::Found(key);
    }

    // If `dist-tags.latest` satisfies the range, prefer it over the
    // strictly-highest matching version. Matches npm and pnpm: a fresh
    // `npm install foo@^1.0.0` returns the version the publisher last
    // tagged `latest`, not whatever happens to be the highest in the
    // version list (which can be a stray prerelease, hotfix on an old
    // line, or unwithdrawn experimental publish). Skipped when
    // `pick_lowest` is on (TimeBased mode wants the floor of the range,
    // not the publisher's preferred build).
    if !pick_lowest
        && let Some(latest_ver) = packument.dist_tags.get("latest")
        && let Ok(v) = node_semver::Version::parse(latest_ver)
        && v.satisfies(&range)
        && passes_cutoff(latest_ver, Some(&v))
        && let Some((key, _)) = packument.versions.get_key_value(latest_ver)
    {
        return VersionPick::Found(key);
    }

    // Track whether *any* version satisfied the range — if so but
    // every one was rejected by the cutoff, the failure is age-gate
    // related, not a real "no match in range".
    let mut had_satisfying_but_age_gated = false;

    let mut best: Option<(node_semver::Version, (&'a str, bool))> = None;
    let mut fallback_lowest: Option<(node_semver::Version, (&'a str, bool))> = None;

    for (ver_str, meta) in packument.versions {
        let candidate = (ver_str.as_str(), meta.is_deprecated());
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }

        // The lenient fallback drops the minimumReleaseAge gate but never
        // the time-based hard wall, so only versions that clear
        // `exempt_cutoff` are eligible (a no-op `None` when time-based
        // mode is off).
        if passes_effective_cutoff(ver_str, exempt_cutoff)
            && outranks_status(
                &v,
                candidate.1,
                fallback_lowest.as_ref().map(|(v, m)| (v, m.1)),
                true,
            )
        {
            fallback_lowest = Some((v.clone(), candidate));
        }

        if passes_cutoff(ver_str, Some(&v)) {
            if outranks_status(
                &v,
                candidate.1,
                best.as_ref().map(|(v, m)| (v, m.1)),
                pick_lowest,
            ) {
                best = Some((v, candidate));
            }
        } else {
            had_satisfying_but_age_gated = true;
        }
    }

    if let Some((_, (key, _))) = best {
        return VersionPick::Found(key);
    }

    // Strict mode (or no cutoff active): give up. Distinguish age-gate
    // failures so the caller can surface a meaningful error instead of
    // pretending the range itself was wrong.
    if strict || cutoff.is_none() {
        return if had_satisfying_but_age_gated {
            VersionPick::AgeGated
        } else {
            VersionPick::NoMatch
        };
    }

    // Lenient fallback: pnpm's `pickPackageFromMetaUsingTime` bypasses
    // the minimumReleaseAge gate and picks the *lowest* satisfying
    // version (lowest non-deprecated, per `outranks`) — the candidate
    // already cleared the time-based wall above.
    if let Some((_, (key, _))) = fallback_lowest {
        return VersionPick::Found(key);
    }
    // Nothing left: either the range was unsatisfiable, or the
    // time-based wall excluded every satisfying version. Report the age
    // gate in the latter case so the caller surfaces a meaningful error
    // rather than a bogus "no matching version".
    if had_satisfying_but_age_gated {
        VersionPick::AgeGated
    } else {
        VersionPick::NoMatch
    }
}

/// Does the candidate `(v, meta)` beat the incumbent pick?
///
/// A non-deprecated version outranks a deprecated one whatever their
/// order; between two versions of equal deprecation status `lowest`
/// decides the direction. This is pnpm's `pickVersionByVersionRange`
/// rule — when the highest match carries a `deprecated` message it
/// re-runs the range match over the non-deprecated versions and only
/// keeps the deprecated pick when the range admits nothing else —
/// expressed as a comparison so every scan in this crate gets it from
/// one place. Real case: `codemirror@6.65.7` is an accidentally
/// mis-tagged republish of `5.65.7`, so it sorts above every genuine
/// 6.x and a plain highest-satisfying scan hands it to anyone whose
/// range reaches past `dist-tags.latest`.
///
/// pnpm applies the rule on its highest-version path only; aube applies
/// it in both directions on purpose. A deprecated floor is no safer
/// than a non-deprecated one, so `resolution-mode=time-based` has
/// nothing to gain from pinning a version the publisher withdrew.
#[inline]
pub(crate) fn outranks(
    v: &node_semver::Version,
    meta: &aube_registry::VersionMetadata,
    incumbent: Option<&(node_semver::Version, &aube_registry::VersionMetadata)>,
    lowest: bool,
) -> bool {
    outranks_status(
        v,
        meta.deprecated.is_some(),
        incumbent.map(|(version, metadata)| (version, metadata.deprecated.is_some())),
        lowest,
    )
}

fn outranks_status(
    v: &node_semver::Version,
    deprecated: bool,
    incumbent: Option<(&node_semver::Version, bool)>,
    lowest: bool,
) -> bool {
    let Some((cur_v, cur_deprecated)) = incumbent else {
        return true;
    };
    if deprecated != cur_deprecated {
        return !deprecated;
    }
    if lowest { v < cur_v } else { v > cur_v }
}

/// Walk the packument's versions and return the highest non
/// prerelease version string. Used as the `latest` tag fallback
/// when the registry response lacks `dist-tags.latest`. Some
/// private mirrors and mid-publish races drop the tag briefly
/// and returning NoMatch there would break `aube install foo` for
/// no real reason. npm and pnpm both fall back to highest stable.
#[inline]
/// True when `range_str` looks like a non-registry protocol selector
/// that should never reach the dist-tag fallback (workspace / catalog
/// / file / link / npm-alias / jsr-alias / git / http(s)). Lowercased
/// so an attacker dist-tag named `Workspace:*` cannot bypass the gate.
fn looks_like_protocol_range(range_str: &str) -> bool {
    let Some(idx) = range_str.find(':') else {
        return false;
    };
    let prefix = range_str[..idx].to_ascii_lowercase();
    matches!(
        prefix.as_str(),
        "workspace"
            | "catalog"
            | "npm"
            | "jsr"
            | "file"
            | "link"
            | "git"
            | "git+ssh"
            | "git+http"
            | "git+https"
            | "git+file"
            | "ssh"
            | "http"
            | "https"
            | "github"
            | "gitlab"
            | "bitbucket"
            | "gist"
    )
}

#[inline]
pub(crate) fn highest_stable_version(packument: &Packument) -> Option<String> {
    highest_stable_key(packument.versions.keys())
}

fn highest_stable_key<'a>(keys: impl Iterator<Item = &'a String>) -> Option<String> {
    let mut best: Option<(node_semver::Version, String)> = None;
    for key in keys {
        let Ok(v) = node_semver::Version::parse(key) else {
            continue;
        };
        // Skip prereleases so we match npm semantics. Registry
        // with only prereleases returns None and caller gets
        // NoMatch, same as before.
        if !v.pre_release.is_empty() {
            continue;
        }
        match &best {
            None => best = Some((v, key.clone())),
            Some((cur, _)) if v > *cur => best = Some((v, key.clone())),
            _ => {}
        }
    }
    best.map(|(_, k)| k)
}
/// Extract the trailing `@<version>` from an `npm:<name>@<version>`
/// or `jsr:<name>@<version>` alias spec. Returns the input unchanged
/// when the spec isn't an alias or doesn't carry a version tail.
#[inline]
pub(crate) fn strip_alias_prefix(range: &str) -> &str {
    for prefix in ["npm:", "jsr:"] {
        if let Some(rest) = range.strip_prefix(prefix) {
            return match rest.rfind('@') {
                Some(at) if at > 0 => &rest[at + 1..],
                _ => rest,
            };
        }
    }
    range
}

#[inline]
pub(crate) fn version_satisfies(version: &str, range_str: &str) -> bool {
    with_cached_version(version, |v| {
        let Some(v) = v else { return false };
        with_cached_range(normalize_range(range_str), |r| match r {
            Some(r) => v.satisfies(r),
            None => false,
        })
    })
}

/// npm / pnpm / yarn all treat an empty or whitespace-only version
/// range as equivalent to `"*"` (match any). `node_semver` rejects it
/// with `No valid ranges could be parsed`. Normalize here so the
/// resolver and every `version_satisfies` caller agree with the
/// upstream registry semantics. Real-world case: `hashring@0.0.8`
/// declares `"bisection": ""` in its dependencies.
pub(crate) fn normalize_range(range_str: &str) -> &str {
    if range_str.trim().is_empty() {
        "*"
    } else {
        range_str
    }
}

/// Thread-local `node_semver::Range` parse cache.
///
/// Resolver hot loops (sibling dedupe, lockfile-reuse scan,
/// peer-context fixed-point, catalog pick) call `version_satisfies`
/// thousands of times against a small repeating range set
/// (`"^18.2.0"`, `"*"`, `"1.x"`). Re-parsing burns CPU. Memo turns
/// 15k reparses on a 500-pkg graph into ~500 parses plus hits.
///
/// `thread_local!` beats a global mutex. Each tokio worker owns its
/// slice of ranges, lock contention would erase the parse savings.
/// Two workers parsing the same range twice is cheaper than one
/// lock round-trip.
fn with_cached_range<R>(range_str: &str, f: impl FnOnce(Option<&node_semver::Range>) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<crate::FxHashMap<String, Option<node_semver::Range>>> =
            std::cell::RefCell::default();
    }
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(range_str) {
            let parsed = node_semver::Range::parse(range_str).ok();
            map.insert(range_str.to_string(), parsed);
        }
        f(map.get(range_str).and_then(Option::as_ref))
    })
}

// Mirrors with_cached_range. Locked-version side hits same string
// thousands of times across peer-context + dedupe passes. Hit rate
// trends to 1.0 after first BFS layer.
fn with_cached_version<R>(version: &str, f: impl FnOnce(Option<&node_semver::Version>) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<crate::FxHashMap<String, Option<node_semver::Version>>> =
            std::cell::RefCell::default();
    }
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(version) {
            let parsed = node_semver::Version::parse(version).ok();
            map.insert(version.to_string(), parsed);
        }
        f(map.get(version).and_then(Option::as_ref))
    })
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Candidate {
        deprecated: bool,
    }
    impl VersionCandidate for Candidate {
        fn is_deprecated(&self) -> bool {
            self.deprecated
        }
    }

    #[test]
    fn compact_candidates_preserve_selection_policies_without_dependency_metadata() {
        let versions: BTreeMap<_, _> = [
            ("1.0.0".to_owned(), Candidate { deprecated: true }),
            ("1.1.0".to_owned(), Candidate { deprecated: false }),
            ("2.0.0".to_owned(), Candidate { deprecated: false }),
            ("3.0.0-beta.1".to_owned(), Candidate { deprecated: false }),
        ]
        .into();
        let mut tags: BTreeMap<String, String> = [
            ("latest".into(), "2.0.0".into()),
            ("beta".into(), "3.0.0-beta.1".into()),
            ("workspace:*".into(), "1.1.0".into()),
        ]
        .into();
        let times: BTreeMap<String, String> = [
            ("1.0.0".into(), "2026-01-01".into()),
            ("1.1.0".into(), "2026-02-01".into()),
            ("2.0.0".into(), "2026-09-01".into()),
        ]
        .into();
        let pick = |range, locked, lowest, cutoff, wall, strict, exempt| {
            pick_version_key(
                VersionIndex {
                    versions: &versions,
                    dist_tags: &tags,
                    time: &times,
                },
                range,
                locked,
                lowest,
                cutoff,
                wall,
                strict,
                |v, _| exempt && v == "2.0.0",
            )
        };
        assert_eq!(
            pick("*", None, false, None, None, true, false),
            VersionPick::Found("2.0.0")
        );
        assert_eq!(
            pick("*", Some("1.0.0"), false, None, None, true, false),
            VersionPick::Found("1.0.0")
        );
        assert_eq!(
            pick("*", None, true, None, None, true, false),
            VersionPick::Found("1.1.0")
        );
        assert_eq!(
            pick("beta", None, false, None, None, true, false),
            VersionPick::Found("3.0.0-beta.1")
        );
        assert_eq!(
            pick("workspace:*", None, false, None, None, true, false),
            VersionPick::NoMatch
        );
        assert_eq!(
            pick("^4", None, false, None, None, true, false),
            VersionPick::NoMatch
        );
        assert_eq!(
            pick("*", None, false, Some("2026-06-01"), None, true, false),
            VersionPick::Found("1.1.0")
        );
        assert_eq!(
            pick("*", None, false, Some("2026-06-01"), None, true, true),
            VersionPick::Found("2.0.0")
        );
        assert_eq!(
            pick(
                "*",
                None,
                false,
                Some("2026-06-01"),
                Some("2026-03-01"),
                true,
                true
            ),
            VersionPick::Found("1.1.0")
        );
        assert_eq!(
            pick("*", None, false, Some("2025-01-01"), None, true, false),
            VersionPick::AgeGated
        );
        assert_eq!(
            pick("*", None, false, Some("2025-01-01"), None, false, false),
            VersionPick::Found("1.1.0")
        );
        assert_eq!(
            pick(
                "*",
                None,
                false,
                Some("2025-01-01"),
                Some("2025-01-01"),
                false,
                false
            ),
            VersionPick::AgeGated
        );
        tags.remove("latest");
        assert_eq!(
            pick_version_key(
                VersionIndex {
                    versions: &versions,
                    dist_tags: &tags,
                    time: &times
                },
                "latest",
                None,
                false,
                None,
                None,
                true,
                |_, _| false
            ),
            VersionPick::Found("2.0.0")
        );
    }

    #[test]
    fn metadata_lookup_uses_registry_key_when_embedded_version_differs() {
        let full: Packument=serde_json::from_value(serde_json::json!({
            "name":"sample", "versions":{"1.0.0":{"name":"sample","version":"different","dependencies":{"kept":"^2"}}}
        })).unwrap();
        let PickResult::Found(metadata) =
            pick_version(&full, "1.0.0", None, false, None, None, true, |_, _| false)
        else {
            panic!("expected the registry key to match")
        };
        assert_eq!(metadata.version, "different");
        assert_eq!(metadata.dependencies["kept"], "^2");
    }
}
