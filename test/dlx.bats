#!/usr/bin/env bats
#
# dlx installs and immediately executes transient bins. Keep these tests out of
# the parallel pool so setup/network failures do not surface as BATS BW01
# command-not-found warnings.
#
# bats file_tags=serial

# Force within-file tests to run one at a time regardless of --jobs.
# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube dlx runs a package binary" {
	# `semver <version>` prints the version back if it parses. Cheap smoke
	# test that proves the bin actually ran, not just that clap swallowed
	# a `--help` flag before it reached the binary. Use --partial because
	# aube's install pipeline interleaves progress lines on stderr, which
	# bats merges into `output`.
	run aube dlx semver 1.2.3
	assert_success
	assert_line "1.2.3"
}

@test "aube dlx prefers an installed local binary" {
	mkdir -p tools/local-bin
	cat >package.json <<-'JSON'
		{
		  "name": "dlx-local-bin",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": {
		    "local-bin": "file:tools/local-bin"
		  }
		}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(`local-dlx:${process.argv.slice(2).join(",")}`)
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube dlx local-bin alpha beta
	assert_success
	assert_line "local-dlx:alpha,beta"
}

@test "aube dlx launches a native bin from a scratch install" {
	case "$(uname -s)" in
	Darwin | Linux) ;;
	*) skip "native fixture requires a Unix executable" ;;
	esac
	mkdir -p tools/native-bin
	cat >tools/native-bin/package.json <<-'JSON'
		{
		  "name": "native-bin",
		  "version": "1.0.0",
		  "bin": {
		    "native-aube": "native-aube"
		  }
		}
	JSON
	cp "$PROJECT_ROOT/target/debug/aube" tools/native-bin/native-aube
	chmod +x tools/native-bin/native-aube

	run aube dlx --package "file:$PWD/tools/native-bin" native-aube --version
	assert_success
	assert_output --regexp '\([0-9]{4}-[0-9]{2}-[0-9]{2}\)$'
}

@test "aube dlx -p installs a different package than the bin name" {
	# The `which` npm package ships a binary named `node-which`, not `which`.
	# Running `node-which node` prints the absolute path of the `node`
	# executable on PATH, so we assert the output contains `/node`.
	run aube dlx --package which node-which node
	assert_success
	assert_output --partial "/node"
}

@test "aube dlx falls back to the package's single bin when names differ" {
	# `which` ships its bin as `node-which`, so the naive
	# "bin name == unscoped package name" inference would look for
	# `.bin/which` and fail. With the installed-package fallback aube
	# should pick `node-which` (the only bin) and run it — exactly
	# what `npx which node` does.
	run aube dlx which node
	assert_success
	assert_output --partial "/node"
}

@test "aube dlx accepts an @version suffix on the command" {
	# semver@7.7.4 is what the fixture set has pinned. Run it against a
	# prerelease version so the output is distinguishable from the default.
	run aube dlx semver@7.7.4 1.2.3-alpha.1
	assert_success
	assert_line "1.2.3-alpha.1"
}

@test "aube dlx version spec bypasses local binary shortcut" {
	mkdir -p tools/semver
	cat >package.json <<-'JSON'
		{
		  "name": "dlx-versioned-local-bin",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": {
		    "semver": "file:tools/semver"
		  }
		}
	JSON
	cat >tools/semver/package.json <<-'JSON'
		{
		  "name": "semver",
		  "version": "0.0.0",
		  "bin": {
		    "semver": "index.js"
		  }
		}
	JSON
	cat >tools/semver/index.js <<-'JS'
		#!/usr/bin/env node
		console.log("local-semver")
	JS
	chmod +x tools/semver/index.js

	aube install
	run aube dlx semver@7.7.4 1.2.3-alpha.1
	assert_success
	assert_line "1.2.3-alpha.1"
	refute_output --partial "local-semver"
}

