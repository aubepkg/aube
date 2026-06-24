//! Benchmarks the per-package dep-path filename encode the linker runs
//! while populating the virtual store.
//!
//! Run with:
//!   cargo bench -p aube-lockfile --bench dep_path_filename
//!
//! `dep_path_to_filename` is a `String` alloc plus an escape pass and an
//! uppercase scan, and on long/scoped/peer-context names a second alloc
//! and a BLAKE3 short-hash. On the materialize path the linker used to
//! call it up to 4× per package — the local `.aube/<entry>` name in the
//! serial pre-pass and again in the par_iter, plus the virtual-store
//! subdir in the par_iter and a third encode of it downstream in
//! `ensure_in_virtual_store`. The hoist computes the entry name and the
//! subdir once each in the pre-pass and threads them through, leaving 2
//! encodes per package. This bench quantifies that trim by timing the
//! function across representative dep-path shapes and contrasting a
//! 4-encodes-per-package loop with the hoisted 2-encodes loop over a
//! synthetic 1.5k-package graph.

use std::hint::black_box;

use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const MAX: usize = DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;

/// One dep-path per branch of `dep_path_to_filename`.
const SHAPES: &[(&str, &str)] = &[
    ("plain", "lodash@4.17.21"),
    ("scoped", "@babel/core@7.29.7"),
    ("peer_context", "react-dom@18.3.1(react@18.3.1)"),
    ("uppercase_hashed", "@ng/Core@17.0.0"),
    (
        "long_peer_blake3",
        "@fig/eslint-config-autocomplete@2.0.0(@typescript-eslint+eslint-plugin@7.18.0(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(eslint@8.57.1))(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(@withfig+eslint-plugin-fig-linter@1.4.1)(eslint@8.57.1)(eslint-plugin-compat@4.2.0(eslint@8.57.1))(typescript@5.9.3)",
    ),
];

fn bench_single_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("dep_path_to_filename");
    for (label, dep_path) in SHAPES {
        group.bench_with_input(BenchmarkId::from_parameter(label), dep_path, |b, dp| {
            b.iter(|| dep_path_to_filename(black_box(dp), black_box(MAX)));
        });
    }
    group.finish();
}

/// Synthetic graph mirroring a medium install: a realistic mix of plain,
/// scoped, and peer-context dep paths.
fn build_dep_paths(n: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(match i % 4 {
            0 => format!("pkg-{i}@1.{i}.0"),
            1 => format!("@scope-{i}/name-{i}@2.{i}.0"),
            2 => format!("dep-{i}@3.{i}.0(react@18.3.1)(typescript@5.6.3)"),
            _ => format!("@types/node-{i}@20.{i}.0(eslint@8.57.1)"),
        });
    }
    out
}

/// Per-package encode work before vs after the hoist on the
/// materialize path. Before: 4 encodes — the entry name in the serial
/// pre-pass and again in the par_iter, plus the virtual-store subdir in
/// the par_iter and a third time downstream in
/// `ensure_in_virtual_store`. After: 2 encodes — the entry name and the
/// subdir computed once each in the pre-pass and threaded through.
/// (`virtual_store_subdir` is `dep_path_to_filename` of the
/// graph-hash-folded path; with no hashes installed it folds nothing,
/// so a no-hash baseline is modeled here as a bare encode of the same
/// input. The encode cost — alloc + scans + optional BLAKE3 — is the
/// same regardless of the fold, so the count delta is what this
/// measures.)
fn bench_per_package_encode_count(c: &mut Criterion) {
    let dep_paths = build_dep_paths(1_500);
    let mut group = c.benchmark_group("linker_prep_encodes");

    group.bench_function("before_4_encodes_per_package", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for dp in &dep_paths {
                // entry name (pre-pass) + entry name (par_iter)
                // + subdir (par_iter) + subdir (ensure_in_virtual_store).
                acc += dep_path_to_filename(black_box(dp), MAX).len();
                acc += dep_path_to_filename(black_box(dp), MAX).len();
                acc += dep_path_to_filename(black_box(dp), MAX).len();
                acc += dep_path_to_filename(black_box(dp), MAX).len();
            }
            black_box(acc)
        });
    });

    group.bench_function("after_2_encodes_per_package", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for dp in &dep_paths {
                // entry name + subdir computed once each, threaded through.
                let entry = dep_path_to_filename(black_box(dp), MAX);
                let subdir = dep_path_to_filename(black_box(dp), MAX);
                acc += entry.len() + subdir.len();
            }
            black_box(acc)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_single_encode, bench_per_package_encode_count);
criterion_main!(benches);
