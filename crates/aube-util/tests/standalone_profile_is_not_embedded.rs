//! `set_embedder(&AUBE)` — what aube's own binary does on every run — must
//! not read as embedding.
//!
//! Lives in its own integration-test binary (= its own process) because the
//! active profile is once-per-process, and because the bug this pins only
//! appears when the registration crosses a crate boundary the way
//! `aube::cli_main` does: [`AUBE`] is a `const`, so the `&AUBE` registered
//! there and the `&AUBE` inside `aube-util` are distinct values. Comparing
//! them by address made standalone aube look like a guest and handed its
//! lifecycle scripts the embedded `npm_execpath` shim instead of the aube
//! binary itself.

use aube_util::{AUBE, is_embedded, set_embedder};

#[test]
fn registering_aubes_own_profile_is_not_embedding() {
    assert!(!is_embedded(), "nothing registered yet");
    set_embedder(&AUBE);
    assert!(!is_embedded());
}
