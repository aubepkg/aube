//! Walk the module graph reachable from a package's PUBLISHED entry points,
//! collecting the bare-specifier occurrences seen along the way.
//!
//! Restricting to reachable files is what keeps a `devDependencies`-only import
//! in a test/example file (never referenced by `exports`/`main`/`bin`) from being
//! mistaken for a phantom: those files are simply never reached. Relative edges
//! are followed (with Node-style extension/index resolution); bare edges become
//! candidate dependencies.
//!
//! Ported from nub's `nub-phantom-scan` (jdx/nub / nubjs/nub, MIT), trimmed to
//! the filesystem-backed walk only — nub's CAS-index variant (extract-time
//! scanning before a navigable tree exists) is specific to nub's own
//! package-linking pipeline and does not apply here.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::{Entry, EntryKind};
use aube_phantom_core::extract::{Occurrence, extract};
use aube_phantom_core::specifier::{self, SpecKind};

/// Provenance bit: reached from the main surface (`main`/`bin`/`exports."."`).
const FROM_MAIN: u8 = 0b001;
/// Provenance bit: reached from a non-`.` `exports` subpath (the adapter surface).
const FROM_SUBPATH: u8 = 0b010;
/// Provenance bit: reached from the `.d.ts` TYPE surface (`types`/`typings`/an
/// `exports` `types` condition/`index.d.ts`).
const FROM_TYPES: u8 = 0b100;

/// A bare-specifier reference collected from a reachable file.
#[derive(Debug, Clone)]
pub struct Reference {
    /// Package name the specifier resolves to (`@scope/name` or `name`).
    pub(crate) package: String,
    /// The raw specifier (kept for the report — shows the exact subpath).
    pub(crate) raw: String,
    /// Guarded (try/catch or a conditional branch) at every occurrence collapses
    /// to soft; a single unguarded occurrence makes the package hard.
    pub(crate) soft: bool,
    /// Reachable from the main entry surface.
    pub(crate) from_main: bool,
    /// Reachable from a non-`.` `exports` subpath — the "consumer opts into
    /// `<pkg>/<subpath>`" adapter surface. A hard phantom reachable ONLY from a
    /// subpath (not main) is the subpath-adapter class.
    pub(crate) from_subpath: bool,
    /// Reachable from the `.d.ts` TYPE surface.
    pub(crate) from_types: bool,
}

/// Result of the reachable-module walk.
#[derive(Debug, Default)]
pub struct Walk {
    pub references: Vec<Reference>,
    pub files_analyzed: usize,
    /// Relative imports that could not be resolved to a file on disk (a tell of
    /// an incomplete install or an exotic resolver condition; reported, not fatal).
    pub unresolved_relative: usize,
}

/// Cap the walk so a pathological package (thousands of files) can't stall a
/// scan. Real published entry graphs are far smaller.
const MAX_FILES: usize = 6000;

