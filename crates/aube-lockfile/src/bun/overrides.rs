//! bun.lock `overrides` ⇄ aube's pnpm-style override selector keys.
//!
//! A flat bun rule is a string row (`"foo": "1.0.0"`). bun 1.4 writes
//! scoped rules (`lockfileVersion: 3`) as selector-keyed groups:
//!
//! ```jsonc
//! "overrides": {
//!   "foo": { ".": "1.0.0", "bar": "2.0.0" },
//!   "baz@^1": { ".": "1.5.0" },
//! }
//! ```
//!
//! A group's key is the parent selector (`name` or `name@range`). Its
//! `"."` child overrides the parent itself, and every other child is a
//! target scoped to that parent as its direct dependent. That is pnpm's
//! `parent>child`, so the graph keeps the keys the manifest layer and
//! the resolver already understand: `foo`, `foo>bar` and `baz@^1`.

use serde_json::Value;
use std::collections::BTreeMap;

/// Translate bun's `overrides` block into selector keys.
pub(super) fn read(raw: &BTreeMap<String, Value>) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in raw {
        match value {
            Value::String(version) => {
                out.insert(key.clone(), version.clone());
            }
            Value::Object(children) => {
                if split_segment(key).is_none() || pnpm_delimiter(key).is_some() {
                    return Err(format!("invalid override key {key:?}"));
                }
                for (child, child_value) in children {
                    let Value::String(version) = child_value else {
                        return Err(format!(
                            "override {key:?} > {child:?} must be a string, got {child_value}"
                        ));
                    };
                    if child == "." {
                        out.insert(key.clone(), version.clone());
                        continue;
                    }
                    if split_segment(child).is_none() || pnpm_delimiter(child).is_some() {
                        return Err(format!("invalid override key {child:?} under {key:?}"));
                    }
                    out.insert(format!("{key}>{child}"), version.clone());
                }
            }
            other => {
                return Err(format!(
                    "override {key:?} must be a string or an object, got {other}"
                ));
            }
        }
    }
    Ok(out)
}

/// Build bun's `overrides` block from selector keys. Returns the block
/// and whether it holds scoped rules, which bun stamps as
/// `lockfileVersion: 3`.
///
/// Keys bun cannot express (a multi-level `a>b>c` chain, or yarn's
/// `**/foo` and `parent/foo` forms) are written as string rows, as they
/// were before scoped rules existed.
pub(super) fn write(overrides: &BTreeMap<String, String>) -> (Value, bool) {
    // (name, range) of a group or string row → its entry. bun orders
    // both by name, then range text, with a range-less row first.
    let mut entries: BTreeMap<(&str, &str), Entry<'_>> = BTreeMap::new();
    let mut flat: Vec<(&str, &str)> = Vec::new();
    for (key, version) in overrides {
        match classify(key) {
            Some(Rule::Flat { name }) => flat.push((name, version)),
            Some(Rule::Scoped { parent, target }) => {
                let group = entries.entry(parent).or_insert_with(Entry::group);
                if let Entry::Group { children, .. } = group {
                    children.insert(target, version);
                }
            }
            Some(Rule::Ranged { target }) => {
                let group = entries.entry(target).or_insert_with(Entry::group);
                if let Entry::Group { dot, .. } = group {
                    *dot = Some(version);
                }
            }
            None => {
                entries.insert((key, ""), Entry::Row(version));
            }
        }
    }
    // A flat rule folds into its name's unranged group as `"."`.
    for (name, version) in flat {
        match entries.get_mut(&(name, "")) {
            Some(Entry::Group { dot, .. }) => *dot = Some(version),
            _ => {
                entries.insert((name, ""), Entry::Row(version));
            }
        }
    }

    let scoped = entries.values().any(|e| matches!(e, Entry::Group { .. }));
    let mut block = serde_json::Map::new();
    for ((name, range), entry) in entries {
        let key = selector_key(name, range);
        let value = match entry {
            Entry::Row(version) => Value::String(version.to_string()),
            Entry::Group { dot, children } => {
                let mut group = serde_json::Map::new();
                if let Some(version) = dot {
                    group.insert(".".to_string(), Value::String(version.to_string()));
                }
                for ((child, child_range), version) in children {
                    group.insert(
                        selector_key(child, child_range),
                        Value::String(version.to_string()),
                    );
                }
                Value::Object(group)
            }
        };
        block.insert(key, value);
    }
    (Value::Object(block), scoped)
}

enum Entry<'a> {
    Row(&'a str),
    Group {
        dot: Option<&'a str>,
        children: BTreeMap<(&'a str, &'a str), &'a str>,
    },
}

impl Entry<'_> {
    fn group() -> Self {
        Entry::Group {
            dot: None,
            children: BTreeMap::new(),
        }
    }
}

enum Rule<'a> {
    /// `name`
    Flat { name: &'a str },
    /// `name@range`
    Ranged { target: (&'a str, &'a str) },
    /// `parent[@range]>child[@range]`
    Scoped {
        parent: (&'a str, &'a str),
        target: (&'a str, &'a str),
    },
}

fn classify(key: &str) -> Option<Rule<'_>> {
    let Some(delimiter) = pnpm_delimiter(key) else {
        let (name, range) = split_segment(key)?;
        return Some(if range.is_empty() {
            Rule::Flat { name }
        } else {
            Rule::Ranged {
                target: (name, range),
            }
        });
    };
    let child = &key[delimiter + 1..];
    if pnpm_delimiter(child).is_some() {
        return None;
    }
    Some(Rule::Scoped {
        parent: split_segment(&key[..delimiter])?,
        target: split_segment(child)?,
    })
}

