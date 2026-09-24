#!/usr/bin/env bats

# The hidden lockfile (`node_modules/.aube-lock.yaml`) mirrors the
# install's lockfile graph, like pnpm's `node_modules/.pnpm/lock.yaml`.
# When the project lockfile is gone, install seeds from it instead of
# re-resolving from the registry.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Install is-number pinned to 6.0.0, then widen the range to `>=6.0.0`
# in both package.json and the hidden lockfile. The registry also has
# 7.0.0, so a fresh resolve picks 7.0.0 while a hidden-lockfile seed
# keeps 6.0.0. This stands in for "the lockfile was written before a
# newer version was published".
_install_pinned_then_widen() {
	cat >package.json <<-'EOF'
		{
		  "name": "test-hidden-lockfile",
		  "version": "1.0.0",
		  "dependencies": { "is-number": "6.0.0" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists node_modules/.aube-lock.yaml
	sed -i.bak 's/"6.0.0"/">=6.0.0"/' package.json
	sed -i.bak "s/specifier: 6.0.0/specifier: '>=6.0.0'/" node_modules/.aube-lock.yaml
	rm -f package.json.bak node_modules/.aube-lock.yaml.bak
	run grep -F "specifier: '>=6.0.0'" node_modules/.aube-lock.yaml
	assert_success
}

@test "install writes the hidden lockfile next to the install state" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-hidden-lockfile",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists node_modules/.aube-lock.yaml
	run cmp aube-lock.yaml node_modules/.aube-lock.yaml
	assert_success
}

@test "missing lockfile is restored from the hidden lockfile with locked versions" {
	_install_pinned_then_widen
	rm aube-lock.yaml
	AUBE_LOG=debug run aube install
	assert_success
	assert_output --partial "seeding install from hidden lockfile"
	assert_output --partial "phase:resolve (from lockfile)"
	run grep -F "is-number@6.0.0" aube-lock.yaml
	assert_success
	run grep -F "is-number@7.0.0" aube-lock.yaml
	assert_failure
	run jq -r .version node_modules/is-number/package.json
	assert_output "6.0.0"
	# The restored lockfile makes the next install a no-op.
	run aube install
	assert_success
	assert_output --partial "Already up to date"
}

@test "without the hidden lockfile a missing lockfile re-resolves to the newest version" {
	_install_pinned_then_widen
	rm aube-lock.yaml node_modules/.aube-lock.yaml
	run aube install
	assert_success
	run grep -F "is-number@7.0.0" aube-lock.yaml
	assert_success
}

@test "hidden lockfile seed still re-resolves manifest drift" {
	_install_pinned_then_widen
	rm aube-lock.yaml
	# New dependency: not in the hidden lockfile, so it has to resolve,
	# while the unchanged is-number spec keeps its locked version.
	jq '.dependencies["is-odd"] = "3.0.1"' package.json >package.json.tmp
	mv package.json.tmp package.json
	run aube install
	assert_success
	run grep -F "is-odd@3.0.1" aube-lock.yaml
	assert_success
	run grep -F "is-number@6.0.0" aube-lock.yaml
	assert_success
	assert_dir_exists node_modules/is-odd
	run grep -F "is-odd@3.0.1" node_modules/.aube-lock.yaml
	assert_success
}

@test "auto-CI frozen default seeds from the hidden lockfile" {
	_install_pinned_then_widen
	rm aube-lock.yaml
	CI=true run aube install
	assert_success
	run grep -F "is-number@6.0.0" aube-lock.yaml
	assert_success
}

@test "--frozen-lockfile still fails without a project lockfile" {
	_install_pinned_then_widen
	rm aube-lock.yaml
	run aube install --frozen-lockfile
	assert_failure
	assert_output --partial "no lockfile found"
	run test -e aube-lock.yaml
	assert_failure
}

@test "--no-frozen-lockfile ignores the hidden lockfile" {
	_install_pinned_then_widen
	rm aube-lock.yaml
	run aube install --no-frozen-lockfile
	assert_success
	run grep -F "is-number@7.0.0" aube-lock.yaml
	assert_success
}

@test "corrupt hidden lockfile warns and falls back to a resolve" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-hidden-lockfile",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	rm aube-lock.yaml
	printf 'lockfileVersion: [\n  : not yaml\n' >node_modules/.aube-lock.yaml
	run aube install
	assert_success
	assert_output --partial "WARN_AUBE_HIDDEN_LOCKFILE_BROKEN"
	assert_file_exists aube-lock.yaml
	run cmp aube-lock.yaml node_modules/.aube-lock.yaml
	assert_success
}

@test "hoisted node-linker keeps a hidden lockfile too" {
	printf '\nnode-linker=hoisted\n' >>.npmrc
	_install_pinned_then_widen
	rm aube-lock.yaml
	run aube install
	assert_success
	run grep -F "is-number@6.0.0" aube-lock.yaml
	assert_success
}

@test "lockfile=false removes the hidden lockfile" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-hidden-lockfile",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists node_modules/.aube-lock.yaml
	printf '\nlockfile=false\n' >>.npmrc
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_not_exists node_modules/.aube-lock.yaml
}

@test "workspace: hidden lockfile covers every importer and picks up new members" {
	cat >package.json <<-'EOF'
		{ "name": "root", "version": "1.0.0", "private": true }
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - 'packages/*'
	EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<-'EOF'
		{ "name": "a", "version": "1.0.0", "dependencies": { "is-number": "6.0.0" } }
	EOF
	run aube install
	assert_success
	run grep -F "packages/a:" node_modules/.aube-lock.yaml
	assert_success
	sed -i.bak 's/"6.0.0"/">=6.0.0"/' packages/a/package.json
	sed -i.bak "s/specifier: 6.0.0/specifier: '>=6.0.0'/" node_modules/.aube-lock.yaml
	rm -f packages/a/package.json.bak node_modules/.aube-lock.yaml.bak
	# A member added after the hidden lockfile was written.
	cat >packages/b/package.json <<-'EOF'
		{ "name": "b", "version": "1.0.0", "dependencies": { "is-odd": "3.0.1" } }
	EOF
	rm aube-lock.yaml
	run aube install
	assert_success
	run grep -F "is-number@6.0.0" aube-lock.yaml
	assert_success
	run grep -F "packages/b:" aube-lock.yaml
	assert_success
	assert_dir_exists packages/b/node_modules/is-odd
}
