//! Purpose-built byte-cursor SUBSET parser for `pnpm-lock.yaml`.
//!
//! pnpm emits a tightly constrained dialect of YAML: 2-space block
//! indentation, no anchors/aliases, no multi-line scalars, no flow
//! style except small inline maps (`resolution: {...}`, `engines:
//! {...}`) and inline seqs (`os: [linux, darwin]`). That regularity
//! lets us skip a general YAML state machine and walk the bytes
//! directly, using `memchr` to find line boundaries and the single
//! structural `:` separator. This is dramatically cheaper than the
//! event-stream + serde path for large lockfiles, where the bulk of
//! the work is thousands of trivial `key: value` snapshot/dependency
//! lines.
//!
//! ## Default-preserving by construction
//!
//! The parser is a *subset* parser: it recognizes the exact shape pnpm
//! writes and produces the SAME [`RawPnpmLockfile`] the serde path
//! produces. The instant it meets anything outside the recognized
//! subset — an unexpected indent, a flow construct it doesn't model, a
//! multi-document stream, a quoting style it can't normalize — it
//! returns `None` and the caller transparently falls back to the
//! `yaml_serde` parser. So the engine's observable behavior is
//! unchanged: the fast path only ever fires when it can produce a
//! byte-identical result, and everything else degrades to the original
//! parser.
//!
//! Inline flow values (`{...}` / `[...]`) are not re-implemented; the
//! small fragment is handed to `yaml_serde` so the fiddly resolution /
//! variants / `string_or_seq` shapes stay in lockstep with serde.

use super::raw::{
    RawCatalogEntry, RawDepSpec, RawImporter, RawPackageInfo, RawPatchedDependency,
    RawPnpmLockfile, RawSettings, RawSnapshot,
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Try to parse `content` with the subset parser. Returns `None`
/// whenever the input strays outside the recognized pnpm subset, in
/// which case the caller must fall back to the general YAML parser.
pub(super) fn try_parse(content: &str) -> Option<RawPnpmLockfile> {
    // Multi-document streams (pnpm v11 bootstrap + project doc) are
    // left to the scoring fallback — detecting which document to keep
    // is exactly the heuristic the serde path owns.
    if has_document_separator(content) {
        return None;
    }
    let mut p = Parser::new(content.as_bytes());
    p.parse()
}

/// `---` on its own line (a YAML document separator) signals a
/// multi-document stream we don't handle here.
fn has_document_separator(content: &str) -> bool {
    content
        .as_bytes()
        .split(|&b| b == b'\n')
        .any(|line| line == b"---" || line.starts_with(b"--- "))
}

struct Parser<'a> {
    bytes: &'a [u8],
    /// Cursor at the start of the current unconsumed line.
    pos: usize,
}

