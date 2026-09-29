#!/usr/bin/env bats
#
# `--lockfile-dir` / `lockfileDir`: relocate aube-lock.yaml to a
# different directory than the project root, with the project recorded
# under the lockfile's `importers:` map keyed by its relative path
# (mirrors pnpm's `--lockfile-dir`). See [pnpm/test/install/misc.ts:112]
# for the canonical pnpm behavior this matches.

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install --lockfile-dir: writes lockfile to the named dir, not the project root" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-host",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	# Lockfile lives in the parent, not the project root.
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml

	# node_modules and the linker output stay with the project.
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube install --lockfile-dir: importer key is the project's relative path" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-importer-key",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	# Importer is keyed by `project` (the directory name), not `.`.
	# Match leading whitespace + `project:` to avoid matching the
	# package name embedded inside other lockfile fields.
	run grep -E "^[[:space:]]+project:" ../aube-lock.yaml
	assert_success
	run grep -E "^[[:space:]]+\.:" ../aube-lock.yaml
	assert_failure
}

@test "aube install --lockfile-dir: creates the target directory if missing" {
	# pnpm parity: `--lockfile-dir <missing-path>` materializes the
	# directory rather than aborting with a `canonicalize` ENOENT.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-mkdir",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir ../shared-locks/nested --no-frozen-lockfile
	assert_success

	assert_file_exists ../shared-locks/nested/aube-lock.yaml
	assert_file_not_exists aube-lock.yaml
}

@test "aube install --lockfile-dir: refuses to write a lockfile already used by another project" {
	# Multi-project shared lockfiles outside a workspace are out of
	# scope here: silently overwriting the first project's importer
	# entries (and orphan-stripping its packages) would be data-
	# destructive. Loud-fail with a message that points at workspaces
	# or per-project lockfile dirs.
	mkdir alpha beta
	cat >alpha/package.json <<'JSON'
{
  "name": "lfd-alpha",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	cat >beta/package.json <<'JSON'
{
  "name": "lfd-beta",
  "version": "1.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	(cd alpha && aube install --lockfile-dir .. --no-frozen-lockfile)
	assert_file_exists aube-lock.yaml

	cd beta || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_failure
	assert_output --partial "records importers from other projects"
	assert_output --partial "alpha"
}

@test "aube install --lockfile-dir: refuses to clobber a parent dir that is itself a project" {
	# `lockfile_dir` is itself someone's project root (importer `.`).
	# Pointing a subdir there with `--lockfile-dir ..` would silently
	# drop the parent's `.` entry on write — same data-loss class as
	# two unrelated peers. The foreign-importer guard must reject `.`
	# too, since by the time it runs the caller has already
	# established that the current project's importer key is non-`.`.
	cat >package.json <<'JSON'
{
  "name": "lfd-parent",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	mkdir child
	cat >child/package.json <<'JSON'
{
  "name": "lfd-child",
  "version": "1.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	aube install --no-frozen-lockfile
	assert_file_exists aube-lock.yaml

	cd child || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_failure
	assert_output --partial "records importers from other projects"
}

@test "aube install --lockfile-dir: warm install reads the relocated lockfile" {
	# Round-trip: write once, wipe node_modules, install again. The
	# second install must read the relocated lockfile (not regenerate)
	# and the `importers:` block must still use `project`, not `.`.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-roundtrip",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	rm -rf node_modules

	run aube install --lockfile-dir .. --frozen-lockfile
	assert_success
	assert_file_exists node_modules/is-odd/index.js

	run grep -E "^[[:space:]]+project:" ../aube-lock.yaml
	assert_success
}

@test "aube install --lockfile-dir: local deps survive a reinstall from a lockfile below the project" {
	# pnpm records `file:` directories and tarballs relative to the
	# lockfile directory and `link:` targets relative to the project.
	# Writing project-relative `file:` paths, or reading them back
	# against the wrong directory, made every later install look for
	# `<project>/../vendor/...` and fail (or link to it, for `link:`).
	mkdir -p project/vendor/dir-dep project/vendor/link-dep project/tgz/package
	echo '{"name":"dir-dep","version":"2.0.0"}' >project/vendor/dir-dep/package.json
	echo '{"name":"link-dep","version":"3.0.0"}' >project/vendor/link-dep/package.json
	echo '{"name":"tgz-dep","version":"4.0.0"}' >project/tgz/package/package.json
	(cd project/tgz && tar czf ../tgz-dep.tgz package)
	cat >project/package.json <<'JSON'
{
  "name": "lfd-local-deps",
  "version": "1.0.0",
  "dependencies": {
    "dir-dep": "file:./vendor/dir-dep",
    "link-dep": "link:./vendor/link-dep",
    "tgz-dep": "file:./tgz-dep.tgz"
  }
}
JSON

	cd project || return
	run aube install --lockfile-dir .lock --no-frozen-lockfile
	assert_success

	run cat .lock/aube-lock.yaml
	assert_output --partial "directory: ../vendor/dir-dep"
	assert_output --partial "tarball: file:../tgz-dep.tgz"
	assert_output --partial "version: link:vendor/link-dep"

	for flags in --no-frozen-lockfile --frozen-lockfile; do
		rm -rf node_modules
		run aube install --lockfile-dir .lock "$flags"
		assert_success
		run cat node_modules/dir-dep/package.json node_modules/link-dep/package.json \
			node_modules/tgz-dep/package.json
		assert_output --partial '"version":"2.0.0"'
		assert_output --partial '"version":"3.0.0"'
		assert_output --partial '"version":"4.0.0"'
	done
}
