//! aube-phantom-scan — scan an already-installed package's PUBLISHED,
//! reachable code for UNDECLARED (phantom) dependencies, for `aube doctor`.
//!
//! Ported from nub's `nub-phantom-scan` (jdx/nub / nubjs/nub, MIT). nub reduces
//! the same walk + classify pipeline to a boolean eject decision consumed by its
//! install pipeline; `aube doctor` only needs the classified [`classify::Finding`]
//! list to report, so this crate stops at [`scan`] rather than nub's
//! `ScanResult`/`scan_extracted`/`scan_index` reduction.
//!
//! The pipeline: walk the module graph from `exports`/`main`/`bin` → extract
//! import/require specifiers (via `aube-phantom-core`'s oxc parser) → classify
//! each against the declared surface.

pub mod classify;
pub mod graph;
pub mod manifest;

use std::path::Path;

pub use classify::{Finding, Verdict};
use manifest::Manifest;

/// Scan an already-installed package tree rooted at `root` (the dir holding
/// `package.json`) and classify every reachable bare reference. Returns `None`
/// when the tree has no parseable `package.json` — the caller treats it as
/// "nothing to report", never a hard failure.
pub fn scan(root: &Path) -> Option<Vec<Finding>> {
    let raw = std::fs::read(root.join("package.json")).ok()?;
    let manifest = Manifest::parse(&raw)?;
    let walk = graph::walk(root, &manifest.entry_points);
    Some(classify::classify(&manifest, &walk.references))
}

#[cfg(test)]
mod tests {
    use super::scan;
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aube-phantom-scan-e2e-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn flags_hard_phantoms_only() {
        let root = scratch("fixture");
        fs::write(
            root.join("package.json"),
            r#"{
                "name": "demo",
                "main": "index.js",
                "dependencies": { "declared-dep": "1" },
                "peerDependencies": { "zod": "*" },
                "peerDependenciesMeta": { "zod": { "optional": true } }
            }"#,
        )
        .unwrap();
        fs::write(
            root.join("index.js"),
            r#"const a = require('declared-dep');
               const ghost = require('undeclared-ghost');
               let opt; try { opt = require('soft-ghost'); } catch {}
               let z; try { z = require('zod'); } catch {}"#,
        )
        .unwrap();

        let findings = scan(&root).unwrap();
        let verdict = |name: &str| findings.iter().find(|f| f.package == name).unwrap().verdict;
        assert_eq!(verdict("declared-dep"), super::Verdict::Declared);
        assert_eq!(verdict("undeclared-ghost"), super::Verdict::HardPhantom);
        assert_eq!(verdict("soft-ghost"), super::Verdict::SoftPhantom);
        assert_eq!(verdict("zod"), super::Verdict::DeclaredOptionalPeer);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn type_only_reference_to_a_self_typed_package_is_never_a_phantom() {
        // Regression: pnpm#14128 / nub's classify.rs fix. A type-only reference
        // reachable only from the `.d.ts` surface must classify TypeOnly even
        // when the referenced package (here, a stand-in for `typescript`) ships
        // its own types and has no separate `@types/<pkg>` twin declared.
        let root = scratch("dts-fixture");
        fs::write(
            root.join("package.json"),
            r#"{"name":"demo","main":"index.js","types":"index.d.ts"}"#,
        )
        .unwrap();
        fs::write(root.join("index.js"), "module.exports = {};").unwrap();
        fs::write(
            root.join("index.d.ts"),
            "import type { Program } from 'self-typed-pkg';\nexport declare const x: Program;\n",
        )
        .unwrap();

        let findings = scan(&root).unwrap();
        let verdict = findings
            .iter()
            .find(|f| f.package == "self-typed-pkg")
            .unwrap()
            .verdict;
        assert_eq!(verdict, super::Verdict::TypeOnly);
        let _ = fs::remove_dir_all(&root);
    }
}
