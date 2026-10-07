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

@test "explicit frozen install rejects same-size lockfile corruption with preserved mtime" {
	cp -p aube-lock.yaml lockfile-timestamp
	node -e 'const fs = require("fs"); const p = "aube-lock.yaml"; fs.writeFileSync(p, Buffer.alloc(fs.statSync(p).size, 91));'
	touch -r lockfile-timestamp aube-lock.yaml
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
}

@test "explicit frozen install rejects same-size manifest drift with preserved mtime" {
	cp -p package.json manifest-timestamp
	node -e 'const fs = require("fs"); const p = "package.json"; const before = fs.readFileSync(p, "utf8"); const after = before.replace("3.0.1", "9.9.9"); if (before === after) throw new Error("fixture version not found"); fs.writeFileSync(p, after);'
	touch -r manifest-timestamp package.json
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
	assert_output --partial "lockfile is out of date"
}

@test "explicit frozen install rejects a new workspace member with current state" {
	printf '%s\n' 'packages:' '  - packages/*' >pnpm-workspace.yaml
	mkdir -p packages/first
	printf '%s\n' '{"name":"first","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}' >packages/first/package.json
	aube install --no-frozen-lockfile --ignore-scripts
	mkdir -p packages/second
	cp packages/first/package.json packages/second/package.json
	sed -i.bak 's/first/second/' packages/second/package.json
	rm packages/second/package.json.bak
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
}

@test "explicit frozen install rejects patch drift outside the default patches directory" {
	rm aube-lock.yaml
	cat >package.json <<'EOF'
{
  "name": "patch-hash-drift-test",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
patchedDependencies:
  is-odd@3.0.1: custom-patches/is-odd@3.0.1.patch
EOF
	mkdir custom-patches
	cat >custom-patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'
importers:
  .: {}
EOF

	run aube install --no-frozen-lockfile --ignore-scripts
	assert_success
	old_hash="$(awk '$1 == "is-odd@3.0.1:" && NF == 2 { print $2; exit }' pnpm-lock.yaml)"
	assert_equal "${#old_hash}" 64

	sed -i.bak "s/patched = 'v1'/patched = 'v2'/" custom-patches/is-odd@3.0.1.patch
	rm custom-patches/is-odd@3.0.1.patch.bak

	run aube install --frozen-lockfile --ignore-scripts
	assert_failure
	assert_output --partial "ERR_AUBE_LOCKFILE_CONFIG_MISMATCH"

}

@test "explicit frozen install still runs root lifecycle hooks when scripts are enabled" {
	cat >record-hook.cjs <<'EOF'
require('fs').appendFileSync('hooks.log', process.env.npm_lifecycle_event + '\n');
EOF
	node -e 'const fs = require("fs"); const p = JSON.parse(fs.readFileSync("package.json")); p.scripts = Object.fromEntries(["preinstall", "install", "postinstall", "prepare"].map(name => [name, "node record-hook.cjs"])); fs.writeFileSync("package.json", JSON.stringify(p));'
	aube install --no-frozen-lockfile
	rm hooks.log
	run aube install --frozen-lockfile --offline
	assert_success
	assert_file_exists hooks.log
	assert_equal "$(cat hooks.log)" "$(printf 'preinstall\ninstall\npostinstall\nprepare')"
}

@test "explicit frozen install preserves per-install advisory routing" {
	export AUBE_ADVISORY_CHECK=on
	export AUBE_ADVISORY_CHECK_EVERY_INSTALL=true
	aube install --ignore-scripts
	run env AUBE_DIAG_FILE="$TEST_TEMP_DIR/advisory.jsonl" aube install --frozen-lockfile --ignore-scripts
	assert_success
	run grep -F '"cat":"install_phase","name":"resolve"' "$TEST_TEMP_DIR/advisory.jsonl"
	assert_success
}

@test "explicit frozen install validates a removed local tarball" {
	mkdir -p archive/package
	echo '{"name":"local-archive","version":"1.0.0"}' >archive/package/package.json
	tar -czf archive.tgz -C archive package
	echo '{"name":"tarball-root","dependencies":{"local-archive":"file:./archive.tgz"}}' >package.json
	aube install --no-frozen-lockfile --ignore-scripts
	rm archive.tgz
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
}

@test "explicit frozen install validates a removed local peer tarball" {
	mkdir -p archive/package
	echo '{"name":"local-archive","version":"1.0.0"}' >archive/package/package.json
	tar -czf archive.tgz -C archive package
	echo '{"name":"tarball-root","peerDependencies":{"local-archive":"file:./archive.tgz"}}' >package.json
	aube install --no-frozen-lockfile --ignore-scripts
	rm archive.tgz
	run aube install --frozen-lockfile --offline --ignore-scripts
	assert_failure
}
