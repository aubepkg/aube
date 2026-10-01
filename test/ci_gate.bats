#!/usr/bin/env bats

# Covers the decision logic behind the required `final` check in ci.yml:
# which path-gated builds are needed, and how job results are judged.

setup() {
	load 'test_helper/common_setup'
	_common_setup
	CHANGES="$PROJECT_ROOT/.github/scripts/ci-changes.sh"
	FINAL="$PROJECT_ROOT/.github/scripts/ci-final.py"
	export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
	export GITHUB_OUTPUT="$TEST_TEMP_DIR/output"
	: >"$GITHUB_OUTPUT"
	git init -q repo
	cd repo
	echo base >README.md
	git add -A
	git commit -q -m base
	BASE_SHA="$(git rev-parse HEAD)"
}

teardown() {
	_common_teardown
}

# Commit the given paths on top of the base commit.
_commit_paths() {
	local p
	for p in "$@"; do
		mkdir -p "$(dirname "$p")"
		echo change >>"$p"
	done
	git add -A
	git commit -q -m change
}

_pr_outputs() {
	_commit_paths "$@"
	EVENT_NAME=pull_request run "$CHANGES"
	assert_success
}

@test "changes: docs-only pull request builds neither package" {
	_pr_outputs docs/a.md
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=false"
	assert_line "node_addon=false"
}

@test "changes: ffi crate change builds only ffi" {
	_pr_outputs crates/aube-ffi/src/lib.rs
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=false"
}

@test "changes: node addon crate change builds only node-addon" {
	_pr_outputs crates/aube-node/src/lib.rs
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=false"
	assert_line "node_addon=true"
}

@test "changes: node-addon builds for registry config but not the rest of the registry" {
	_pr_outputs crates/aube-registry/src/config/auth.rs
	run cat "$GITHUB_OUTPUT"
	assert_line "node_addon=true"
	: >"$GITHUB_OUTPUT"
	git reset -q --hard "$BASE_SHA"
	_pr_outputs crates/aube-registry/src/client.rs
	run cat "$GITHUB_OUTPUT"
	assert_line "node_addon=false"
}

@test "changes: shared inputs build both packages" {
	_pr_outputs Cargo.lock
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=true"
}

@test "changes: aube-codes source builds both but its changelog builds neither" {
	_pr_outputs crates/aube-codes/src/errors.rs
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=true"
	: >"$GITHUB_OUTPUT"
	git reset -q --hard "$BASE_SHA"
	_pr_outputs crates/aube-codes/CHANGELOG.md
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=false"
	assert_line "node_addon=false"
}

@test "changes: each package's own workflow file triggers only that package" {
	_pr_outputs .github/workflows/ffi-impl.yml
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=false"
}

@test "changes: moving a shared input away still builds both packages" {
	mkdir -p crates/aube/src
	echo embed >crates/aube/src/embed.rs
	git add -A
	git commit -q -m embed
	BASE_SHA="$(git rev-parse HEAD)"
	mkdir -p docs
	git mv crates/aube/src/embed.rs docs/embed.rs
	git commit -q -m move
	EVENT_NAME=pull_request run "$CHANGES"
	assert_success
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=true"
}

@test "changes: an early match in a very long file list is still a match" {
	mkdir -p crates/aube-ffi/src docs/bulk
	echo x >crates/aube-ffi/src/lib.rs
	local i
	for i in $(seq 1 4000); do
		echo x >"docs/bulk/file-with-a-fairly-long-name-to-fill-the-pipe-$i.md"
	done
	git add -A
	git commit -q -m bulk
	EVENT_NAME=pull_request run "$CHANGES"
	assert_success
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
}

@test "changes: push diffs against the previous commit" {
	_commit_paths crates/aube-ffi/src/lib.rs
	EVENT_NAME=push BEFORE_SHA="$BASE_SHA" HEAD_SHA="$(git rev-parse HEAD)" run "$CHANGES"
	assert_success
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=false"
}

@test "changes: a push whose base can't be diffed builds everything" {
	_commit_paths docs/a.md
	EVENT_NAME=push BEFORE_SHA=0000000000000000000000000000000000000000 HEAD_SHA="$(git rev-parse HEAD)" run "$CHANGES"
	assert_success
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=true"
}

@test "changes: workflow_dispatch builds everything" {
	EVENT_NAME=workflow_dispatch run "$CHANGES"
	assert_success
	run cat "$GITHUB_OUTPUT"
	assert_line "ffi=true"
	assert_line "node_addon=true"
}

# $1 ffi result, $2 node-addon result, $3 ffi wanted, $4 node-addon wanted,
# $5 changes result, $6 ci result
_final() {
	NEEDS_JSON=$(
		cat <<JSON
{
  "ci": {"result": "${6:-success}", "outputs": {}},
  "zizmor": {"result": "success", "outputs": {}},
  "changes": {"result": "${5:-success}", "outputs": {"ffi": "$3", "node_addon": "$4"}},
  "ffi": {"result": "$1", "outputs": {}},
  "node-addon": {"result": "$2", "outputs": {}}
}
JSON
	) run "$FINAL"
}

@test "final: passes when unneeded builds are skipped" {
	_final skipped skipped false false
	assert_success
}

@test "final: passes when needed builds succeed" {
	_final success success true true
	assert_success
}

@test "final: fails when a needed build is skipped" {
	_final skipped success true true
	assert_failure
	assert_output --partial "ffi: skipped (expected success)"
}

@test "final: fails when a needed build fails" {
	_final success failure true true
	assert_failure
	assert_output --partial "node-addon: failure (expected success)"
}

@test "final: fails when the path detection job fails" {
	_final skipped skipped "" "" failure
	assert_failure
	assert_output --partial "changes: failure"
}

@test "final: fails when the main ci workflow fails or is skipped" {
	_final skipped skipped false false success failure
	assert_failure
	assert_output --partial "ci: failure"
	_final skipped skipped false false success skipped
	assert_failure
}

@test "final: fails when a changes output is missing or malformed" {
	_final skipped skipped "" false
	assert_failure
	assert_output --partial "changes output 'ffi' is ''"
	_final skipped skipped false maybe
	assert_failure
	assert_output --partial "changes output 'node_addon' is 'maybe'"
}