/// Walk from `entry_points`, following relative edges and collecting bare
/// references. Each reachable file accumulates a provenance mask (which entry
/// surface(s) reach it); a bare reference inherits its file's final mask, so the
/// report can separate subpath-adapter phantoms from main-graph ones.
///
/// Two phases keep provenance correct across diamonds: (1) BFS parses each file
/// once (cached) and propagates provenance bits to fixpoint — a file re-reached
/// with new bits is re-queued for propagation only, never re-parsed; (2) build
/// references from the cache using each file's FINAL mask.
pub fn walk(root: &Path, entry_points: &[Entry]) -> Walk {
    let mut result = Walk::default();
    let mut parsed: BTreeMap<PathBuf, Vec<Occurrence>> = BTreeMap::new();
    let mut flags: BTreeMap<PathBuf, u8> = BTreeMap::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();

    for ep in entry_points {
        let prefer_dts = ep.kind == EntryKind::Types;
        if let Some(resolved) = fs_resolve(root, root, &ep.path, prefer_dts, 0) {
            let bit = match ep.kind {
                EntryKind::Main => FROM_MAIN,
                EntryKind::Subpath => FROM_SUBPATH,
                EntryKind::Types => FROM_TYPES,
            };
            if add_flags(&mut flags, &resolved, bit) {
                queue.push_back(resolved);
            }
        }
    }

    while let Some(file) = queue.pop_front() {
        let fflags = *flags.get(&file).unwrap_or(&0);
        // Parse once; a re-queue for provenance propagation reuses the cache.
        if !parsed.contains_key(&file) {
            if parsed.len() >= MAX_FILES {
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let rel = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            parsed.insert(file.clone(), extract(&rel, &text));
        }
        // Collect relative edges first (immutable borrow of the cache) then
        // propagate — avoids holding a borrow across the flags mutation.
        let mut targets = Vec::new();
        for occ in &parsed[&file] {
            if let SpecKind::Relative = specifier::classify(&occ.spec) {
                let from_dir = file.parent().unwrap_or(root);
                let prefer_dts = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(crate::manifest::is_dts_like);
                match fs_resolve(root, from_dir, &occ.spec, prefer_dts, 0) {
                    Some(t) => targets.push(t),
                    None => result.unresolved_relative += 1,
                }
            }
        }
        // `ImportsHash` (self) and `NonPackage` (URL/virtual/internal) are not
        // dependency edges; only `Bare` references are collected in phase 2.
        for t in targets {
            if add_flags(&mut flags, &t, fflags) {
                queue.push_back(t);
            }
        }
    }

    result.files_analyzed = parsed.len();
    for (file, occs) in &parsed {
        let fflags = *flags.get(file).unwrap_or(&0);
        for occ in occs {
            if let SpecKind::Bare(package) = specifier::classify(&occ.spec) {
                result.references.push(Reference {
                    package,
                    raw: occ.spec.clone(),
                    soft: occ.soft,
                    from_main: fflags & FROM_MAIN != 0,
                    from_subpath: fflags & FROM_SUBPATH != 0,
                    from_types: fflags & FROM_TYPES != 0,
                });
            }
        }
    }
    result
}

/// OR `bit` into `key`'s provenance mask. Returns true if the mask GREW (new
/// file, or new bits) — the caller then (re)queues it so the new provenance
/// propagates to its edges. The 2-bit lattice bounds re-queues to ≤2 per file.
fn add_flags(flags: &mut BTreeMap<PathBuf, u8>, key: &PathBuf, bit: u8) -> bool {
    let entry = flags.entry(key.clone()).or_insert(0);
    let before = *entry;
    *entry |= bit;
    *entry != before
}

/// Bound on `main`-chasing recursion. A dir whose `package.json` `main` points
/// back at itself (`"."`/`""`/`"./"`) or a mutual `main` cycle across dirs would
/// otherwise recurse forever → a stack-overflow ABORT that kills the whole scan
/// (a process abort, not a catchable panic). Such manifests occur in the wild, so
/// the cap is a hard robustness requirement, not a nicety.
const MAX_RESOLVE_DEPTH: u32 = 16;

/// Node-style RUNTIME resolution extensions, in priority order — used when
/// resolving from a runtime (`.js`/`.ts`/SFC) source or a Main/Subpath entry.
const JS_EXTS: [&str; 8] = ["js", "cjs", "mjs", "jsx", "ts", "tsx", "mts", "cts"];

/// TypeScript DECLARATION extensions — used when resolving from a `.d.ts` source
/// or a `Types` entry. A type-surface edge is resolved to `.d.ts` ONLY and never
/// diverts to a `.js` sibling: the standard compiled layout ships `widgets.js`
/// beside `widgets.d.ts`, and resolving a type re-export to the `.js` would both
/// capture that `.js`'s RUNTIME imports as type references (over-flag) and skip
/// the real `widgets.d.ts` (missed type imports).
const DTS_EXTS: [&str; 3] = ["d.ts", "d.mts", "d.cts"];

/// Extension ladder + file-acceptance predicate keyed to the resolution surface.
/// The runtime surface admits JS/SFC files (byte-identical to the pre-type-surface
/// behavior); the type surface admits `.d.ts` only.
fn surface_exts(prefer_dts: bool) -> &'static [&'static str] {
    if prefer_dts { &DTS_EXTS } else { &JS_EXTS }
}

