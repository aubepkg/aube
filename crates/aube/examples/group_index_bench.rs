//! Paired timing probe: per-dep_path verified index loads vs grouping
//! loads by store identity. Mirrors the fetch.rs check boundary.
use aube_store::{PackageIndex, Store, StoredFile};
use std::collections::BTreeMap;
use std::time::Instant;

fn build(store: &Store, files: usize) -> PackageIndex {
    let mut index = PackageIndex::default();
    for i in 0..files {
        let file = store.root().join(format!("file-{i}"));
        std::fs::write(&file, b"fixture").unwrap();
        index.insert(
            format!("file-{i}"),
            StoredFile {
                hex_hash: format!("{i:064x}"),
                store_path: file,
                executable: false,
                size: Some(7),
            },
        );
    }
    store
        .save_index("bench-pkg", "1.0.0", None, &index)
        .unwrap();
    index
}

fn main() {
    let files = 1024usize;
    let placements = [1usize, 8, 32];
    let pairs = 15usize;
    let mut results = Vec::new();
    for placements in placements {
        let mut base_samples = Vec::new();
        let mut grouped_samples = Vec::new();
        for pair in 0..pairs + 2 {
            let fixture = std::env::temp_dir().join(format!(
                "aube-group-bench-{}-{placements}",
                std::process::id()
            ));
            let store = Store::with_dirs(fixture.join("files"), fixture.join("cache"));
            std::fs::create_dir_all(store.root()).unwrap();
            let index = build(&store, files);
            // warm-up pairs run once each before samples are taken
            let warm = pair < 2;

            let start = Instant::now();
            let baseline: BTreeMap<usize, PackageIndex> = (0..placements)
                .map(|i| {
                    (
                        i,
                        store
                            .load_index_verified("bench-pkg", "1.0.0", None)
                            .unwrap(),
                    )
                })
                .collect();
            let base = start.elapsed();

            let start = Instant::now();
            let canonical = store
                .load_index_verified("bench-pkg", "1.0.0", None)
                .unwrap();
            let grouped: BTreeMap<usize, PackageIndex> =
                (0..placements).map(|i| (i, canonical.clone())).collect();
            let group = start.elapsed();

            assert_eq!(baseline.len(), grouped.len());
            for i in 0..placements {
                assert_eq!(baseline[&i].len(), grouped[&i].len());
                for (path, entry) in &baseline[&i] {
                    assert_eq!(entry.hex_hash, grouped[&i][path].hex_hash);
                    assert_eq!(entry.store_path, grouped[&i][path].store_path);
                }
            }
            drop((baseline, grouped));
            drop(index);
            std::fs::remove_dir_all(&fixture).unwrap();
            if !warm {
                base_samples.push(base.as_secs_f64() * 1000.0);
                grouped_samples.push(group.as_secs_f64() * 1000.0);
            }
        }
        base_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        grouped_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let base_median = base_samples[base_samples.len() / 2];
        let group_median = grouped_samples[grouped_samples.len() / 2];
        results.push(serde_json::json!({
            "files": files,
            "placements": placements,
            "baseline_ms": base_median,
            "grouped_ms": group_median,
            "reduction_pct": (1.0 - group_median / base_median) * 100.0,
        }));
    }
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
