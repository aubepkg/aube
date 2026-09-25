#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Cache dir resolves through aube-store::dirs::cache_dir, which honors
# $HOME (set by _common_setup) so each test gets an isolated cache.
_cache_dir() {
	echo "$HOME/.cache/aube/packuments-v1"
}

# Drop a fake corgi-cache file for `pkg` so list/delete/view have
# something to chew on without requiring network access.
_fake_cache_entry() {
	local pkg="$1"
	local safe="${pkg//\//__}"
	mkdir -p "$(_cache_dir)"
	cat >"$(_cache_dir)/${safe}.json" <<EOF
{
  "etag": "W/\"abc123\"",
  "last_modified": "Wed, 01 Jan 2025 00:00:00 GMT",
  "fetched_at": 1735689600,
  "packument": {
    "name": "${pkg}",
    "versions": {
      "1.0.0":  {"name": "${pkg}", "version": "1.0.0"},
      "9.0.0":  {"name": "${pkg}", "version": "9.0.0"},
      "10.0.0": {"name": "${pkg}", "version": "10.0.0"}
    },
    "dist-tags": {"latest": "10.0.0"}
  }
}
EOF
}

@test "aube cache --help" {
	run aube cache --help
	assert_success
	assert_output --partial "Inspect and manage the packument metadata cache"
	assert_output --partial "list-registries"
}

@test "aube cache path prints the resolved metadata cache directory" {
	run aube cache path
	assert_success
	assert_output "$HOME/.cache/aube"
}

@test "aube cache path honors cache-dir" {
	echo "cache-dir=$TEST_TEMP_DIR/custom-cache" >>.npmrc
	run aube cache path
	assert_success
	assert_output "$TEST_TEMP_DIR/custom-cache"
}

@test "aube cache path resolves relative cache-dir from the workspace root" {
	mkdir -p packages/lib
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	echo '{"name":"root","private":true}' >package.json
	echo '{"name":"lib","version":"1.0.0"}' >packages/lib/package.json
	echo 'cache-dir=.cache/aube' >.npmrc
	workspace_root="$(pwd -P)"

	cd packages/lib
	run aube cache path
	assert_success
	assert_output "$workspace_root/.cache/aube"
}

@test "aube cache path prefers a nested yaml-only workspace over an outer project" {
	mkdir -p tools/packages/lib
	echo '{"name":"outer","private":true}' >package.json
	cat >tools/pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	echo 'cache-dir=.cache/aube' >tools/.npmrc
	workspace_root="$(cd tools && pwd -P)"

	cd tools/packages/lib
	run aube cache path
	assert_success
	assert_output "$workspace_root/.cache/aube"

	run aube view is-odd --json
	assert_success
	[ -d "$TEST_TEMP_DIR/tools/.cache/aube/packuments-full-v1" ]
	[ ! -e "$TEST_TEMP_DIR/tools/packages/lib/.cache/aube" ]
}

@test "aube cache list on an empty cache prints nothing" {
	run aube cache list
	assert_success
	assert_output ""
}

@test "aube cache list prints cached package names (decoded)" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	run aube cache list
	assert_success
	assert_line "lodash"
	assert_line "@babel/core"
}

@test "aube cache list filters by glob pattern" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	_fake_cache_entry "@babel/parser"
	run aube cache list "@babel/*"
	assert_success
	assert_line "@babel/core"
	assert_line "@babel/parser"
	refute_line "lodash"
}

@test "aube cache view summarizes a cached entry" {
	_fake_cache_entry "lodash"
	run aube cache view lodash
	assert_success
	assert_output --partial "lodash (corgi)"
	assert_output --partial "versions:      3"
	# Regression guard: must use semver ordering, not lexicographic
	# (otherwise "9.0.0" would beat "10.0.0").
	assert_output --partial "highest:       10.0.0"
	assert_output --partial "latest: 10.0.0"
	assert_output --partial "etag:          W/\"abc123\""
}

@test "aube cache view --json dumps the raw cache file" {
	_fake_cache_entry "lodash"
	run aube cache view --json lodash
	assert_success
	assert_output --partial "\"etag\": \"W/\\\"abc123\\\"\""
	assert_output --partial "\"fetched_at\": 1735689600"
}

@test "aube cache view errors on a cold cache" {
	run aube cache view totally-not-cached
	assert_failure
	assert_output --partial "no cached metadata"
}

@test "aube cache delete removes matching entries" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	run aube cache delete "@babel/*"
	assert_success
	assert_output --partial "removed"
	# lodash should still be there
	run aube cache list
	assert_success
	assert_line "lodash"
	refute_line "@babel/core"
}