fn resolvable_name(name: &str, prefer_dts: bool) -> bool {
    if prefer_dts {
        crate::manifest::is_dts_like(name)
    } else {
        crate::manifest::is_js_like(name) || crate::manifest::is_sfc_like(name)
    }
}

/// On the type surface, TS resolves a `./widgets.js` re-export's TYPES at
/// `./widgets.d.ts` (the NodeNext convention: the specifier keeps the runtime
/// extension, the declaration sits beside it). Strip any runtime/declaration
/// extension so the `.d.ts` ladder re-appends the declaration form. `./widgets` →
/// `./widgets`; `./widgets.js` → `./widgets`; `./widgets.d.ts` → `./widgets`.
fn dts_stem(spec: &str) -> &str {
    for ext in [
        ".d.ts", ".d.mts", ".d.cts", ".js", ".cjs", ".mjs", ".jsx", ".ts", ".tsx", ".mts", ".cts",
    ] {
        if let Some(stem) = spec.strip_suffix(ext) {
            return stem;
        }
    }
    spec
}

fn fs_resolve(
    root: &Path,
    from_dir: &Path,
    spec: &str,
    prefer_dts: bool,
    depth: u32,
) -> Option<PathBuf> {
    if depth > MAX_RESOLVE_DEPTH {
        return None;
    }
    // On the type surface, `./widgets.js` re-exports resolve types at
    // `./widgets.d.ts`; strip the extension so the `.d.ts` ladder re-appends it.
    let spec = if prefer_dts { dts_stem(spec) } else { spec };
    let joined = from_dir.join(spec);
    // Keep the walk inside the package tree (a `../../` that climbs out is not
    // part of the published surface).
    let base = normalize(&joined);
    // `root` itself must go through the same lexical normalization as `base` —
    // a caller-supplied root carrying a `.`/`..` component (e.g.
    // `./node_modules/foo`) would otherwise never satisfy `starts_with` against
    // the normalized `base`, silently resolving nothing.
    if !base.starts_with(normalize(root)) {
        return None;
    }
    let exts = surface_exts(prefer_dts);

    // 1. Exact file (runtime surface only — the type surface always stems + appends).
    if !prefer_dts && is_resolvable_file(&base, prefer_dts) {
        return Some(base);
    }
    // 2. `base.<ext>`.
    for ext in exts {
        let cand = with_appended_ext(&base, ext);
        if is_resolvable_file(&cand, prefer_dts) {
            return Some(cand);
        }
    }
    // 3. `base/index.<ext>`.
    for ext in exts {
        let cand = base.join(format!("index.{ext}"));
        if is_resolvable_file(&cand, prefer_dts) {
            return Some(cand);
        }
    }
    // 4. `base/package.json` → its `types`/`main` (depth-bounded). The type surface
    // chases `types`/`typings`; the runtime surface chases `main`.
    let pkg = base.join("package.json");
    if let Ok(raw) = fs::read(&pkg) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw)
            && let Some(entry) = manifest_entry(&v, prefer_dts)
        {
            return fs_resolve(root, &base, &entry, prefer_dts, depth + 1);
        }
        // package.json with no entry → default `index.<ext>` in that dir.
        for ext in exts {
            let cand = base.join(format!("index.{ext}"));
            if is_resolvable_file(&cand, prefer_dts) {
                return Some(cand);
            }
        }
    }
    None
}

/// The relevant entry field of a nested `package.json` for the current surface:
/// `types`/`typings` for the type walk, `main` for runtime.
fn manifest_entry(v: &serde_json::Value, prefer_dts: bool) -> Option<String> {
    let fields: &[&str] = if prefer_dts {
        &["types", "typings"]
    } else {
        &["main"]
    };
    fields
        .iter()
        .find_map(|f| v.get(*f).and_then(|m| m.as_str()))
        .map(str::to_string)
}