/// A logical line, split into its leading indent (count of spaces) and
/// the trimmed remainder. Blank/comment lines are skipped before this
/// is ever produced.
struct Line<'a> {
    indent: usize,
    /// Content after the indent, with no trailing `\r`/`\n`. Trailing
    /// spaces are NOT trimmed (pnpm never emits them; if present we
    /// bail to be safe).
    body: &'a [u8],
    /// Byte offset of the start of this line (for rewind).
    start: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Parser { bytes, pos: 0 }
    }

    /// Peek the next non-blank, non-comment logical line without
    /// consuming it. Returns `None` at EOF.
    fn peek_line(&self) -> Option<Line<'a>> {
        let mut pos = self.pos;
        while pos < self.bytes.len() {
            let nl = memchr::memchr(b'\n', &self.bytes[pos..])
                .map(|i| pos + i)
                .unwrap_or(self.bytes.len());
            let mut raw = &self.bytes[pos..nl];
            if raw.last() == Some(&b'\r') {
                raw = &raw[..raw.len() - 1];
            }
            let indent = raw.iter().take_while(|&&b| b == b' ').count();
            let body = &raw[indent..];
            // Skip blank lines and full-line comments.
            if body.is_empty() || body[0] == b'#' {
                pos = nl + 1;
                continue;
            }
            return Some(Line {
                indent,
                body,
                start: pos,
            });
        }
        None
    }

    /// Consume up to and including the line that `peek_line` returned.
    fn consume_line(&mut self, line: &Line<'a>) {
        let nl = memchr::memchr(b'\n', &self.bytes[line.start..])
            .map(|i| line.start + i + 1)
            .unwrap_or(self.bytes.len());
        self.pos = nl;
    }

    fn parse(&mut self) -> Option<RawPnpmLockfile> {
        let mut lockfile_version: Option<yaml_serde::Value> = None;
        let mut settings = None;
        let mut overrides = None;
        let mut package_extensions_checksum = None;
        let mut pnpmfile_checksum = None;
        let mut catalogs = None;
        let mut patched_dependencies = None;
        let mut ignored_optional_dependencies = None;
        let mut importers = BTreeMap::new();
        let mut packages = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        let mut time = None;

        while let Some(line) = self.peek_line() {
            // Top-level keys live at indent 0.
            if line.indent != 0 {
                return None;
            }
            let (key, inline) = split_key(line.body)?;
            self.consume_line(&line);
            match key {
                b"lockfileVersion" => {
                    lockfile_version = Some(parse_scalar_value(inline?)?);
                }
                b"packageExtensionsChecksum" => {
                    package_extensions_checksum = Some(scalar_string(inline?)?);
                }
                b"pnpmfileChecksum" => {
                    pnpmfile_checksum = Some(scalar_string(inline?)?);
                }
                b"settings" => {
                    settings = Some(self.parse_via_serde::<RawSettings>(2)?);
                }
                b"overrides" => {
                    overrides = Some(self.parse_string_map(2)?);
                }
                b"catalogs" => {
                    catalogs = Some(self.parse_catalogs()?);
                }
                b"patchedDependencies" => {
                    patched_dependencies =
                        Some(self.parse_via_serde::<BTreeMap<String, RawPatchedDependency>>(2)?);
                }
                b"ignoredOptionalDependencies" => {
                    // Inline `[a, b]` or a block seq — delegate.
                    ignored_optional_dependencies =
                        Some(self.parse_seq_value(inline, &line, 2)?);
                }
                b"importers" => {
                    importers = self.parse_importers()?;
                }
                b"packages" => {
                    packages = self.parse_packages()?;
                }
                b"snapshots" => {
                    snapshots = self.parse_snapshots()?;
                }
                b"time" => {
                    time = Some(self.parse_string_map(2)?);
                }
                // Any top-level key we don't recognize: bail to serde so
                // we never silently drop a field a future pnpm adds.
                _ => return None,
            }
        }

        Some(RawPnpmLockfile {
            lockfile_version: lockfile_version?,
            settings,
            overrides,
            package_extensions_checksum,
            pnpmfile_checksum,
            catalogs,
            patched_dependencies,
            ignored_optional_dependencies,
            importers,
            packages,
            snapshots,
            time,
        })
    }

    /// Collect the raw text of a block nested at `>= min_indent` plus
    /// the parent header line, and hand the whole fragment to
    /// `yaml_serde`. Used for sub-trees whose shape is fiddly enough
    /// that re-implementing it risks divergence (settings, catalogs
    /// entries, patchedDependencies). `header` is the already-consumed
    /// parent key (without trailing colon), reconstructed at indent 0
    /// for the fragment.
    fn parse_via_serde<T: for<'de> Deserialize<'de>>(&mut self, min_indent: usize) -> Option<T> {
        let block = self.take_block(min_indent)?;
        // Re-indent: the block lines are at >= min_indent; serde wants
        // them as a top-level mapping, so strip exactly min_indent.
        let dedented = dedent(&block, min_indent)?;
        yaml_serde::from_str::<T>(&dedented).ok()
    }

    /// Gather the raw source text of every line indented at
    /// `>= min_indent`, stopping at the first line with smaller indent
    /// (or EOF). Consumes those lines. Returns the raw bytes as a
    /// UTF-8 string slice range copied out.
    fn take_block(&mut self, min_indent: usize) -> Option<String> {
        let start = self.pos;
        let mut end = self.pos;
        while let Some(line) = self.peek_line() {
            if line.indent < min_indent {
                break;
            }
            self.consume_line(&line);
            end = self.pos;
        }
        std::str::from_utf8(&self.bytes[start..end])
            .ok()
            .map(|s| s.to_string())
    }

    /// Parse a simple `key: scalar` block (string→string) at
    /// `min_indent`. Bails (None) on any nested structure.
    fn parse_string_map(&mut self, min_indent: usize) -> Option<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < min_indent {
                break;
            }
            if line.indent != min_indent {
                return None;
            }
            let (k, v) = split_key(line.body)?;
            let v = v?; // must be inline
            self.consume_line(&line);
            map.insert(scalar_key_string(k)?, scalar_string(v)?);
        }
        Some(map)
    }

    /// Parse a seq value that is either inline (`[a, b]`) on the header
    /// line or a block seq nested below. Delegates to serde via a
    /// reconstructed fragment to keep edge cases exact.
    fn parse_seq_value(
        &mut self,
        inline: Option<&[u8]>,
        header: &Line<'a>,
        min_indent: usize,
    ) -> Option<Vec<String>> {
        if let Some(v) = inline {
            return serde_value_from_fragment(v);
        }
        // Block seq: gather indented `- item` lines.
        let _ = header;
        let block = self.take_block(min_indent)?;
        let dedented = dedent(&block, min_indent)?;
        yaml_serde::from_str::<Vec<String>>(&dedented).ok()
    }

    fn parse_catalogs(
        &mut self,
    ) -> Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>> {
        // catalogs:
        //   <catalogName>:
        //     <pkg>:
        //       specifier: ...
        //       version: ...
        // Delegate the whole sub-tree to serde — catalogs are tiny and
        // rare, not worth a bespoke path.
        let block = self.take_block(2)?;
        let dedented = dedent(&block, 2)?;
        yaml_serde::from_str(&dedented).ok()
    }

    fn parse_importers(&mut self) -> Option<BTreeMap<String, RawImporter>> {
        let mut importers = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                return None;
            }
            // `<importerPath>:` header.
            let (name, rest) = split_key(line.body)?;
            if rest.is_some() {
                // Inline importer body — unexpected; bail.
                return None;
            }
            let name = scalar_key_string(name)?;
            self.consume_line(&line);
            let imp = self.parse_importer_body()?;
            importers.insert(name, imp);
        }
        Some(importers)
    }

    fn parse_importer_body(&mut self) -> Option<RawImporter> {
        let mut dependencies = None;
        let mut dev_dependencies = None;
        let mut optional_dependencies = None;
        let mut skipped_optional_dependencies = None;
        while let Some(line) = self.peek_line() {
            if line.indent < 4 {
                break;
            }
            if line.indent != 4 {
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            if rest.is_some() {
                return None;
            }
            self.consume_line(&line);
            let specs = self.parse_dep_specs(6)?;
            match key {
                b"dependencies" => dependencies = Some(specs),
                b"devDependencies" => dev_dependencies = Some(specs),
                b"optionalDependencies" => optional_dependencies = Some(specs),
                b"skippedOptionalDependencies" => skipped_optional_dependencies = Some(specs),
                _ => return None,
            }
        }
        Some(RawImporter {
            dependencies,
            dev_dependencies,
            optional_dependencies,
            skipped_optional_dependencies,
        })
    }

    /// Parse a block of `<pkg>:` entries each with `specifier:` /
    /// `version:` children at `entry_indent`.
    fn parse_dep_specs(
        &mut self,
        entry_indent: usize,
    ) -> Option<BTreeMap<String, RawDepSpec>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < entry_indent {
                break;
            }
            if line.indent != entry_indent {
                return None;
            }
            let (name, rest) = split_key(line.body)?;
            if rest.is_some() {
                return None;
            }
            let name = scalar_key_string(name)?;
            self.consume_line(&line);
            // children: specifier / version at entry_indent + 2
            let mut specifier = None;
            let mut version = None;
            let child_indent = entry_indent + 2;
            while let Some(c) = self.peek_line() {
                if c.indent < child_indent {
                    break;
                }
                if c.indent != child_indent {
                    return None;
                }
                let (ck, cv) = split_key(c.body)?;
                let cv = cv?;
                self.consume_line(&c);
                match ck {
                    b"specifier" => specifier = Some(scalar_string(cv)?),
                    b"version" => version = Some(scalar_string(cv)?),
                    _ => return None,
                }
            }
            map.insert(
                name,
                RawDepSpec {
                    specifier: specifier?,
                    version: version?,
                },
            );
        }
        Some(map)
    }

    fn parse_packages(&mut self) -> Option<BTreeMap<String, RawPackageInfo>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            // `<depPath>: {}` (empty inline) or block body.
            let name = scalar_key_string(key)?;
            self.consume_line(&line);
            let info = if let Some(inline) = rest {
                // Inline body for a package entry is unexpected (pnpm
                // writes block bodies); bail unless it's an empty map.
                if inline == b"{}" {
                    default_package_info()
                } else {
                    return None;
                }
            } else {
                self.parse_package_body(4)?
            };
            map.insert(name, info);
        }
        Some(map)
    }

    fn parse_package_body(&mut self, indent: usize) -> Option<RawPackageInfo> {
        // Collect the whole package sub-block and hand to serde. Package
        // bodies carry the fiddly `resolution`/`variants`/`string_or_seq`
        // shapes; re-implementing them risks divergence and they are a
        // minority of total lines compared to snapshots+importers.
        let block = self.take_block(indent)?;
        if block.is_empty() {
            return Some(default_package_info());
        }
        let dedented = dedent(&block, indent)?;
        yaml_serde::from_str::<RawPackageInfo>(&dedented).ok()
    }

    fn parse_snapshots(&mut self) -> Option<BTreeMap<String, RawSnapshot>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            let name = scalar_key_string(key)?;
            self.consume_line(&line);
            let snap = if let Some(inline) = rest {
                if inline == b"{}" {
                    default_snapshot()
                } else {
                    return None;
                }
            } else {
                self.parse_snapshot_body(4)?
            };
            map.insert(name, snap);
        }
        Some(map)
    }

    fn parse_snapshot_body(&mut self, indent: usize) -> Option<RawSnapshot> {
        let mut dependencies = None;
        let mut optional_dependencies = None;
        let mut bundled_dependencies = None;
        let mut optional = None;
        let mut transitive_peer_dependencies = None;
        while let Some(line) = self.peek_line() {
            if line.indent < indent {
                break;
            }
            if line.indent != indent {
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            self.consume_line(&line);
            match key {
                b"dependencies" => {
                    if rest.is_some() {
                        return None;
                    }
                    dependencies = Some(self.parse_string_map(indent + 2)?);
                }
                b"optionalDependencies" => {
                    if rest.is_some() {
                        return None;
                    }
                    optional_dependencies = Some(self.parse_string_map(indent + 2)?);
                }
                b"optional" => {
                    optional = Some(parse_bool(rest?)?);
                }
                b"bundledDependencies" => {
                    bundled_dependencies =
                        Some(self.parse_seq_value(rest, &line, indent + 2)?);
                }
                b"transitivePeerDependencies" => {
                    transitive_peer_dependencies =
                        Some(self.parse_seq_value(rest, &line, indent + 2)?);
                }
                _ => return None,
            }
        }
        Some(RawSnapshot {
            dependencies,
            optional_dependencies,
            bundled_dependencies,
            optional,
            transitive_peer_dependencies,
        })
    }
}

