#!/usr/bin/env bats

# `aube doctor` dumps a grouped snapshot of aube's environment and
# project layout, then reports any warnings and errors it detected
# statically. Exits non-zero when the error list is non-empty.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube doctor prints the core sections outside of any project" {
	# Ensure there is no package.json at or above HOME.
	run aube doctor
	assert_success
	assert_output --partial "version:"
	assert_output --partial "dirs:"
	assert_output --partial "registry:"
}

@test "aube doctor surfaces the detected lockfile and package name" {
	cat >package.json <<'JSON'
{
  "name": "doctor-demo",
  "version": "0.1.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	run aube doctor
	assert_success
	assert_output --partial "doctor-demo@0.1.0"
	assert_output --partial "aube-lock.yaml"
	assert_output --partial "No problems found"
}

@test "aube doctor reports broken sibling links as errors and exits 1" {
	cat >package.json <<'JSON'
{
  "name": "doctor-broken",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	rm node_modules/.aube/is-odd@3.0.1/node_modules/is-number
	run aube doctor
	assert_failure
	assert_output --partial "broken dependency link"
	assert_output --partial "aube check"
}

@test "aube doctor --json emits sections, warnings, and errors as JSON" {
	cat >package.json <<'JSON'
{
  "name": "doctor-json",
  "version": "0.0.0"
}

JSON
	run aube doctor --json
	assert_success
	assert_output --partial '"sections"'
	assert_output --partial '"warnings"'
	assert_output --partial '"errors"'
}

@test "doctor rejects unreadable top-level and per-registry CA files" {
	printf 'cafile=%s/missing-root.pem\n//registry.example.test/:cafile=%s/missing-scoped.pem\n' "$PWD" "$PWD" >.npmrc
	run aube doctor --json
	assert_failure 1
	assert_output --partial 'ERR_AUBE_INVALID_CAFILE'
	assert_output --partial 'missing-root.pem'
	assert_output --partial 'missing-scoped.pem'
}

@test "doctor rejects empty CA bundles and NODE_EXTRA_CA_CERTS" {
	touch empty.pem
	printf 'cafile=%s/empty.pem\n' "$PWD" >.npmrc
	run env NODE_EXTRA_CA_CERTS="$PWD/missing-extra.pem" aube doctor --json
	assert_failure 1
	assert_output --partial 'ERR_AUBE_INVALID_CAFILE'
	assert_output --partial 'contains no PEM certificates'
	assert_output --partial 'missing-extra.pem'
}

@test "doctor accepts a readable CA bundle without claiming connectivity" {
	printf 'cafile=%s/crates/aube-registry/tests/fixtures/test-ca.pem\n' "$PROJECT_ROOT" >.npmrc
	run aube doctor --json
	assert_success
	assert_output --partial 'not tested (local checks only)'
	refute_output --partial 'ERR_AUBE_INVALID_CAFILE'
}

@test "doctor reports default registry auth despite an unrelated malformed CA" {
	printf '%s\n' '-----BEGIN CERTIFICATE-----' 'AAAA' '-----END CERTIFICATE-----' >bad-ca.pem
	printf 'registry=https://registry.example.test/\n//registry.example.test/:_authToken=test-doctor-token\n//unrelated.example.test/:cafile=%s/bad-ca.pem\n' "$PWD" >.npmrc
	run aube doctor --json
	assert_failure 1
	assert_output --partial 'ERR_AUBE_INVALID_CAFILE'
	assert_output --partial '"auth": "configured"'
	refute_output --partial 'test-doctor-token'
}

@test "doctor resolves a trusted token helper independently of CA failures" {
	cat >token-helper <<'SH'
#!/bin/sh
echo test-helper-token
SH
	chmod +x token-helper
	printf 'registry=https://registry.example.test/\n//registry.example.test/:tokenHelper=%s/token-helper\ncafile=%s/missing.pem\n' "$PWD" "$PWD" >"$HOME/.npmrc"
	run aube doctor --json
	assert_failure 1
	assert_output --partial '"auth": "configured"'
	refute_output --partial 'test-helper-token'
	printf '#!/bin/sh\nexit 1\n' >token-helper
	run aube doctor --json
	assert_failure 1
	assert_output --partial '"auth": "(none)"'
}