@test "aube cache delete errors when nothing matched" {
	_fake_cache_entry "lodash"
	run aube cache delete "@babel/*"
	assert_failure
	assert_output --partial "no cached packages matched"
}

@test "aube cache list-registries prints the default registry" {
	# _common_setup writes registry=$AUBE_TEST_REGISTRY into .npmrc when
	# the var is set, otherwise the built-in default applies.
	run aube cache list-registries
	assert_success
	assert_output --partial "default:"
}

@test "cache list and delete cover registry partitions and exact versions" {
	cache_root="$HOME/.cache/aube"
	for subdir in packuments-v1/origin-one packuments-full-v1/origin-two; do
		mkdir -p "$cache_root/$subdir"
		echo '{}' >"$cache_root/$subdir/@scope__pkg.json"
		echo '{}' >"$cache_root/$subdir/keep.json"
	done
	exact="$cache_root/packuments-full-v1/exact-v1/origin-two/@scope__pkg"
	mkdir -p "$exact"
	echo '{"exact":{"metadata":{"version":"1.0.0"}}}' >"$exact/one.json"
	echo '{"exact":{"metadata":{"version":"2.0.0"}}}' >"$exact/two.json"
	run aube cache list
	assert_success
	assert_output $'@scope/pkg\nkeep'
	run aube cache view @scope/pkg
	assert_success
	assert_output --partial 'version:       1.0.0'
	assert_output --partial 'version:       2.0.0'
	run aube cache delete '@scope/*'
	assert_success
	[ ! -e "$exact/one.json" ]
	[ ! -e "$exact/two.json" ]
	[ ! -e "$cache_root/packuments-v1/origin-one/@scope__pkg.json" ]
	[ ! -e "$cache_root/packuments-full-v1/origin-two/@scope__pkg.json" ]
	run aube cache list
	assert_success
	assert_output 'keep'
	run aube cache delete '*'
	assert_success
	run aube cache list
	assert_success
	assert_output ''
}

@test "cache deletion does not follow symlinks or traverse unrelated directories" {
	cache="$(_cache_dir)"
	mkdir -p "$cache/origin-one" "$cache/unrelated" "$TEST_TEMP_DIR/external"
	echo '{}' >"$TEST_TEMP_DIR/external/keep.json"
	ln -s "$TEST_TEMP_DIR/external" "$cache/origin-linked"
	ln -s "$TEST_TEMP_DIR/external/keep.json" "$cache/origin-one/linked.json"
	echo '{}' >"$cache/unrelated/keep.json"
	echo '{}' >"$cache/origin-one/delete.json"
	run aube cache delete '*'
	assert_success
	[ ! -e "$cache/origin-one/delete.json" ]
	[ -f "$TEST_TEMP_DIR/external/keep.json" ]
	[ -L "$cache/origin-one/linked.json" ]
	[ -f "$cache/unrelated/keep.json" ]
}

@test "cache commands preserve scoped names containing double underscores" {
	_fake_cache_entry "@foo__bar/baz"
	exact="$HOME/.cache/aube/packuments-full-v1/exact-v1/origin-one/@foo%2Fbar__baz"
	mkdir -p "$exact"
	echo '{"exact":{"metadata":{"name":"@foo/bar__baz","version":"1.0.0"}}}' >"$exact/one.json"
	run aube cache list
	assert_success
	assert_line '@foo__bar/baz'
	assert_line '@foo/bar__baz'
	run aube cache delete '@foo/*'
	assert_success
	[ ! -e "$exact/one.json" ]
	run aube cache list
	assert_success
	assert_output '@foo__bar/baz'
	run aube cache delete '*'
	assert_success
}

@test "cache view skips corrupt exact entries and wildcard deletion clears them" {
	_fake_cache_entry "demo"
	exact="$HOME/.cache/aube/packuments-full-v1/exact-v1/origin-one/demo"
	ambiguous="$HOME/.cache/aube/packuments-full-v1/exact-v1/origin-one/@foo__bar__baz"
	mkdir -p "$exact" "$ambiguous"
	echo 'corrupt' >"$exact/one.json"
	echo 'corrupt' >"$ambiguous/one.json"
	run aube cache view demo
	assert_success
	assert_output --partial 'highest:       10.0.0'
	run aube cache delete '*'
	assert_success
	[ ! -e "$exact/one.json" ]
	[ ! -e "$ambiguous/one.json" ]
}
