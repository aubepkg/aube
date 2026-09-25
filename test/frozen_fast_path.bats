#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	_setup_basic_fixture
	aube install --ignore-scripts
}

teardown() {
	_common_teardown
}

@test "explicit frozen install reuses current state without resolving" {
	run env AUBE_DIAG_FILE="$TEST_TEMP_DIR/frozen.jsonl" aube install --frozen-lockfile --offline --ignore-scripts
	assert_success
	run grep -F '"cat":"frozen","name":"check_needs_install"' "$TEST_TEMP_DIR/frozen.jsonl"
	assert_success
	run grep -F '"cat":"install_phase","name":"resolve"' "$TEST_TEMP_DIR/frozen.jsonl"
	assert_failure
}

@test "explicit frozen install still rejects a missing root lockfile with current state" {
	rm aube-lock.yaml
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
	assert_output --partial "no lockfile found"
}

@test "explicit frozen install still rejects disabled lockfiles with current state" {
	printf '%s\n' 'lockfile=false' >>.npmrc
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
	assert_output --partial "incompatible with lockfile=false"
}

@test "explicit frozen install still rejects manifest drift with current state" {
	printf '%s\n' '{"name":"frozen-fast-path","version":"1.0.0","dependencies":{"is-odd":"^99.0.0"}}' >package.json
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
	assert_output --partial "lockfile is out of date"
}

@test "explicit frozen install restores a missing dependency link" {
	rm node_modules/is-odd
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_success
	assert_link_exists node_modules/is-odd
}
