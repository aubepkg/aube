//! Old-vs-new parse benchmark for `pnpm-lock.yaml`.
//!
//! Compares the general `yaml_serde` parse path against the byte-cursor
//! subset parser on a large generated lockfile. Set
//! `AUBE_BENCH_LOCKFILE=/path/to/pnpm-lock.yaml` to point at a real
//! lockfile instead of the bundled small fixture.

use aube_lockfile::pnpm;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn load() -> String {
    if let Ok(p) = std::env::var("AUBE_BENCH_LOCKFILE") {
        return std::fs::read_to_string(p).expect("read AUBE_BENCH_LOCKFILE");
    }
    include_str!("../tests/fixtures/pnpm-native.yaml").to_string()
}

fn bench(c: &mut Criterion) {
    let content = load();
    eprintln!("benchmarking pnpm-lock.yaml: {} bytes", content.len());

    // Sanity: both paths must agree on the structure they parse.
    let s = pnpm::__bench_parse_subset(&content);
    let y = pnpm::__bench_parse_serde(&content);
    assert!(s.is_some(), "subset parser declined the bench input");
    assert_eq!(s, y, "subset and serde parsers disagree on counts");

    let mut g = c.benchmark_group("pnpm-lock-parse");
    g.bench_function("serde", |b| {
        b.iter(|| black_box(pnpm::__bench_parse_serde(black_box(&content))))
    });
    g.bench_function("subset", |b| {
        b.iter(|| black_box(pnpm::__bench_parse_subset(black_box(&content))))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
