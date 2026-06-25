//! Benchmarks the per-dependency materialization of a resolver task's
//! ancestor chain: the old `Vec<(String, String)>` deep-clone per
//! enqueued dependency vs the `Arc<[(String, String)]>` refcount bump.
//!
//! Run with:
//!   cargo bench -p aube-resolver --bench ancestor_chain
//!
//! Background: when the resolver processes a package it builds that
//! package's child ancestor chain once (parent chain + the package's own
//! `(name, version)` frame), then hands a copy to *every* dependency it
//! enqueues. The chain is read-only after it is built. Previously the
//! field was a `Vec`, so each per-dep enqueue deep-cloned the whole
//! chain — cost `O(edges × depth)` over the resolve, with every frame's
//! two `String`s reallocated. Switching the field to `Arc<[_]>` makes
//! the per-dep hand-off a refcount bump while keeping a single chain
//! allocation per package.
//!
//! The fixture mirrors that hot path directly: a `depth`-deep chain of
//! realistic-length scoped-package frames, fanned out to `fanout`
//! dependencies, summed across `packages` packages — the
//! `edges × depth` shape the change targets. The benched closures are
//! exactly the two materialization strategies; everything else (chain
//! construction, the fold over deps) is identical between them.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// Build a `depth`-deep ancestor chain of realistic-length frames
/// (scoped names + semver versions), the shape an ancestor chain takes
/// deep in a real dependency graph.
fn build_chain(depth: usize) -> Vec<(String, String)> {
    (0..depth)
        .map(|i| (format!("@scope/package-{i:04}"), format!("{i}.2.3")))
        .collect()
}

/// Old behavior: the child chain is a `Vec`, so each of `fanout`
/// dependencies deep-clones it. Returns a length sum so the work can't
/// be optimized away.
fn vec_per_dep(parent: &[(String, String)], fanout: usize) -> usize {
    let mut child: Vec<(String, String)> = parent.to_vec();
    child.push(("@scope/parent".to_string(), "9.9.9".to_string()));
    let mut total = 0;
    for _ in 0..fanout {
        // What `ResolveTask::transitive(.., child_ancestors.clone())`
        // did when `ancestors` was a `Vec`: a full deep clone per dep.
        let dep_ancestors: Vec<(String, String)> = child.clone();
        total += dep_ancestors.len();
        black_box(&dep_ancestors);
    }
    total
}

/// New behavior: the child chain is frozen to an `Arc<[_]>` once, then
/// each of `fanout` dependencies takes a refcount bump.
fn arc_per_dep(parent: &[(String, String)], fanout: usize) -> usize {
    let mut child: Vec<(String, String)> = parent.to_vec();
    child.push(("@scope/parent".to_string(), "9.9.9".to_string()));
    let child: Arc<[(String, String)]> = child.into();
    let mut total = 0;
    for _ in 0..fanout {
        // What `ResolveTask::transitive(.., child_ancestors.clone())`
        // does now that `ancestors` is an `Arc<[_]>`: a refcount bump.
        let dep_ancestors: Arc<[(String, String)]> = Arc::clone(&child);
        total += dep_ancestors.len();
        black_box(&dep_ancestors);
    }
    total
}

fn bench_ancestor_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("ancestor_chain");

    // (depth, fanout, packages): a shallow-but-wide tree and a
    // deep-and-wide tree, each summed over many packages so the bench
    // reflects the whole resolve's edge count, not one package.
    for (depth, fanout, packages) in [(8usize, 16usize, 2000usize), (24, 16, 2000)] {
        let parent = build_chain(depth);
        let id = format!("depth{depth}_fanout{fanout}");

        // Both strategies produce the same per-dep chain length, so the
        // length sums must agree — the byte-identical guard for the
        // micro-bench.
        let vec_sum: usize = (0..packages).map(|_| vec_per_dep(&parent, fanout)).sum();
        let arc_sum: usize = (0..packages).map(|_| arc_per_dep(&parent, fanout)).sum();
        assert_eq!(
            vec_sum, arc_sum,
            "vec and arc strategies disagree at depth={depth} fanout={fanout}",
        );

        group.bench_with_input(BenchmarkId::new("vec_clone", &id), &packages, |b, &n| {
            b.iter(|| {
                let mut total = 0;
                for _ in 0..n {
                    total += vec_per_dep(&parent, fanout);
                }
                black_box(total)
            })
        });
        group.bench_with_input(BenchmarkId::new("arc_bump", &id), &packages, |b, &n| {
            b.iter(|| {
                let mut total = 0;
                for _ in 0..n {
                    total += arc_per_dep(&parent, fanout);
                }
                black_box(total)
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_ancestor_chain);
criterion_main!(benches);