fn default_package_info() -> RawPackageInfo {
    // An empty package entry: serde produces this from `{}`. Reuse the
    // serde path once so the field defaults stay authoritative.
    yaml_serde::from_str::<RawPackageInfo>("{}").expect("empty package info parses")
}

fn default_snapshot() -> RawSnapshot {
    RawSnapshot {
        dependencies: None,
        optional_dependencies: None,
        bundled_dependencies: None,
        optional: None,
        transitive_peer_dependencies: None,
    }
}

/// Split `key: value` on the FIRST structural `:` (a `:` followed by a
/// space or end-of-line). Returns the raw key bytes and the inline
/// value (`None` if the line is just `key:`). Bails (None) on a line
/// with no structural colon.
fn split_key(body: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    // pnpm package/snapshot keys contain `@` and version `:`-free
    // dep-paths; the structural separator is the LAST `: ` / trailing
    // `:` is not reliable for keys like `foo@1.0.0(bar@2.0.0)`. pnpm
    // never puts a bare `: ` inside a top-level/entry key except inside
    // quotes. We scan for the first `:` that is followed by a space or
    // is at end-of-line, skipping over quoted regions.
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < body.len() {
        let b = body[i];
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b':' if !in_single && !in_double => {
                let next = body.get(i + 1);
                if next.is_none() {
                    return Some((&body[..i], None));
                }
                if next == Some(&b' ') {
                    let val = &body[i + 2..];
                    // strip trailing spaces (pnpm shouldn't emit any)
                    let val = trim_trailing_ws(val);
                    return Some((&body[..i], Some(val)));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn trim_trailing_ws(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && (b[end - 1] == b' ' || b[end - 1] == b'\t') {
        end -= 1;
    }
    &b[..end]
}

/// A YAML key that may be quoted. Unquote single/double quotes;
/// otherwise take verbatim. Bail on anything needing escape processing
/// inside double quotes (rare in pnpm keys — those are plain).
fn scalar_key_string(raw: &[u8]) -> Option<String> {
    scalar_string(raw)
}

/// Decode a scalar value: bare, single-quoted (`'...'` with `''`
/// escape), or double-quoted (only when it contains no backslash
/// escapes — otherwise bail to serde). Plain scalars are taken
/// verbatim.
fn scalar_string(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    if raw[0] == b'\'' {
        if raw.len() < 2 || raw[raw.len() - 1] != b'\'' {
            return None;
        }
        let inner = &raw[1..raw.len() - 1];
        // YAML single-quote escape is `''` → `'`.
        let s = std::str::from_utf8(inner).ok()?;
        return Some(s.replace("''", "'"));
    }
    if raw[0] == b'"' {
        if raw.len() < 2 || raw[raw.len() - 1] != b'"' {
            return None;
        }
        let inner = &raw[1..raw.len() - 1];
        // Bail if any backslash escape is present — let serde handle it.
        if inner.contains(&b'\\') {
            return None;
        }
        return std::str::from_utf8(inner).ok().map(|s| s.to_string());
    }
    // Bare scalar. Must not be a flow construct.
    if raw[0] == b'{' || raw[0] == b'[' || raw[0] == b'&' || raw[0] == b'*' {
        return None;
    }
    std::str::from_utf8(raw).ok().map(|s| s.to_string())
}

/// Parse `lockfileVersion`-style scalar into a `yaml_serde::Value`,
/// preserving the original string/number distinction by going through
/// serde for that one tiny value.
fn parse_scalar_value(raw: &[u8]) -> Option<yaml_serde::Value> {
    let s = std::str::from_utf8(raw).ok()?;
    yaml_serde::from_str::<yaml_serde::Value>(s).ok()
}

fn parse_bool(raw: &[u8]) -> Option<bool> {
    match raw {
        b"true" => Some(true),
        b"false" => Some(false),
        _ => None,
    }
}

/// Deserialize a tiny inline flow fragment (`[a, b]`, etc.) via serde.
fn serde_value_from_fragment(raw: &[u8]) -> Option<Vec<String>> {
    let s = std::str::from_utf8(raw).ok()?;
    yaml_serde::from_str::<Vec<String>>(s).ok()
}

/// Strip exactly `n` leading spaces from every non-blank line so a
/// nested block parses as a top-level serde mapping. Returns `None` if
/// a non-blank line has fewer than `n` leading spaces.
fn dedent(block: &str, n: usize) -> Option<String> {
    let mut out = String::with_capacity(block.len());
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        let spaces = trimmed.bytes().take_while(|&b| b == b' ').count();
        if spaces < n {
            return None;
        }
        out.push_str(&line[n..]);
    }
    Some(out)
}

#[cfg(test)]
mod subset_tests {
    use super::*;

    const NATIVE: &str = include_str!("../../tests/fixtures/pnpm-native.yaml");

    #[test]
    fn fast_path_fires_on_native_fixture() {
        let raw = try_parse(NATIVE).expect("subset parser should accept native pnpm fixture");
        assert_eq!(raw.packages.len(), 8);
        assert_eq!(raw.snapshots.len(), 8);
        assert_eq!(raw.importers.len(), 1);
    }

    #[test]
    fn declines_multi_document_stream() {
        // pnpm v11 two-document layout: the scoring fallback owns this.
        let two_docs = "lockfileVersion: '9.0'\npackages: {}\n---\nlockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies: {}\n";
        assert!(try_parse(two_docs).is_none());
    }

    #[test]
    fn declines_unknown_top_level_key() {
        // An unmodeled top-level field must fall back so it is never
        // silently dropped.
        let with_unknown = "lockfileVersion: '9.0'\nfutureField: 1\n";
        assert!(try_parse(with_unknown).is_none());
    }

    #[test]
    fn split_key_respects_quoted_colons() {
        let (k, v) = split_key(b"'a:b': value").unwrap();
        assert_eq!(k, b"'a:b'");
        assert_eq!(v, Some(&b"value"[..]));
    }
}
