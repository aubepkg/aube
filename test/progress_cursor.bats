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
_run_in_pty() {
	local out=$1
	shift
	cat >pty-cmd.sh <<-SH
		#!/usr/bin/env bash
		$*
	SH
	chmod +x pty-cmd.sh
	if [ "$(uname -s)" = "Darwin" ]; then
		script -q "$out" ./pty-cmd.sh >/dev/null 2>&1 || true
	else
		script -qec ./pty-cmd.sh /dev/null >"$out" 2>&1 || true
	fi
}

# Fails when the last cursor escape in the captured PTY stream is a hide
# (`ESC [ ? 25 l`) with no restore (`ESC [ ? 25 h`) after it.
_assert_cursor_restored() {
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
