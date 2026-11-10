#!/bin/bash
set -euo pipefail

# Measure both configurations on one Linux runner. A systemd cgroup includes
# detached Blaze workers as well as Cargo/rustdoc and their child processes.
mkdir -p build-memory-results
# Fetch once outside the measured groups; neither variant pays for downloads.
mise exec -- sh scripts/setup-blaze.sh env MBX_BLAZE=0 MBX_DISABLE=1 cargo fetch
for variant in previous revised; do
	unit="aube-memory-${variant}-${GITHUB_RUN_ID:-local}"
	target="$RUNNER_TEMP/aube-memory-$variant"
	if [[ $variant == previous ]]; then
		command='unset CARGO_BUILD_JOBS; cargo test'
	else
		command='export CARGO_BUILD_JOBS=2; sh scripts/test-ci.sh'
	fi
	started=$(date +%s)
	sudo systemd-run --unit="$unit" --no-block \
		--property=Type=oneshot --property=RemainAfterExit=yes \
		--property=MemoryAccounting=yes --property=MemoryMax=12G \
		--property=TimeoutStartSec=1800 \
		--working-directory="$PWD" --uid="$(id -u)" \
		--setenv="HOME=$HOME" --setenv="PATH=$PATH" \
		--setenv="MISE_TRUSTED_CONFIG_PATHS=$PWD" \
		--setenv="CARGO_TARGET_DIR=$target" --setenv=CARGO_INCREMENTAL=0 \
		--setenv="RA_JIT_CACHE=$RUNNER_TEMP/aube-jit-$variant" \
		--setenv=MBX_DISABLE=1 --setenv=RUSTFLAGS=-Dwarnings \
		bash -c ". .blaze/env.sh; export MBX_DISABLE=1; $command"
	# RemainAfterExit retains cgroup counters after success. Wait for the main
	# process to finish, not for the unit to become inactive (it stays active).
	deadline=$((SECONDS + 1900))
	peak_anon=0
	group="/sys/fs/cgroup$(sudo systemctl show "$unit" --property=ControlGroup --value)"

	while :; do
		if [[ -r $group/memory.stat ]]; then
			anon=$(awk '$1 == "anon" || $1 == "shmem" { total += $2 } END { printf "%.0f", total }' "$group/memory.stat")
			if ((anon > peak_anon)); then peak_anon=$anon; fi
		fi
		state=$(sudo systemctl show "$unit" --property=SubState --value)
		case "$state" in
		exited | failed) break ;;
		esac
		if ((SECONDS >= deadline)); then
			printf 'Timed out waiting for %s (state %s)\n' "$unit" "$state" >&2
			sudo systemctl stop "$unit"
			break
		fi
		sleep 0.2
	done
	elapsed=$(($(date +%s) - started))
	# Output files belong to the runner user; only reading systemd needs sudo.
	# shellcheck disable=SC2024
	sudo systemctl show "$unit" -p MemoryPeak -p CPUUsageNSec -p Result \
		>"build-memory-results/$variant.properties"
	printf 'ElapsedSeconds=%s\n' "$elapsed" >>"build-memory-results/$variant.properties"
	printf 'SampledAnonymousPeak=%s\n' "$peak_anon" >>"build-memory-results/$variant.properties"
	if [[ -r $group/memory.events ]]; then
		cat "$group/memory.events" >"build-memory-results/$variant.memory-events"
	fi
	# shellcheck disable=SC2024
	sudo journalctl -u "$unit" --no-pager >"build-memory-results/$variant.log"
	sudo systemctl stop "$unit" || true
	rm -rf "$target" "$RUNNER_TEMP/aube-jit-$variant"
done
python3 - <<'PY'
import json, pathlib
out={}
for path in pathlib.Path('build-memory-results').glob('*.properties'):
    values=dict(line.split('=',1) for line in path.read_text().splitlines())
    out[path.stem]=values
pathlib.Path('build-memory-results/results.json').write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps(out,indent=2))
PY
