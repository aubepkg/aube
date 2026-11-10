#!/bin/bash
set -euo pipefail

# Small dependency groups first; keep the full-suite command for the warm run.
mkdir -p build-memory-results
MBX_BLAZE=0 cargo metadata --locked --offline --format-version 1 --no-deps \
	>build-memory-results/workspace.json
python3 - <<'PY' >build-memory-results/prime-packages.txt
import json
from pathlib import Path
metadata = json.loads(Path('build-memory-results/workspace.json').read_text())
packages = {p['name']: p for p in metadata['packages']}
selected = set(metadata['workspace_default_members'])

def dependencies(name, seen):
    for dep in packages[name]['dependencies']:
        target = dep['name']
        if target in packages and target not in seen:
            seen.add(target)
            dependencies(target, seen)
    return seen

for package in sorted(packages.values(), key=lambda p: (len(dependencies(p['name'], {p['name']})), p['name'])):
    if package['id'] in selected:
        print(package['name'])
PY
[[ -s build-memory-results/prime-packages.txt ]]
# Stop on failure too, so no daemon survives this priming job.
trap 'blaze stop' EXIT
while IFS= read -r package; do
	printf '\nPriming package: %s\n' "$package"
	cargo test --locked -p "$package" --lib --bins --tests -- --test-threads=1
	blaze stop
	printf 'Primed and stopped: %s\n' "$package"
done <build-memory-results/prime-packages.txt
trap - EXIT
MBX_BLAZE=0 cargo test --locked --doc
