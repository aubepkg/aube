#!/bin/bash
set -euo pipefail

mkdir -p build-memory-results/ticket-cache
out="$PWD/build-memory-results/ticket-cache"
printf 'compiler,mode,scope,attempt,status\n' >"$out/results.csv"
# Use rustup's Cargo directly: developer machines may wrap `cargo` with Blaze.
for compiler in cargo blaze; do
	if [[ $compiler == cargo ]]; then
		command=(rustup run nightly-2026-10-27 cargo)
	else
		command=("$PWD/.blaze/bin/blaze")
	fi
	"${command[@]}" test --locked -p aube-util --lib --no-run >"$out/$compiler-build.log" 2>&1
	for mode in default serial; do
		args=()
		if [[ $mode == serial ]]; then args=(--test-threads=1); fi
		for scope in isolated group suite; do
			case "$scope" in
			isolated)
				filter=http::ticket_cache::tests::max_per_host_evicts_oldest
				count=5
				;;
			group)
				filter=http::ticket_cache::tests
				count=20
				;;
			suite)
				filter=
				count=5
				;;
			esac
			for ((attempt = 1; attempt <= count; attempt++)); do
				status=0
				"${command[@]}" test --locked -p aube-util --lib "$filter" -- "${args[@]}" \
					>"$out/$compiler-$mode-$scope-$attempt.log" 2>&1 || status=$?
				printf '%s,%s,%s,%s,%s\n' "$compiler" "$mode" "$scope" "$attempt" "$status" | tee -a "$out/results.csv"
			done
		done
	done
	if [[ $compiler == blaze ]]; then blaze stop; fi
done
