#!/bin/sh
set -eu

# Run from setup-blaze.sh so Cargo, Blaze, and rustc use the same toolchain.
# Release the JIT workspace before rustdoc builds its own copy of the libraries.
trap 'blaze stop' EXIT
cargo test --lib --bins --tests -- --test-threads=1
blaze stop
trap - EXIT
MBX_BLAZE=0 cargo test --doc
