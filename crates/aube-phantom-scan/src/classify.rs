//! Classify each referenced package against the manifest's declared surface.
//!
//! Aggregation rule: a package is HARD-needed if it is referenced by at least one
//! UNGUARDED occurrence; it is soft only if EVERY occurrence is guarded (in a
//! try/catch). The classification then answers the one question that matters —
//! is this reference covered by something a consumer install makes resolvable?
//!
//! Ported from nub's `nub-phantom-scan` (jdx/nub / nubjs/nub, MIT), unchanged.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::graph::Reference;
use crate::manifest::Manifest;
use aube_phantom_core::builtins::is_builtin;

/// The verdict for one referenced package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Undeclared and hard-required — a genuine phantom dependency.
    HardPhantom,
    /// Undeclared but only ever loaded under a try/catch — a soft/optional load,
    /// not a hard break.
    SoftPhantom,
    /// Undeclared and reachable ONLY from the `.d.ts` type surface — a
    /// type-position import that needs nothing at runtime, whether its types
    /// come from a declared `@types/<pkg>` twin or the package's own bundled
    /// types. NOT a phantom; excluded from compat targets. This is the class
    /// that flagged `estree` (pnpm#13981) and, when gated on a declared
    /// `@types/<pkg>` twin, `typescript` (pnpm#14128) — a type-only import is
    /// not a runtime dependency regardless of where its types come from.
    TypeOnly,
    /// Declared as an OPTIONAL peer (`peerDependenciesMeta.<x>.optional`). NOT a
    /// phantom — the pick-your-plugin pattern. Tracked so the report can show how
    /// much a naive scan over-counts.
    DeclaredOptionalPeer,
    /// Declared as a required peer.
    DeclaredPeer,
    /// Declared in `dependencies`/`optionalDependencies`, or bundled.
    Declared,
    /// A Node builtin.
    Builtin,
    /// A self reference (the package's own name / subpath).
    SelfRef,
}

/// One classified package reference.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub package: String,
    pub verdict: Verdict,
    /// True if every occurrence was guarded (try/catch or a conditional branch).
    soft: bool,
    /// Reachable from the package's main entry surface.
    pub(crate) from_main: bool,
    /// Reachable from a non-`.` `exports` subpath (the adapter surface).
    pub(crate) from_subpath: bool,
    /// Reachable from the `.d.ts` TYPE surface — a DECLARED PEER with this set is
    /// the nub#450 peer-type class (its `@types/<peer>` must be project-local).
    pub(crate) from_types: bool,
    /// Example raw specifiers (deduped) showing how it was referenced.
    pub specifiers: Vec<String>,
}

impl Finding {
    /// The subpath-adapter class the GVS-default bug hinges on: a HARD phantom
    /// reachable ONLY from a non-`.` `exports` subpath (not the main graph). This
    /// is the `<pkg>/<adapter>` that statically imports a consumer-installed
    /// backend it never declares (`@hookform/resolvers/zod` → `zod`).
    pub fn is_subpath_adapter(&self) -> bool {
        self.verdict == Verdict::HardPhantom && self.from_subpath && !self.from_main
    }
}

