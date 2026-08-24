//! Phantom-classification primitives ported from nub's `nub-phantom-core`
//! (jdx/nub / nubjs/nub, MIT) for `aube doctor`'s undeclared-import check.
//!
//! - [`extract`] — oxc-parsed specifier occurrences, with guard modeling
//!   (try/catch, conditional branches → `soft`) and type-only erasure.
//! - [`specifier`] — bare-vs-relative-vs-nonpackage classification + package-name
//!   extraction (`lodash/fp` → `lodash`).
//! - [`builtins`] — Node builtin recognition (never a phantom).
//!
//! The *verdict* layer (what counts as a phantom against a declared surface)
//! lives in `aube-phantom-scan`, not here.

pub mod builtins;
pub mod extract;
pub mod specifier;