/// Existence + surface-appropriate extension check (`.d.ts` on the type surface,
/// JS/SFC on the runtime surface).
fn is_resolvable_file(p: &Path, prefer_dts: bool) -> bool {
    if !p.is_file() {
        return false;
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    resolvable_name(name, prefer_dts)
}

/// Append an extension to a path's file name (`a/b` + `js` → `a/b.js`), rather
/// than replacing an existing one (`a/b.min` must become `a/b.min.js`).
fn with_appended_ext(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Lexically normalize `.`/`..` segments WITHOUT touching the filesystem (we do
/// not want symlink resolution). Used only to enforce the stay-under-root
/// invariant.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::walk;
    use crate::manifest::{Entry, EntryKind};
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aube-phantom-graph-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn main_entry(path: &str) -> Entry {
        Entry {
            path: path.to_string(),
            kind: EntryKind::Main,
        }
    }

    #[test]
    fn root_with_a_dot_component_still_resolves() {
        // A caller passing `root` with a lexical `.`/`..` component (e.g. via
        // `some_dir.join("./node_modules/foo")`) must still walk correctly —
        // `root` needs the same normalization `base` already gets, or the
        // `starts_with` containment check never matches and the walk silently
        // resolves nothing.
        let real_root = scratch("dot-root");
        fs::write(real_root.join("index.js"), "require('real-dep');").unwrap();
        let dotted_root = real_root.join(".");

        let w = walk(&dotted_root, &[main_entry("index.js")]);
        assert_eq!(w.files_analyzed, 1, "entry must resolve through a dotted root");
        assert!(
            w.references.iter().any(|r| r.package == "real-dep"),
            "reference must be collected: {:?}",
            w.references
        );
        let _ = fs::remove_dir_all(&real_root);
    }

    #[test]
    fn follows_relative_edges_collects_bare_ignores_unreached() {
        let root = scratch("reach");
        // entry → ./util (reached) imports "declared-dep"; orphan test file
        // imports "dev-only" but is never referenced.
        fs::write(
            root.join("index.js"),
            "require('./util'); require('real-dep');",
        )
        .unwrap();
        fs::write(root.join("util.js"), "import x from 'util-dep';").unwrap();
        fs::write(root.join("test.js"), "require('dev-only');").unwrap();

        let w = walk(&root, &[main_entry("index.js")]);
        let pkgs: Vec<_> = w.references.iter().map(|r| r.package.as_str()).collect();
        assert!(pkgs.contains(&"real-dep"));
        assert!(pkgs.contains(&"util-dep"));
        assert!(
            !pkgs.contains(&"dev-only"),
            "unreached test file must not contribute"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn self_cyclic_main_does_not_stack_overflow() {
        // A dir whose package.json `main` points back at itself would recurse
        // forever without the depth cap → process abort, killing the whole scan.
        let root = scratch("cycle");
        fs::write(root.join("index.js"), "require('./lib');").unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/package.json"), r#"{"main":"."}"#).unwrap();
        // Must return (not abort); the cyclic dir simply resolves to nothing.
        let w = walk(&root, &[main_entry("index.js")]);
        assert_eq!(w.files_analyzed, 1); // only index.js; lib/ resolves to no file
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn subpath_provenance_separates_adapter_from_main_graph() {
        let root = scratch("subpath");
        // main graph imports `main-dep`; the `./zod` adapter subpath imports
        // `backend-zod`. The zod import must carry from_subpath && !from_main.
        fs::write(root.join("index.js"), "require('main-dep');").unwrap();
        fs::write(root.join("zod.js"), "import 'backend-zod';").unwrap();

        let w = walk(
            &root,
            &[
                main_entry("index.js"),
                Entry {
                    path: "zod.js".to_string(),
                    kind: EntryKind::Subpath,
                },
            ],
        );
        let zod = w
            .references
            .iter()
            .find(|r| r.package == "backend-zod")
            .unwrap();
        assert!(zod.from_subpath && !zod.from_main, "adapter-only backend");
        let main = w
            .references
            .iter()
            .find(|r| r.package == "main-dep")
            .unwrap();
        assert!(main.from_main && !main.from_subpath, "main-graph dep");
        let _ = fs::remove_dir_all(&root);
    }
}
