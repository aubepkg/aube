//! The v13 syntax: JSON indented with one tab per level, with a trailing
//! comma after every entry of a multi-line collection, in pnpm's key order.
//! Ported from pnpm's `yaml_emit/{flow,next_order,nesting}.rs`.

use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::fmt::Write as _;

/// The top-level key order.
const ROOT_KEYS: [&str; 11] = [
    "lockfileVersion",
    "settings",
    "catalogs",
    "overrides",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "untrackedPnpmfileReadPackageHook",
    "patchedDependencies",
    "importers",
    "projects",
    "packages",
];

/// The key order of a `packages` entry and of its `snapshots` items.
const PACKAGE_KEYS: [&str; 17] = [
    "resolution",
    "integrity",
    "version",
    "engines",
    "cpu",
    "os",
    "libc",
    "hasBin",
    "prepare",
    "bundledDependencies",
    "peerDependencies",
    "specifiers",
    "id",
    "optional",
    "dependencies",
    "optionalDependencies",
    "snapshots",
];

/// The maps `settings` holds after the settings themselves, in this order.
const SETTINGS_RECORDS: [&str; 6] = [
    "catalogs",
    "overrides",
    "patchedDependencies",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "ignoredOptionalDependencies",
];

/// Keys whose collection renders on one line.
const SINGLE_LINE_KEYS: [&str; 4] = ["cpu", "engines", "os", "libc"];

/// Sorts the keys of an entry of a section.
type Sort = fn(Value) -> Value;

/// `document` with its keys in the order pnpm writes them.
pub(super) fn sorted(document: Value) -> Value {
    let Value::Object(mut root) = document else {
        return document;
    };
    if let Some(Value::Object(settings)) = root.remove("settings") {
        root.insert("settings".into(), Value::Object(sort_settings(settings)));
    }
    if let Some(Value::Object(time)) = root.remove("time") {
        root.insert("time".into(), Value::Object(sort_direct(time)));
    }
    let sorts: [(&str, Sort); 2] = [("packages", sort_package), ("projects", sort_importer)];
    for (section, sort) in sorts {
        if let Some(Value::Object(entries)) = root.remove(section) {
            let entries = sort_direct(entries)
                .into_iter()
                .map(|(key, value)| (key, sort(value)))
                .collect();
            root.insert(section.into(), Value::Object(entries));
        }
    }
    Value::Object(sort_map(
        root,
        &|left, right| priority_cmp(&ROOT_KEYS, left, right),
        false,
    ))
}

fn sort_settings(settings: Map<String, Value>) -> Map<String, Value> {
    let mut sorted = sort_direct(settings);
    let records: Vec<(&str, Value)> = SETTINGS_RECORDS
        .iter()
        .filter_map(|key| Some((*key, sorted.shift_remove(*key)?)))
        .collect();
    for (key, value) in records {
        let value = match (key, value) {
            ("catalogs", Value::Object(catalogs)) => Value::Object(
                sort_direct(catalogs)
                    .into_iter()
                    .map(|(name, entries)| (name, sort_deep(entries)))
                    .collect(),
            ),
            ("patchedDependencies", Value::Object(patches)) => Value::Object(sort_direct(patches)),
            (_, value) => value,
        };
        sorted.insert(key.into(), value);
    }
    sorted
}

fn sort_package(entry: Value) -> Value {
    let Value::Object(map) = entry else {
        return entry;
    };
    let sorted = sort_map(
        map,
        &|left, right| priority_cmp(&PACKAGE_KEYS, left, right),
        false,
    )
    .into_iter()
    .map(|(key, value)| {
        let value = match (key.as_str(), value) {
            ("snapshots", Value::Array(items)) => {
                Value::Array(items.into_iter().map(sort_package).collect())
            }
            (
                "peerDependencies" | "dependencies" | "optionalDependencies",
                Value::Object(entries),
            ) => Value::Object(sort_direct(entries)),
            (_, value) => sort_deep(value),
        };
        (key, value)
    })
    .collect();
    Value::Object(sorted)
}

fn sort_importer(entry: Value) -> Value {
    let Value::Object(map) = entry else {
        return entry;
    };
    let sorted = sort_map(
        map,
        &|left, right| priority_cmp(&["name"], left, right),
        false,
    )
    .into_iter()
    .map(|(key, value)| {
        let value = match (key.as_str(), value) {
            (
                "dependencies" | "devDependencies" | "optionalDependencies",
                Value::Object(dependencies),
            ) => Value::Object(sort_direct(dependencies)),
            (_, value) => sort_deep(value),
        };
        (key, value)
    })
    .collect();
    Value::Object(sorted)
}

