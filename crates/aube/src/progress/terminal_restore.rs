//! Putting the terminal back when aube dies without unwinding.
//!
//! The animated progress display hides the cursor on every frame and drives
//! the OSC 9;4 taskbar indicator as it goes; both are undone by the renderer's
//! teardown. Two exits never reach that teardown:
//!
//! * **A termination signal** — Ctrl-C during an install kills the process
//!   outright, so nothing in aube runs.
//! * **A panic** — the release profile is `panic = "abort"`, so no destructor
//!   runs. The panic *hook* still does, which is what
//!   [`restore_now`] is for.
//!
//! Both paths write the same two sequences straight to stderr rather than
//! going through clx: a signal handler may only call async-signal-safe
//! functions, and `write(2)` is one while clx's lock-taking teardown is not.
//! Writing them twice (once here, once from a teardown that does run) is
//! harmless — showing a visible cursor and clearing a cleared indicator are
//! both no-ops.
//!
//! The signal half only takes over a signal still at `SIG_DFL`, so it is the
//! standalone binary that benefits: an embedding host managing its own signals
//! keeps them untouched, and the terminal restore on those paths is its own to
//! make. See `may_take_over`.

use std::io::IsTerminal;
use std::sync::atomic::AtomicBool;

/// DEC private mode 25, set: make the cursor visible again.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// OSC 9;4 in state 0 (none): clear the taskbar progress indicator. Byte-for
/// byte what clx emits when it retires a job, ST terminator included.
const CLEAR_OSC_PROGRESS: &[u8] = b"\x1b]9;4;0;0\x1b\\";

/// Whether a restore should also clear the taskbar indicator. Resolved once,
/// when the bar starts, because neither caller can afford to look it up:
/// `getenv` is not async-signal-safe, and `std::env` takes a lock that a panic
/// hook must not wait on. `false` until then, which is also the right answer
/// for a panic with no bar on screen — there is no indicator to clear.
static CLEAR_OSC_INDICATOR: AtomicBool = AtomicBool::new(false);

/// Whether the terminal understands OSC 9;4, mirroring clx's own detection
/// (`clx::osc::terminal_supports_osc_9_4`, which is private). aube only emits
/// the clear for terminals clx would have set the indicator on, so a terminal
/// that treats the sequence as text can't be made to print it.
fn osc_progress_supported() -> bool {
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        match term_program.as_str() {
            "ghostty" | "vscode" | "iTerm.app" => return true,
            "WezTerm" | "Alacritty" => return false,
            _ => {}
        }
    }
    std::env::var_os("WT_SESSION").is_some() || std::env::var_os("VTE_VERSION").is_some()
}

/// Write `bytes` to stderr without taking a lock.
///
/// A panic hook can be entered while another thread holds the lock behind
/// `std::io::stderr()`, and under `panic = "abort"` waiting on it would hang
/// the process instead of aborting it. A signal handler has the stricter
/// version of the same problem: it may only call async-signal-safe functions.
/// `write(2)` answers both.
#[cfg(unix)]
fn write_stderr(bytes: &[u8]) {
    // SAFETY: `bytes` is a const slice that outlives the process, and `write`
    // is async-signal-safe.
    unsafe {
        libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len());
    }
}

/// Windows has no `write(2)`; the panic hook is the only caller there — the
/// signal half is Unix-only — so the locking handle is acceptable.
#[cfg(not(unix))]
fn write_stderr(bytes: &[u8]) {
    use std::io::Write;

    let _ = std::io::stderr().write_all(bytes);
}

/// Restore the terminal from ordinary (non-signal) context: the panic hook.
///
/// Gated on an interactive stderr so a piped or redirected stream never gains
/// an escape sequence. Takes no locks and reads no environment, so it stays
/// safe when the panic came from inside clx holding its terminal lock, or
/// from a thread holding stderr's.
pub(crate) fn restore_now() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    write_stderr(SHOW_CURSOR);
    if CLEAR_OSC_INDICATOR.load(std::sync::atomic::Ordering::Relaxed) {
        write_stderr(CLEAR_OSC_PROGRESS);
    }
}

/// Take over the terminal-restoring duties for the life of a progress
/// display: resolve whether the taskbar indicator is in play, and install the
/// signal handlers that cover a death the renderer's own teardown can't.
pub(crate) fn arm() {
    CLEAR_OSC_INDICATOR.store(
        osc_progress_supported(),
        std::sync::atomic::Ordering::Relaxed,
    );
    arm_signal_handlers();
}

/// Hand them back once the display is retired.
pub(crate) fn disarm() {
    disarm_signal_handlers();
}

