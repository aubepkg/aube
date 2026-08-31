use aube_store::{PackageIndex, StoredFile};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut index = PackageIndex::default();
    for i in 0..64 {
        index.insert(
            format!("lib/components/component-{i}/index.js"),
            StoredFile {
                hex_hash: format!("{i:064x}"),
                store_path: PathBuf::from(format!("/store/files/{:02x}/{i:062x}", i % 256)),
                executable: i % 17 == 0,
                size: Some(1024 + i),
            },
        );
    }

    const ITERATIONS: usize = 100_000;
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(serde_json::to_vec(black_box(&index)).unwrap());
    }
    let serde_json = started.elapsed();

    let started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(sonic_rs::to_vec(black_box(&index)).unwrap());
    }
    let sonic = started.elapsed();

    println!("serde_json: {serde_json:?}");
    println!("sonic-rs:   {sonic:?}");
    println!(
        "speedup:    {:.2}x",
        serde_json.as_secs_f64() / sonic.as_secs_f64()
    );
}