@test "aube dlx --shell-mode runs the joined line through sh -c" {
	# `semver 1.2.3` would print "1.2.3"; piping through tr proves the
	# command actually ran inside a shell instead of being exec'd as a
	# single argv. We pass `-p semver` because in shell-mode the first
	# positional is a shell line, not a bin name.
	run aube dlx --shell-mode -p semver 'semver 1.2.3 | tr 0-9 a-z'
	assert_success
	assert_line "b.c.d"
}

@test "aube dlx --shell-mode forwards a termination signal to the command" {
	# `process_guard` forwards SIGTERM/SIGINT/SIGHUP/SIGQUIT to the child it
	# spawned so that signalling aube tears the running tool down. A `sh -c`
	# wrapper swallowed that hop: `sh` stays resident as the command's parent
	# and a non-interactive shell does not relay a signal to the child it is
	# waiting on, so the tool ran on and aube never exited. A line that is one
	# plain command now skips the shell entirely, which puts the tool back
	# where the forwarding expects it — as aube's direct child.
	cat >dlx-signal-child.js <<-'JS'
		process.on("SIGINT", () => {
			console.log("CHILD GOT SIGINT")
			process.exit(7)
		})
		console.log("CHILD READY")
		setInterval(() => {}, 1000)
	JS

	# `set -m` puts the backgrounded dlx in its own process group, so the
	# `kill -INT` reaches aube alone rather than this shell's whole group,
	# and aube inherits SIGINT with the default disposition instead of the
	# already-ignored one a background job would otherwise be handed.
	cat >interrupt.sh <<-'SH'
		#!/usr/bin/env bash
		set -m
		aube dlx -p is-odd -c "node dlx-signal-child.js" >child.out 2>&1 </dev/null &
		pid=$!
		for _ in $(seq 1 600); do
			grep -aq "CHILD READY" child.out 2>/dev/null && break
			sleep 0.1
		done
		kill -INT "$pid"
		# Bounded wait, then reap. Unfixed, nothing reaches the child and both
		# it and aube run forever while holding the fds bats captures output
		# on — so tearing the tree down here is what turns the regression into
		# a failed assertion instead of a wedged suite.
		for _ in $(seq 1 100); do
			kill -0 "$pid" 2>/dev/null || break
			sleep 0.1
		done
		if kill -0 "$pid" 2>/dev/null; then
			kill -KILL "$pid" 2>/dev/null
			pkill -f dlx-signal-child.js 2>/dev/null
			echo "AUBE_EXIT=hung"
		else
			wait "$pid"
			echo "AUBE_EXIT=$?"
		fi
	SH
	chmod +x interrupt.sh

	# `timeout(1)` is GNU coreutils — Linux ships it as `timeout`, macOS
	# only as `gtimeout` after `brew install coreutils`, and not at all on
	# a stock host. It is a backstop here rather than the mechanism:
	# `interrupt.sh` bounds its own wait and reaps the tree either way, so
	# running it bare when neither is on PATH still terminates.
	local timeout_cmd=""
	if command -v timeout >/dev/null 2>&1; then
		timeout_cmd="timeout 120"
	elif command -v gtimeout >/dev/null 2>&1; then
		timeout_cmd="gtimeout 120"
	fi
	# shellcheck disable=SC2086 # intentional word-split: empty -> no wrapper
	run $timeout_cmd ./interrupt.sh
	assert_success
	# The child picked its own exit code inside its handler and aube reported
	# it, which proves aube stayed bound to the tool rather than exiting out
	# from under it.
	assert_line "AUBE_EXIT=7"
	assert_file_contains child.out "CHILD GOT SIGINT"
}

@test "aube dlx -c infers the package from the first word when -p is omitted" {
	# Without -p the first whitespace-separated word is taken as the
	# install spec — same convention as plain `aube dlx <cmd>`.
	run aube dlx -c 'semver 7.0.0'
	assert_success
	assert_line "7.0.0"
}

@test "aube dlx accepts the global gvs override after the subcommand" {
	run aube dlx --enable-gvs semver 1.2.3
	assert_success
	assert_line "1.2.3"
}