/// Classify all references against `manifest`. Returns one `Finding` per distinct
/// referenced package, sorted by package name.
pub fn classify(manifest: &Manifest, references: &[Reference]) -> Vec<Finding> {
    // Aggregate per package: soft-ness ANDs (hard wins), provenance ORs, collect
    // example specs.
    struct Agg {
        all_soft: bool,
        from_main: bool,
        from_subpath: bool,
        from_types: bool,
        specs: Vec<String>,
    }
    let mut by_pkg: BTreeMap<String, Agg> = BTreeMap::new();
    for r in references {
        let e = by_pkg.entry(r.package.clone()).or_insert(Agg {
            all_soft: true,
            from_main: false,
            from_subpath: false,
            from_types: false,
            specs: Vec::new(),
        });
        e.all_soft &= r.soft;
        e.from_main |= r.from_main;
        e.from_subpath |= r.from_subpath;
        e.from_types |= r.from_types;
        if !e.specs.contains(&r.raw) {
            e.specs.push(r.raw.clone());
        }
    }

    by_pkg
        .into_iter()
        .map(|(package, agg)| {
            let base = verdict_for(manifest, &package, agg.all_soft);
            // A reference reachable ONLY from the `.d.ts` type surface needs
            // nothing at runtime, whether its types come from a declared
            // `@types/<pkg>` twin (eslint/estree) or the package's own bundled
            // types (typescript, zod) — reclassify it TypeOnly unconditionally
            // so it never becomes a compat target. pnpm#14128: gating this on a
            // declared `@types/<pkg>` twin flagged `@typescript-eslint/types`'s
            // `import type { Program } from 'typescript'` as a HardPhantom,
            // because there is no separate `@types/typescript` package to find —
            // typescript ships its own types, so `types_satisfied` never held.
            let type_surface_only = agg.from_types && !agg.from_main && !agg.from_subpath;
            let verdict = if type_surface_only
                && matches!(base, Verdict::HardPhantom | Verdict::SoftPhantom)
            {
                Verdict::TypeOnly
            } else {
                base
            };
            Finding {
                package,
                verdict,
                soft: agg.all_soft,
                from_main: agg.from_main,
                from_subpath: agg.from_subpath,
                from_types: agg.from_types,
                specifiers: agg.specs,
            }
        })
        .collect()
}

fn verdict_for(manifest: &Manifest, package: &str, all_soft: bool) -> Verdict {
    if is_self(manifest, package) {
        return Verdict::SelfRef;
    }
    if is_builtin(package) {
        return Verdict::Builtin;
    }
    if manifest.deps.contains(package) || manifest.bundled.contains(package) {
        return Verdict::Declared;
    }
    if manifest.optional_peers.contains(package) {
        return Verdict::DeclaredOptionalPeer;
    }
    if manifest.required_peers.contains(package) {
        return Verdict::DeclaredPeer;
    }
    // Undeclared.
    if all_soft {
        Verdict::SoftPhantom
    } else {
        Verdict::HardPhantom
    }
}

/// A reference to the package's own name is a self import (resolvable via the
/// package's own `exports`), never a phantom.
fn is_self(manifest: &Manifest, package: &str) -> bool {
    package == manifest.name
}

#[cfg(test)]
mod tests {
    use super::{Verdict, classify};
    use crate::graph::Reference;
    use crate::manifest::Manifest;

    fn refs(items: &[(&str, &str, bool)]) -> Vec<Reference> {
        items
            .iter()
            .map(|(p, raw, soft)| Reference {
                package: (*p).to_string(),
                raw: (*raw).to_string(),
                soft: *soft,
                from_main: true,
                from_subpath: false,
                from_types: false,
            })
            .collect()
    }

