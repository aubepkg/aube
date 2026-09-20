#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# The animated install bar hides the cursor on every frame it paints, so a
# command that exits without retiring the renderer hands the shell back a
# prompt with an invisible cursor. Asserting that needs a real PTY: on a
# pipe aube picks the append-only reporter, which never touches the cursor.
#
# `script` is the portable PTY wrapper, but the two implementations order
# their arguments differently — util-linux takes `-c <command> <typescript>`,
# BSD/macOS takes `<typescript> <command>` — so both run a command file.
_run_script_in_pty() {
	local out=$1 cmd=$2
	chmod +x "$cmd"
	# Flush per write (`-f` / `-t 0`) so a test that reacts to what the PTY has
	# printed so far isn't reading a stale buffer — macOS `script` otherwise
	# flushes on a 30s timer.
	if [ "$(uname -s)" = "Darwin" ]; then
		script -q -t 0 "$out" "$cmd" >/dev/null 2>&1 || true
	else
		script -qfec "$cmd" /dev/null >"$out" 2>&1 || true
	fi
}

_run_in_pty() {
	local out=$1
	shift
	cat >pty-cmd.sh <<-SH
		#!/usr/bin/env bash
		$*
	SH
	_run_script_in_pty "$out" ./pty-cmd.sh
}

# Fails when the last cursor escape in the captured PTY stream is a hide
# (`ESC [ ? 25 l`) with no restore (`ESC [ ? 25 h`) after it.
_assert_cursor_restored() {
	# shellcheck disable=SC2016 # the JS template literal is for node, not bash
	node -e '
		const fs = require("fs");
		const out = fs.readFileSync(process.argv[1], "latin1");
		const hide = out.lastIndexOf("\x1b[?25l");
		const show = out.lastIndexOf("\x1b[?25h");
		if (show === -1 || hide > show) {
			console.error(`cursor left hidden: last hide ${hide}, last show ${show}`);
			process.exit(1);
		}
	' "$1"
}

# Fails when the stream ends with the OSC 9;4 taskbar indicator still set to a
# live state, i.e. the last sequence isn't `ESC ] 9 ; 4 ; 0 ; 0` (clear). The
# indicator is what keeps an iTerm2/Ghostty/VS Code tab spinning after aube is
# gone.
_assert_osc_progress_cleared() {
	# shellcheck disable=SC2016 # the JS template literal is for node, not bash
	node -e '
		const fs = require("fs");
		const out = fs.readFileSync(process.argv[1], "latin1");
		const seqs = out.match(/\x1b\]9;4;\d+;\d+/g);
		if (!seqs) {
			console.error("no OSC 9;4 sequences: the indicator was never driven");
			process.exit(1);
		}
		const last = seqs[seqs.length - 1];
		if (!last.endsWith(";0;0")) {
			console.error(`terminal progress left active: last sequence ${JSON.stringify(last)}`);
			process.exit(1);
		}
	' "$1"
}

# aube renders the append-only reporter instead of the animated bar when it
# detects CI, and GitHub Actions / Buildkite set these even though the bats
# shard runs on a PTY here. `CI` itself is already unset by _common_setup.
# `TERM_PROGRAM` opts the run into the OSC 9;4 taskbar indicator, which clx
# only drives for terminals known to support it.
_aube_in_pty() {
	_run_in_pty "$1" env -u CI -u CI_NAME -u GITHUB_ACTION -u GITLAB_CI -u BUILDKITE \
		TERM_PROGRAM=iTerm.app aube "${@:2}"
}

@test "an install that fails during resolution restores the cursor" {
	cat >package.json <<-'JSON'
		{
		  "name": "cursor-probe",
		  "version": "1.0.0",
		  "dependencies": {
		    "aube-no-such-package-1557": "^1.0.0"
		  }
		}
	JSON

	_aube_in_pty failed.pty install

	# The diagnostic has to survive the teardown: clearing the bar moves the
	# cursor up over the rendered rows, so a teardown sequenced after the
	# report would erase part of it.
	assert grep -q "aube-no-such-package-1557" failed.pty
	_assert_cursor_restored failed.pty
	_assert_osc_progress_cleared failed.pty
}

@test "a successful install restores the cursor" {
	cat >package.json <<-'JSON'
		{
		  "name": "cursor-probe",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON

	_aube_in_pty ok.pty install

	assert_dir_exist node_modules/is-odd
	_assert_cursor_restored ok.pty
	_assert_osc_progress_cleared ok.pty
}

@test "a termination signal during an install restores the cursor" {
	cat >package.json <<-'JSON'
		{
		  "name": "cursor-probe",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON

	# Hold resolution open so the signal lands while the bar owns the
	# terminal, instead of racing a local-registry install that finishes in
	# milliseconds. `readPackage` runs inside resolve, and `Atomics.wait`
	# blocks the hook host without spawning anything. The marker it drops
	# first is what the signal is timed off: reading it back is a plain file
	# check, where scraping the PTY capture would depend on how promptly each
	# platform's `script` flushes.
	cat >.pnpmfile.cjs <<-EOF
		const fs = require("fs");
		function readPackage(pkg) {
		  if (!globalThis.__aubeStalled) {
		    globalThis.__aubeStalled = true;
		    fs.writeFileSync("$PWD/resolving.marker", "1");
		    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10000);
		  }
		  return pkg;
		}
		module.exports = { hooks: { readPackage } };
	EOF

	# `set -m` puts the backgrounded install in its own process group, so the
	# shell doesn't hand it SIGINT already ignored — that reproduces the
	# disposition a Ctrl-C at an interactive prompt arrives with, rather than
	# the `nohup`-style one aube deliberately leaves alone.
	cat >pty-cmd.sh <<-SH
		#!/usr/bin/env bash
		set -m
		env -u CI -u CI_NAME -u GITHUB_ACTION -u GITLAB_CI -u BUILDKITE \\
			TERM_PROGRAM=iTerm.app aube install &
		pid=\$!
		(
			for _ in \$(seq 1 600); do
				[ -f "$PWD/resolving.marker" ] && break
				sleep 0.05
			done
			kill -INT "\$pid"
		) &
		wait "\$pid"
		echo "AUBE_EXIT=\$?"
	SH
	_run_script_in_pty interrupted.pty ./pty-cmd.sh

	# The marker proves the hook actually held resolution open, so a missing
	# 130 below means the signal path failed rather than the timing.
	assert_file_exist resolving.marker

	# 130 is SIGINT death (128 + 2): the handler restores the terminal and then
	# re-raises with the default disposition, so the shell still sees a signal
	# death rather than a plain exit. A 0 here would mean the install outran
	# the signal and the test proved nothing.
	assert grep -q "AUBE_EXIT=130" interrupted.pty
	_assert_cursor_restored interrupted.pty
	_assert_osc_progress_cleared interrupted.pty
}
