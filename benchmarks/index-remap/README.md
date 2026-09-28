# Package-index remapping

Moving the first placement's file index avoids a deep copy during fresh resolution.
Further peer-context placements still receive independent copies. The public store
API and cache format are unchanged.

This follows upm's approach of avoiding expanded file-index copies between install
stages: [upm link.ts](https://github.com/unjs/upm/blob/16ad722f616e5ddb7b55f67440d75b4d1a1c2c09/src/link.ts#L611).
The implementation here uses Rust ownership rather than upm's worker protocol.

## Measurement

These are **remapping microbenchmarks**, not whole-install speedups. Each row is
one package, with either one placement or eight peer contexts. Setup, file-index
construction, and dropping the output are outside the timer. The harness calls the
production remapping function and checks placement counts and file metadata.

Ten paired process runs alternate base/candidate order. Both builds use optimized
release mode with `CARGO_PROFILE_RELEASE_LTO=false` and
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`. Linux x86-64, AMD EPYC 9V45, four visible
CPUs; Rust 1.98.1. Load averages after measurement: 2.81 / 1.91 / 1.41.

Base: `8f375c93`. Candidate: `1af92b9486328e992dc4f1cdef386119d112b467`.
[Run and checks](https://github.com/jdalton/aube/actions/runs/36433479105).
[Raw paired samples](results.json), recorded in milliseconds for each batch.

| Files | Placements | Base median per remap | Candidate median per remap | Reduction |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 1 | 0.756 µs | 0.162 µs | 78.6% |
| 16 | 8 | 5.317 µs | 5.093 µs | 4.2% |
| 1,024 | 1 | 35.871 µs | 0.288 µs | 99.2% |
| 1,024 | 8 | 1,084.742 µs | 933.423 µs | 13.9% |
| 10,000 | 1 | 1,180.547 µs | 0.521 µs | 99.96% |
| 10,000 | 8 | 10,774.509 µs | 9,490.031 µs | 11.9% |

For 1,024 files and eight placements, batch samples span 101.97–111.43 ms
at baseline and 89.03–94.70 ms after the change. The tiny eight-placement
case overlaps; its 4.2% median difference is not a reliable headline gain.
Frozen installs that skip resolution do not run this remapping step.

## Reproduce

Build the example in a clean candidate checkout:

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  mise exec -- cargo build --locked --release -p aube --example index_remap
cp target/release/examples/index_remap /tmp/index-candidate
```

In a separate checkout of the base revision, copy the candidate example into
`crates/aube/examples/index_remap.rs`. Extract the base implementation into the
module path the example imports:

```python
from pathlib import Path
root = Path("crates/aube")
source = (root / "src/commands/install/fetch.rs").read_text()
start = source.index("/// Re-key a canonical-indexed")
end = source.index("\n#[cfg(test)]", start)
(root / "src/commands/install/index_remap.rs").write_text(
    "use std::collections::BTreeMap;\n" + source[start:end])
p = root / "examples/index_remap.rs"
p.write_text(p.read_text().replace(
    "remap_indices_to_contextualized(input,",
    "remap_indices_to_contextualized(&input,"))
```

Build with the same command and copy the base executable to `/tmp/index-base`.
Run `compare.py` from the candidate checkout. Both executables receive the same
file count, iterations and peer-context count. No network or filesystem placement
occurs inside the timed operation.