/// Prioritized keys first, in priority order, then the rest by code unit.
fn priority_cmp(priority: &[&str], left: &str, right: &str) -> Ordering {
    let rank = |key: &str| priority.iter().position(|entry| *entry == key);
    match (rank(left), rank(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

fn sort_direct(map: Map<String, Value>) -> Map<String, Value> {
    sort_map(map, &str::cmp, false)
}

fn sort_deep(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(sort_map(map, &str::cmp, true)),
        Value::Array(items) => Value::Array(items.into_iter().map(sort_deep).collect()),
        other => other,
    }
}

fn sort_map(
    map: Map<String, Value>,
    compare: &dyn Fn(&str, &str) -> Ordering,
    deep: bool,
) -> Map<String, Value> {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.sort_by(|(left, _), (right, _)| compare(left, right));
    entries
        .into_iter()
        .map(|(key, value)| (key, if deep { sort_deep(value) } else { value }))
        .collect()
}

/// Whether a collection spans lines or sits on one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Lines,
    OneLine,
}

/// `document`, sorted, as the text of the file.
pub(super) fn to_string(document: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, document, 0, Layout::Lines, None);
    out.push('\n');
    out
}

/// `key` is the map key the value is bound to, which decides the one-line
/// collections and the blank lines.
fn write_value(out: &mut String, value: &Value, level: usize, layout: Layout, key: Option<&str>) {
    match value {
        Value::Object(map) => write_map(out, map, level, layout, key),
        Value::Array(items) => write_list(out, items, level, layout, key),
        Value::String(text) => push_string(out, text),
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn write_map(
    out: &mut String,
    map: &Map<String, Value>,
    level: usize,
    layout: Layout,
    key: Option<&str>,
) {
    if map.is_empty() {
        out.push_str("{}");
        return;
    }
    if layout == Layout::OneLine || level == 3 && is_single_line_map(key, map) {
        out.push('{');
        for (index, (entry_key, entry)) in map.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            push_string(out, entry_key);
            out.push_str(": ");
            write_value(out, entry, level, Layout::OneLine, None);
        }
        out.push('}');
        return;
    }
    // The top-level map and the entries of `projects` and `packages` are
    // separated by a blank line.
    let blank_lines = level == 0 || level == 1 && matches!(key, Some("projects" | "packages"));
    out.push('{');
    for (index, (entry_key, entry)) in map.iter().enumerate() {
        if index > 0 && blank_lines {
            out.push('\n');
        }
        next_line(out, level + 1);
        push_string(out, entry_key);
        out.push_str(": ");
        write_value(out, entry, level + 1, Layout::Lines, Some(entry_key));
        out.push(',');
    }
    next_line(out, level);
    out.push('}');
}

/// `resolution` sits on one line, except the nested `variations` and
/// `binary` shapes.
fn is_single_line_map(key: Option<&str>, map: &Map<String, Value>) -> bool {
    match key {
        Some(key) if SINGLE_LINE_KEYS.contains(&key) => true,
        Some("resolution") => !matches!(
            map.get("type").and_then(Value::as_str),
            Some("variations" | "binary")
        ),
        _ => false,
    }
}

fn write_list(out: &mut String, items: &[Value], level: usize, layout: Layout, key: Option<&str>) {
    let one_line = items.is_empty()
        || layout == Layout::OneLine
        || level == 3 && key.is_some_and(|key| SINGLE_LINE_KEYS.contains(&key));
    out.push('[');
    for (index, item) in items.iter().enumerate() {
        if one_line {
            if index > 0 {
                out.push_str(", ");
            }
            write_value(out, item, level, Layout::OneLine, None);
        } else {
            next_line(out, level + 1);
            write_value(out, item, level + 1, Layout::Lines, None);
            out.push(',');
        }
    }
    if !one_line {
        next_line(out, level);
    }
    out.push(']');
}

fn next_line(out: &mut String, level: usize) {
    out.push('\n');
    out.extend(std::iter::repeat_n('\t', level));
}

/// `text` as a JSON string.
fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            control if control < ' ' => {
                // Writing to a String cannot fail.
                let _ = write!(out, r"\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}