fn selector_key(name: &str, range: &str) -> String {
    if range.is_empty() {
        name.to_string()
    } else {
        format!("{name}@{range}")
    }
}

/// Byte offset of a pnpm `parent>child` delimiter: the first `>` not
/// preceded by a space, `|` or `@`, which would make it part of a range
/// (`foo@>=1`, `foo@1 || >2`). Same rule as bun and
/// `aube-resolver`'s selector parser.
fn pnpm_delimiter(key: &str) -> Option<usize> {
    let bytes = key.as_bytes();
    (1..bytes.len()).find(|&i| bytes[i] == b'>' && !matches!(bytes[i - 1], b' ' | b'|' | b'@'))
}

/// Split one `name[@range]` package segment. `None` for anything bun
/// would not accept as one: an empty name, a bare `@scope`, a yarn
/// path (`parent/foo`, `**/foo`), or a trailing `@` with no range.
fn split_segment(segment: &str) -> Option<(&str, &str)> {
    let name_end = match segment.strip_prefix('@') {
        Some(scoped) => {
            let slash = scoped.find('/')?;
            let after = &scoped[slash + 1..];
            1 + slash + 1 + after.find('@').unwrap_or(after.len())
        }
        None => segment.find('@').unwrap_or(segment.len()),
    };
    let name = &segment[..name_end];
    let range = segment[name_end..].strip_prefix('@').unwrap_or("");
    let bare = name.strip_prefix('@').unwrap_or(name);
    if bare.is_empty()
        || bare.ends_with('/')
        || bare.matches('/').count() > usize::from(name.starts_with('@'))
        || (range.is_empty() && name_end < segment.len())
    {
        return None;
    }
    Some((name, range))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn split_segment_accepts_names_and_ranges() {
        assert_eq!(split_segment("foo"), Some(("foo", "")));
        assert_eq!(split_segment("foo@^1"), Some(("foo", "^1")));
        assert_eq!(split_segment("@s/foo"), Some(("@s/foo", "")));
        assert_eq!(split_segment("@s/foo@>=1 <2"), Some(("@s/foo", ">=1 <2")));
        for invalid in ["", "@s", "@s/", "foo@", "parent/foo", "**/foo", "@s/a/b"] {
            assert_eq!(split_segment(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn read_flattens_groups_into_selector_keys() {
        let raw: BTreeMap<String, Value> = serde_json::from_str(
            r#"{
                "plain": "1.0.0",
                "foo": { ".": "2.0.0", "bar": "3.0.0", "@s/baz@<2": "4.0.0" },
                "qux@^1": { ".": "5.0.0" },
                "@s/p@1": { "child@>=2": "6.0.0" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            read(&raw).unwrap(),
            map(&[
                ("plain", "1.0.0"),
                ("foo", "2.0.0"),
                ("foo>bar", "3.0.0"),
                ("foo>@s/baz@<2", "4.0.0"),
                ("qux@^1", "5.0.0"),
                ("@s/p@1>child@>=2", "6.0.0"),
            ])
        );
    }

    #[test]
    fn read_rejects_malformed_groups() {
        for raw in [
            r#"{ "foo": { "bar": { "baz": "1" } } }"#,
            r#"{ "foo": 1 }"#,
            r#"{ "a>b": { ".": "1" } }"#,
            r#"{ "foo": { "a>b": "1" } }"#,
        ] {
            let raw: BTreeMap<String, Value> = serde_json::from_str(raw).unwrap();
            assert!(read(&raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn write_groups_scoped_rules_in_bun_order() {
        let overrides = map(&[
            ("plain", "1.0.0"),
            ("foo", "2.0.0"),
            ("foo>bar", "3.0.0"),
            ("foo-x", "3.5.0"),
            ("foo@^1", "4.0.0"),
            ("qux@^1>child", "5.0.0"),
            ("a>b>c", "6.0.0"),
        ]);
        let (block, scoped) = write(&overrides);
        assert!(scoped);
        assert_eq!(
            serde_json::to_string(&block).unwrap(),
            r#"{"a>b>c":"6.0.0","foo":{".":"2.0.0","bar":"3.0.0"},"foo@^1":{".":"4.0.0"},"foo-x":"3.5.0","plain":"1.0.0","qux@^1":{"child":"5.0.0"}}"#
        );
    }

    #[test]
    fn write_keeps_flat_overrides_as_string_rows() {
        let (block, scoped) = write(&map(&[("a", "1"), ("@s/b", "2")]));
        assert!(!scoped);
        assert_eq!(
            serde_json::to_string(&block).unwrap(),
            r#"{"@s/b":"2","a":"1"}"#
        );
    }

    #[test]
    fn read_and_write_round_trip() {
        let overrides = map(&[
            ("foo", "2.0.0"),
            ("foo>bar", "3.0.0"),
            ("qux@^1", "5.0.0"),
            ("@s/p@1>child@>=2", "6.0.0"),
        ]);
        let (block, _) = write(&overrides);
        let raw: BTreeMap<String, Value> = serde_json::from_value(block).unwrap();
        assert_eq!(read(&raw).unwrap(), overrides);
    }
}