    #[test]
    fn declared_optional_peer_is_not_a_phantom() {
        // @hookform/resolvers-style: zod is a DECLARED optional peer, referenced
        // by the /zod subpath. Must NOT be flagged phantom.
        let m = Manifest::parse(
            br#"{"name":"@hookform/resolvers","peerDependencies":{"zod":"*"},
                 "peerDependenciesMeta":{"zod":{"optional":true}}}"#,
        )
        .unwrap();
        let f = classify(&m, &refs(&[("zod", "zod", false)]));
        assert_eq!(f[0].verdict, Verdict::DeclaredOptionalPeer);
    }

    #[test]
    fn undeclared_hard_require_is_a_phantom_soft_is_not() {
        let m = Manifest::parse(br#"{"name":"pkg","dependencies":{"a":"1"}}"#).unwrap();
        let f = classify(
            &m,
            &refs(&[
                ("a", "a", false),         // declared
                ("ghost", "ghost", false), // hard phantom
                ("maybe", "maybe", true),  // soft phantom
                ("fs", "fs", false),       // builtin
            ]),
        );
        let v = |name: &str| f.iter().find(|x| x.package == name).unwrap().verdict;
        assert_eq!(v("a"), Verdict::Declared);
        assert_eq!(v("ghost"), Verdict::HardPhantom);
        assert_eq!(v("maybe"), Verdict::SoftPhantom);
        assert_eq!(v("fs"), Verdict::Builtin);
    }

    #[test]
    fn one_hard_occurrence_beats_a_soft_one() {
        // Same undeclared package referenced both guarded and unguarded → hard.
        let m = Manifest::parse(br#"{"name":"pkg"}"#).unwrap();
        let f = classify(&m, &refs(&[("x", "x", true), ("x", "x/sub", false)]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].verdict, Verdict::HardPhantom);
        assert!(!f[0].soft);
    }

    #[test]
    fn type_surface_only_is_type_only_not_a_phantom() {
        // eslint/`estree` (pnpm#13981): `estree` is referenced only from a `.d.ts`
        // type surface (from_types, not from_main/from_subpath), is undeclared,
        // but its `@types/estree` twin is declared. It must be TypeOnly, not a
        // HardPhantom, so it never becomes a compat target. A runtime import of an
        // undeclared package stays a hard phantom.
        let m =
            Manifest::parse(br#"{"name":"eslint","dependencies":{"@types/estree":"*"}}"#).unwrap();
        let estree = Reference {
            package: "estree".into(),
            raw: "estree".into(),
            soft: false,
            from_main: false,
            from_subpath: false,
            from_types: true,
        };
        let ghost = Reference {
            package: "ghost".into(),
            raw: "ghost".into(),
            soft: false,
            from_main: true,
            from_subpath: false,
            from_types: false,
        };
        let f = classify(&m, &[estree, ghost]);
        let estree_v = f.iter().find(|x| x.package == "estree").unwrap();
        let ghost_v = f.iter().find(|x| x.package == "ghost").unwrap();
        assert_eq!(estree_v.verdict, Verdict::TypeOnly);
        assert_eq!(ghost_v.verdict, Verdict::HardPhantom);
    }

    #[test]
    fn type_surface_only_needs_no_types_twin_at_all() {
        // pnpm#14128: `@typescript-eslint/types`'s only reference to `typescript`
        // is `import type { Program } from 'typescript'` in a `.d.ts` file.
        // Gating TypeOnly on a declared `@types/typescript` twin flagged this a
        // HardPhantom, because typescript ships its own types — there is no
        // separate `@types/typescript` package to find. A type-surface-only
        // reference must be TypeOnly regardless of where (or whether) a types
        // twin is declared, since it needs nothing at runtime either way.
        let m = Manifest::parse(br#"{"name":"@typescript-eslint/types"}"#).unwrap();
        let typescript = Reference {
            package: "typescript".into(),
            raw: "typescript".into(),
            soft: false,
            from_main: false,
            from_subpath: false,
            from_types: true,
        };
        let f = classify(&m, &[typescript]);
        let v = f.iter().find(|x| x.package == "typescript").unwrap();
        assert_eq!(v.verdict, Verdict::TypeOnly);
    }

    #[test]
    fn subpath_only_hard_phantom_is_the_adapter_class() {
        let m = Manifest::parse(br#"{"name":"@hookform/resolvers"}"#).unwrap();
        // A hard phantom reached only from a subpath export is the adapter class;
        // one reached from the main graph is not.
        let subpath_only = Reference {
            package: "zod".into(),
            raw: "zod/v4/core".into(),
            soft: false,
            from_main: false,
            from_subpath: true,
            from_types: false,
        };
        let main_reached = Reference {
            package: "junk".into(),
            raw: "junk".into(),
            soft: false,
            from_main: true,
            from_subpath: false,
            from_types: false,
        };
        let f = classify(&m, &[subpath_only, main_reached]);
        let zod = f.iter().find(|x| x.package == "zod").unwrap();
        let junk = f.iter().find(|x| x.package == "junk").unwrap();
        assert!(zod.is_subpath_adapter());
        assert!(!junk.is_subpath_adapter());
    }
}
