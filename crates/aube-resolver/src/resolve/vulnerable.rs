use aube_registry::Packument;
use std::collections::BTreeMap;

pub(crate) fn is_vulnerable(
    package_name: &str,
    version: &str,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
) -> bool {
    let Some(ranges) = vulnerable_ranges.get(package_name) else {
        return false;
    };
    let Ok(version) = node_semver::Version::parse(version) else {
        return false;
    };
    ranges
        .iter()
        .filter_map(|range| node_semver::Range::parse(range).ok())
        .any(|range| version.satisfies(&range))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prefer_non_vulnerable_pick<'a>(
    package_name: &str,
    packument: &'a Packument,
    range_str: &str,
    fallback: &'a aube_registry::VersionMetadata,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> &'a aube_registry::VersionMetadata {
    if !is_vulnerable(package_name, &fallback.version, vulnerable_ranges) {
        return fallback;
    }
    // Same gated-`latest` handling as `pick_version_key`: a cutoff turns
    // the tag into `*` so a safe mature version can replace a vulnerable one.
    let range_str = if range_str == "latest" && cutoff.is_some() {
        "*"
    } else {
        range_str
    };
    let Ok(range) = node_semver::Range::parse(crate::semver_util::normalize_range(range_str))
    else {
        return fallback;
    };
    // Mirror `pick_version`'s cutoff: a `minimumReleaseAgeExclude` match
    // waves a version past the minimumReleaseAge gate here too (still
    // subject to `exempt_cutoff`, the time-based wall), otherwise the
    // re-pick could discard the exempt safe version and keep the
    // vulnerable one.
    let passes_cutoff = |ver: &str, parsed: Option<&node_semver::Version>| -> bool {
        let effective = if is_age_exempt(ver, parsed) {
            exempt_cutoff
        } else {
            cutoff
        };
        let Some(c) = effective else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            None => true,
        }
    };
    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.satisfies(&range)
            || !passes_cutoff(ver_str, Some(&version))
            || is_vulnerable(package_name, ver_str, vulnerable_ranges)
        {
            continue;
        }
        if crate::semver_util::outranks(&version, meta, best.as_ref(), pick_lowest) {
            best = Some((version, meta));
        }
    }
    best.map(|(_, meta)| meta).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gated_latest_repicks_safe_version_when_tag_is_vulnerable() {
        let packument: Packument = serde_json::from_value(serde_json::json!({
            "name": "foo",
            "versions": {
                "1.9.0": {"name": "foo", "version": "1.9.0"},
                "2.0.0": {"name": "foo", "version": "2.0.0"}
            },
            "dist-tags": {"latest": "2.0.0"},
            "time": {"1.9.0": "2020-01-01T00:00:00.000Z", "2.0.0": "2020-01-02T00:00:00.000Z"}
        }))
        .unwrap();
        let vulnerable = BTreeMap::from([("foo".to_string(), vec!["2.0.0".to_string()])]);
        let picked = prefer_non_vulnerable_pick(
            "foo",
            &packument,
            "latest",
            &packument.versions["2.0.0"],
            false,
            Some("2025-01-01"),
            None,
            &vulnerable,
            |_, _| false,
        );
        assert_eq!(picked.version, "1.9.0");
    }

    #[test]
    fn gated_latest_repicks_stable_when_prerelease_tag_is_vulnerable() {
        let packument: Packument = serde_json::from_value(serde_json::json!({
            "name": "foo",
            "versions": {
                "0.15.0": {"name": "foo", "version": "0.15.0"},
                "1.0.0-beta.1": {"name": "foo", "version": "1.0.0-beta.1"}
            },
            "dist-tags": {"latest": "1.0.0-beta.1"},
            "time": {"0.15.0": "2020-01-01T00:00:00.000Z", "1.0.0-beta.1": "2020-01-02T00:00:00.000Z"}
        }))
        .unwrap();
        let vulnerable = BTreeMap::from([("foo".to_string(), vec!["1.0.0-beta.1".to_string()])]);
        let picked = prefer_non_vulnerable_pick(
            "foo",
            &packument,
            "latest",
            &packument.versions["1.0.0-beta.1"],
            false,
            Some("2025-01-01"),
            None,
            &vulnerable,
            |_, _| false,
        );
        assert_eq!(picked.version, "0.15.0");
    }
}