#[cfg(unix)]
mod signals {
    use super::{CLEAR_OSC_INDICATOR, CLEAR_OSC_PROGRESS, SHOW_CURSOR, write_stderr};
    use std::io::IsTerminal;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    /// The signals that kill aube by default and are catchable — the same set
    /// `process_guard` forwards to a spawned child.
    const HANDLED: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

    /// A `sigaction` saved while arming. The struct is plain C data; the
    /// `Send` bound is only needed to park it in a `static`.
    struct SavedAction(libc::sigaction);

    // SAFETY: `libc::sigaction` is a POD struct — handler addresses, a mask,
    // and flags. It owns no thread-bound resource, so moving it between
    // threads is meaningless rather than unsound.
    unsafe impl Send for SavedAction {}

    /// What [`arm`] displaced, and how many progress displays are relying on
    /// it. The count matters because `embed::install` may be driven
    /// concurrently against one terminal: without it the first install to
    /// finish would hand the signals back while the second one's bar was
    /// still painting, and a Ctrl-C after that would leave the cursor hidden
    /// — the very bug this restores from.
    struct ArmState {
        /// Progress displays currently armed. Only the transition through 1
        /// touches dispositions.
        holders: usize,
        /// Dispositions displaced on the way in, restored on the way out.
        displaced: Vec<(libc::c_int, SavedAction)>,
    }

    static STATE: Mutex<ArmState> = Mutex::new(ArmState {
        holders: 0,
        displaced: Vec::new(),
    });

    /// Whether aube may take `current` over for the duration of the progress
    /// display.
    ///
    /// Only a signal still at `SIG_DFL` is fair game. Anything else already
    /// has an owner — `SIG_IGN` from `nohup` or a shell backgrounding without
    /// job control, a handler from a host embedding the command layer — and
    /// POSIX is explicit about not handling what was inherited as ignored.
    /// The same restraint applied to an existing handler keeps aube out of a
    /// hand-back dance it cannot win: a signal handler can't take a lock to
    /// coordinate with [`disarm`], so any scheme where aube displaces a live
    /// handler and later puts it back races the teardown. A host that manages
    /// its own signals keeps them, and with them the job of restoring the
    /// terminal; aube restores it on every path it still controls.
    fn may_take_over(current: &libc::sigaction) -> bool {
        current.sa_sigaction == libc::SIG_DFL
    }

    /// Whether `current` is the disposition aube installs, i.e. whether aube
    /// still owns the signal.
    fn ours(current: &libc::sigaction) -> bool {
        current.sa_sigaction == restore_and_reraise as *const () as usize
    }

    /// Write the terminal back to a usable state, then die from `sig` as if
    /// aube had never installed a handler.
    ///
    /// Everything here is async-signal-safe: `write`, `sigaction`, and
    /// `raise`. Resetting to `SIG_DFL` before re-raising is both correct and
    /// complete, because [`may_take_over`] only lets aube handle a signal that
    /// was at `SIG_DFL` to begin with — so the parent shell still sees a
    /// signal death (`$? == 130` for Ctrl-C) rather than a plain exit code.
    extern "C" fn restore_and_reraise(sig: libc::c_int) {
        // SAFETY: async-signal-safe calls only, on consts that outlive the
        // process.
        write_stderr(SHOW_CURSOR);
        // SAFETY: async-signal-safe calls only, on consts that outlive the
        // process.
        unsafe {
            if CLEAR_OSC_INDICATOR.load(Ordering::Relaxed) {
                write_stderr(CLEAR_OSC_PROGRESS);
            }
            let mut default_action: libc::sigaction = std::mem::zeroed();
            default_action.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(sig, &default_action, std::ptr::null_mut());
            libc::raise(sig);
        }
    }

