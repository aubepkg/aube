#!/usr/bin/env bats
#
# Multicall shims: `aubr` dispatches as `aube run …` and `aubx` as
# `aube dlx …` purely via argv[0] basename detection in `main.rs`.
# The shims are hardlinks to the same `aube` executable; the BATS
# common_setup refreshes them at the start of every test.
#
# `aubx` needs the fixture registry for package fetches, so keep these
# tests out of the parallel pool like `dlx.bats` does.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aubr runs a package.json script" {
	_setup_basic_fixture
	aube install
	run aubr hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "aubr with no script errors like 'aube run' with no script" {
	_setup_basic_fixture
	aube install
	run aubr </dev/null
	assert_failure
	assert_output --partial "script name required"
}

@test "aubx runs a package binary" {
	run aubx semver 1.2.3
	assert_success
	assert_line "1.2.3"
}

@test "aubx forwards flags through to dlx" {
	# `-p which node-which node` is the same -p smoke the dlx suite uses:
	# install the `which` package, run its `node-which` binary, and
	# expect the resolved `node` path in the output.
	run aubx --package which node-which node
	assert_success
	assert_output --partial "/node"
}

@test "aube exec and run <bin> keep the activated tool shim dir on PATH" {
	_skip_on_windows "uses a POSIX shell bin"
	_setup_basic_fixture
	aube install
	mkdir -p shims node_modules/.bin
	printf '#!/bin/sh\n' >shims/npm
	chmod +x shims/npm
	printf '#!/bin/sh\ncommand -v npm\n' >node_modules/.bin/whichnpm
	chmod +x node_modules/.bin/whichnpm
	export AUBE_SHIM_DIR="$PWD/shims"
	run aube exec whichnpm
	assert_success
	assert_output "$PWD/shims/npm"
	run aube run whichnpm
	assert_success
	assert_output "$PWD/shims/npm"
}
