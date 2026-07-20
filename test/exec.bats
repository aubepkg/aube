#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube exec --parallel fans out across workspace packages" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF

	run aube install
	assert_success

	run aube -r exec --parallel --shell-mode node -- -e 'console.log(require("is-odd")(3) ? process.cwd().split("/").pop() : "no")'
	assert_success
	assert_output --partial "a"
	assert_output --partial "b"
}

@test "aube exec --parallel preflights missing binaries before spawning" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a/node_modules/.bin packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0"}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0"}
EOF
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube install
	assert_success
	mkdir -p packages/a/node_modules/.bin
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube -r exec --parallel sentinel
	assert_failure
	assert_output --partial "binary not found in b: sentinel"
	assert_file_not_exist packages/sentinel-ran
}

# Regression: terminating the aube process must tear down the tool it
# spawned instead of orphaning it under init. See discussion #1059.
# Parameterized over the signal: SIGTERM exercises the forwarding path,
# SIGKILL exercises Linux PR_SET_PDEATHSIG (skipped elsewhere — macOS
# SIGKILL-of-parent teardown is a separate follow-up).
_assert_child_torn_down_on() {
	local sig="$1" dur="$2"
	cat >package.json <<'EOF'
{"name":"t","version":"0.0.0"}
EOF
	mkdir -p node_modules/.bin
	printf '#!/bin/sh\nexec sleep %s\n' "$dur" >node_modules/.bin/sleeper
	chmod +x node_modules/.bin/sleeper

	aube exec --no-install sleeper &
	local aube_pid=$!

	local child=""
	for _ in $(seq 1 50); do
		child="$(pgrep -f "sleep ${dur}\$" || true)"
		[ -n "$child" ] && break
		sleep 0.1
	done
	[ -n "$child" ] || {
		kill "$aube_pid" 2>/dev/null || true
		fail "spawned tool never started"
	}

	kill "-${sig}" "$aube_pid" 2>/dev/null || true

	local gone=""
	for _ in $(seq 1 30); do
		pgrep -f "sleep ${dur}\$" >/dev/null || {
			gone=1
			break
		}
		sleep 0.1
	done
	pkill -f "sleep ${dur}\$" 2>/dev/null || true
	[ -n "$gone" ] || fail "tool orphaned after ${sig} to aube"
}

@test "aube exec tears down the spawned tool on SIGTERM" {
	[ "$(uname -s)" != "Windows_NT" ] || skip "signals are POSIX"
	_assert_child_torn_down_on TERM 987651
}

@test "aube exec tears down the spawned tool on SIGKILL (Linux PDEATHSIG)" {
	[ "$(uname -s)" = "Linux" ] || skip "PR_SET_PDEATHSIG is Linux-only"
	_assert_child_torn_down_on KILL 987652
}
