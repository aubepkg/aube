#!/usr/bin/env bash
# Decides which path-gated builds the `final` check requires. Writes
# `ffi=<bool>` and `node_addon=<bool>` to $GITHUB_OUTPUT.
#
# Env: EVENT_NAME, BEFORE_SHA (push only), HEAD_SHA (push only).
# Pull requests diff the merge commit against its first parent (the base tip);
# anything that can't be diffed reliably builds everything.
set -euo pipefail

shared='crates/aube/src/(lib|embed)\.rs|crates/aube/src/commands/add/mod\.rs|crates/aube/src/commands/install/(control|dep_selection|frozen)\.rs|crates/aube-util/src/(identity|lib)\.rs|Cargo\.(toml|lock)'
ffi_paths="crates/aube-ffi/.*|crates/aube-codes/.*|\.github/workflows/ffi(-impl)?\.yml|$shared"
node_paths="crates/aube-node/.*|crates/aube-codes/.*|crates/aube-registry/src/config/.*|crates/aube-registry/src/lib\.rs|\.github/workflows/node-addon(-impl)?\.yml|$shared"

files=""
all=true
if [[ "$EVENT_NAME" == pull_request ]]; then
	files=$(git diff --name-only HEAD^1 HEAD) && all=false
elif [[ "$EVENT_NAME" == push ]]; then
	files=$(git diff --name-only "$BEFORE_SHA" "$HEAD_SHA") && all=false
fi

touched() {
	[[ "$all" == true ]] && return 0
	grep -Ev '^crates/aube-codes/CHANGELOG\.md$' <<<"$files" | grep -Eq "^($1)\$"
}

for pair in "ffi:$ffi_paths" "node_addon:$node_paths"; do
	if touched "${pair#*:}"; then
		echo "${pair%%:*}=true" >>"$GITHUB_OUTPUT"
	else
		echo "${pair%%:*}=false" >>"$GITHUB_OUTPUT"
	fi
done