    /// Install [`restore_and_reraise`] for the signals that would otherwise
    /// kill aube mid-frame. No-op when stderr isn't a terminal (nothing to
    /// restore), when already armed, or for any signal [`may_take_over`]
    /// declines.
    ///
    /// Scoped to the window where the renderer owns the terminal rather than
    /// installed for the whole process, so it can't displace the handlers
    /// `process_guard` relies on to forward signals to a `dlx` / `exec` child.
    pub(crate) fn arm() {
        let Ok(mut state) = STATE.lock() else {
            return;
        };
        // Counted before the terminal check so every `arm` has a matching
        // `disarm`, whether or not there was anything to install.
        state.holders += 1;
        if state.holders > 1 || !std::io::stderr().is_terminal() {
            return;
        }
        for sig in HANDLED {
            // SAFETY: `action` is fully initialized before use and the
            // handler is a plain `extern "C"` function. The displaced
            // disposition is kept for `disarm`.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut current) != 0 {
                    continue;
                }
                // Cheap rejection, so the common `nohup` / host-handler case
                // never sees aube's handler installed even for an instant.
                if !may_take_over(&current) {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = restore_and_reraise as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &action, &mut previous) != 0 {
                    continue;
                }
                // The query and this install are two syscalls, so an owner
                // that appeared in between would have been missed. `sigaction`
                // swaps atomically, which makes what it hands back the
                // authoritative answer: if that isn't the `SIG_DFL` the query
                // promised, put it straight back and leave the signal alone.
                if !may_take_over(&previous) {
                    libc::sigaction(sig, &previous, std::ptr::null_mut());
                    continue;
                }
                state.displaced.push((sig, SavedAction(previous)));
            }
        }
    }

    /// Put back whatever dispositions [`arm`] displaced — unless someone
    /// else has taken the signal over in the meantime.
    ///
    /// An embedding host can install its own handler while aube holds the
    /// signal, and writing the saved `SIG_DFL` over that would hand the host a
    /// process that dies on the next Ctrl-C. So the disposition is read first
    /// and left completely untouched when it is no longer aube's: not even
    /// swapped out and back, which would leave a moment where a signal killed
    /// the process instead of reaching the host.
    ///
    /// The write that follows is still checked, because the read and the write
    /// are two syscalls and POSIX has no compare-and-set for dispositions — a
    /// handler installed in between comes back from the swap and goes
    /// straight back in. That last interleaving is the one window this can't
    /// close: it needs a host to install a handler within the few instructions
    /// between the two calls *and* a signal to arrive before the revert. Every
    /// wider version of the race — the host installing at any other point
    /// while aube is armed — is handled above.
    pub(crate) fn disarm() {
        let Ok(mut state) = STATE.lock() else {
            return;
        };
        state.holders = state.holders.saturating_sub(1);
        if state.holders > 0 {
            return;
        }
        let displaced = std::mem::take(&mut state.displaced);
        for (sig, previous) in displaced {
            // SAFETY: `previous` came from a successful `sigaction` call on
            // this same signal; `current` and `replaced` are each written by
            // the call above them before being read.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut current) != 0 {
                    continue;
                }
                if !ours(&current) {
                    continue;
                }
                let mut replaced: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &previous.0, &mut replaced) != 0 {
                    continue;
                }
                if !ours(&replaced) {
                    libc::sigaction(sig, &replaced, std::ptr::null_mut());
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        extern "C" fn host_handler(_sig: libc::c_int) {}

        fn action_with(sa_sigaction: usize) -> libc::sigaction {
            // SAFETY: the only field read by `may_take_over` is written here.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = sa_sigaction;
            action
        }

        #[test]
        fn only_a_default_disposition_is_taken_over() {
            assert!(
                may_take_over(&action_with(libc::SIG_DFL)),
                "a signal nobody manages is aube's to restore the terminal from",
            );
            assert!(
                !may_take_over(&action_with(libc::SIG_IGN)),
                "nohup and job-control-free backgrounding ignore these on purpose",
            );
            assert!(
                !may_take_over(&action_with(host_handler as *const () as usize)),
                "an embedding host's handler stays the owner of its signal",
            );
        }
    }
}

#[cfg(unix)]
use signals::{arm as arm_signal_handlers, disarm as disarm_signal_handlers};

/// Windows has no `sigaction`; console control handlers are a different
/// mechanism and aube's progress display is the only thing that would want
/// one, so the signal half is Unix-only. [`restore_now`] still covers panics
/// everywhere.
#[cfg(not(unix))]
fn arm_signal_handlers() {}

#[cfg(not(unix))]
fn disarm_signal_handlers() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_sequences_match_what_clx_emits() {
        // clx writes `ESC [ ? 25 h` to show the cursor and
        // `ESC ] 9 ; 4 ; <state> ; <progress> ESC \` for the indicator, with
        // state 0 meaning "no indicator". Pinning the bytes keeps the
        // signal-safe path in step with the teardown path it stands in for.
        assert_eq!(SHOW_CURSOR, b"\x1b[?25h");
        assert_eq!(CLEAR_OSC_PROGRESS, b"\x1b]9;4;0;0\x1b\\");
    }

    #[test]
    fn osc_support_follows_the_terminal_advertisement() {
        // `TERM_PROGRAM` is read per call, so this only asserts the mapping
        // for the process's own environment shape: an unknown terminal with
        // none of the marker variables must not get the sequence.
        if std::env::var_os("TERM_PROGRAM").is_none()
            && std::env::var_os("WT_SESSION").is_none()
            && std::env::var_os("VTE_VERSION").is_none()
        {
            assert!(!osc_progress_supported());
        }
    }
}
