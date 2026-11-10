Temporary Aube patch: remove `#![no_std]` from this host-side proc-macro crate
to work around https://github.com/indexable-inc/blaze/issues/1780. The macro
body is unchanged. Remove this copy and the root Cargo patch after a release
containing https://github.com/indexable-inc/blaze/pull/1834 passes our probe.

# `multiversion_no_op`

This crate provides a pass-through function attribute called `multiversion`.

The purpose of this crate is to optimize build times on targets that do not actually
need multiversioning. That is, targets that need multiversioning can use the real
[`multiversion`](https://crates.io/crates/multiversion) crate, and targets that do
not need multiversioning can use this crate instead.

This crate works in the `no_std` context.

## License 

Apache-2.0 OR MIT; the file named `COPYRIGHT`.

## Usage

See [`encoding_rs`](https://github.com/hsivonen/encoding_rs/) crate for a usage example.
