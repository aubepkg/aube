//! Isolates the file-index fanout used by fetch/materialize and peer contexts.
//! Run the same example on the base and candidate in release mode.
use aube_store::{PackageIndex, StoredFile};
use std::{hint::black_box, time::Instant};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let files: usize = args.get(1).map_or(1024, |v| v.parse().unwrap());
    let iterations: usize = args.get(2).map_or(1000, |v| v.parse().unwrap());
    let contexts: usize = args.get(3).map_or(8, |v| v.parse().unwrap());
    let index: PackageIndex = (0..files)
        .map(|i| {
            (
                format!("lib/component-{i}/index.js"),
                StoredFile {
                    hex_hash: format!("{i:064x}"),
                    store_path: format!("/private-store/files/{:02x}/{i:062x}", i % 256).into(),
                    executable: i % 17 == 0,
                    size: Some(1024),
                },
            )
        })
        .collect();
    let fanout = || {
        let copies: Vec<_> = (0..contexts).map(|_| black_box(&index).clone()).collect();
        assert_eq!(
            copies.iter().map(|copy| copy.len()).sum::<usize>(),
            files * contexts
        );
        black_box(copies);
    };
    for _ in 0..10 {
        fanout();
    }
    let start = Instant::now();
    for _ in 0..iterations {
        fanout();
    }
    println!("{:.6}", start.elapsed().as_secs_f64() * 1000.0);
}
