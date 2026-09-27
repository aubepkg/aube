//! Parser for bun's `bun.lock` (text JSONC format, bun 1.1+).
//!
//! The `bun.lockb` binary format is NOT supported — users should run
//! `bun install --save-text-lockfile` first (or upgrade to bun 1.2+
//! where text is the default).
//!
//! Format overview:
//!
//! ```jsonc
//! {
//!   "lockfileVersion": 1, // or 2 (bun 1.4+); same content
//!   "workspaces": {
//!     "": {
//!       "name": "my-app",
//!       "dependencies": { "foo": "^1.0.0" },
//!       "devDependencies": { "bar": "^2.0.0" }
//!     }
//!   },
//!   "packages": {
//!     "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "sha512-..."],
//!     "nested": ["nested@3.1.0", "", {}, "sha512-..."]
//!   }
//! }
//! ```
//!
//! Each `packages` entry is a 4-tuple `[ident, resolved_url, metadata, integrity]`,
//! where `ident` is `name@version` and `metadata` may carry transitive
//! `dependencies` / `optionalDependencies`.
//!
//! The file uses JSONC: trailing commas and `//`/`/* */` comments are
//! allowed. We pre-process the content to strip those before handing it
//! to `serde_json`.

mod jsonc;
mod overrides;
mod raw;
mod read;
mod source;
mod write;

#[cfg(test)]
mod tests;

pub use read::parse;
pub use write::write;

/// `lockfileVersion` values the parser reads. bun 1.4 writes v2, which has
/// the same content as v1 and only makes bun's own parser stricter (npm
/// tarballs outside the configured registry must carry an integrity hash,
/// and git tags must be safe path components). v3 is v2 plus scoped
/// `overrides` groups, and bun stamps it only while such rules exist.
const SUPPORTED_LOCKFILE_VERSIONS: [u32; 3] = [1, 2, 3];

/// `LockfileGraph::extra_fields` key carrying a non-default
/// `lockfileVersion` from the parser to the writer.
const LOCKFILE_VERSION_KEY: &str = "lockfileVersion";
