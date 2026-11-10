#!/bin/bash
set -euo pipefail

# Reproduce upstream #1780, then check our one-line dependency workaround.
repo=$(pwd)
probe=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/aube-macro.XXXXXX")
mkdir -p "$repo/build-memory-results"
python3 - "$repo" "$probe" <<'PY'
from pathlib import Path
import shutil
import sys
repo, probe = map(Path, sys.argv[1:])
for case in ('original', 'patched'):
    root = probe / case
    shutil.copytree(repo / 'patches/multiversion_no_op', root / 'mac')
    if case == 'original':
        source = root / 'mac/src/lib.rs'
        source.write_text('#![no_std]\n' + source.read_text())
    (root / 'Cargo.toml').write_text('''[package]
name = "macro-compat"
version = "0.1.0"
edition = "2021"
[dependencies]
multiversion_no_op = { path = "mac" }
[workspace]
''')
    (root / 'src').mkdir()
    (root / 'src/lib.rs').write_text('''#![no_std]
#[multiversion_no_op::multiversion(targets("x86_64+avx2"))]
pub const fn unchanged(value: u32) -> u32 { value }
#[test]
fn attribute_preserves_item() { assert_eq!(unchanged(42), 42); }
''')
PY
for case in original patched; do
	(cd "$probe/$case" && MBX_DISABLE=1 MBX_BLAZE=0 cargo test --offline)
done
if [[ ${1:-} == --cargo-only ]]; then exit; fi
if (cd "$probe/original" && blaze test) >build-memory-results/macro-original.log 2>&1; then
	echo 'Original macro now passes: remove the temporary dependency patch.' >&2
	exit 1
fi
# Reject unrelated startup or filesystem errors as evidence for the workaround.
grep -Eq 'ManuallyDrop|Vec::from_raw_parts|cannot find.*std' build-memory-results/macro-original.log
(cd "$probe/patched" && blaze test) 2>&1 | tee build-memory-results/macro-patched.log
