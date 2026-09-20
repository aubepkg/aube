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

/// Whether the armed signal handlers should also clear the taskbar indicator.
/// Resolved while arming, because a signal handler can't call
/// [`osc_progress_supported`] — `getenv` is not async-signal-safe.
static CLEAR_OSC_ON_SIGNAL: AtomicBool = AtomicBool::new(false);

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

/// Restore the terminal from ordinary (non-signal) context: the panic hook.
///
/// Gated on an interactive stderr so a piped or redirected stream never gains
/// an escape sequence. Unlike the renderer's teardown this takes no locks, so
/// it stays safe even when the panic came from inside clx with its terminal
/// lock held.
pub(crate) fn restore_now() {
    use std::io::Write;

    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(SHOW_CURSOR);
    if osc_progress_supported() {
        let _ = stderr.write_all(CLEAR_OSC_PROGRESS);
    }
    let _ = stderr.flush();
}

#[cfg(unix)]
mod signals {
    use super::{CLEAR_OSC_ON_SIGNAL, CLEAR_OSC_PROGRESS, SHOW_CURSOR, osc_progress_supported};
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

    /// Dispositions displaced by [`arm`], restored by [`disarm`]. Empty while
    /// disarmed, which is also how double-arming is detected.
    static SAVED: Mutex<Vec<(libc::c_int, SavedAction)>> = Mutex::new(Vec::new());

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
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                SHOW_CURSOR.as_ptr().cast(),
                SHOW_CURSOR.len(),
            );
            if CLEAR_OSC_ON_SIGNAL.load(Ordering::Relaxed) {
                libc::write(
                    libc::STDERR_FILENO,
                    CLEAR_OSC_PROGRESS.as_ptr().cast(),
                    CLEAR_OSC_PROGRESS.len(),
                );
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
        if !std::io::stderr().is_terminal() {
            return;
        }
        let Ok(mut saved) = SAVED.lock() else {
            return;
        };
        if !saved.is_empty() {
            return;
        }
        CLEAR_OSC_ON_SIGNAL.store(osc_progress_supported(), Ordering::Relaxed);
        for sig in HANDLED {
            // SAFETY: `action` is fully initialized before use and the
            // handler is a plain `extern "C"` function. The displaced
            // disposition is kept for `disarm`.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut current) != 0 {
                    continue;
                }
                if !may_take_over(&current) {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = restore_and_reraise as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &action, &mut previous) == 0 {
                    saved.push((sig, SavedAction(previous)));
                }
            }
        }
    }

    /// Put back whatever dispositions [`arm`] displaced.
    pub(crate) fn disarm() {
        let Ok(mut saved) = SAVED.lock() else {
            return;
        };
        for (sig, previous) in saved.drain(..) {
            // SAFETY: `previous` came from a successful `sigaction` call on
            // this same signal.
            unsafe {
                libc::sigaction(sig, &previous.0, std::ptr::null_mut());
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
pub(crate) use signals::{arm as arm_signal_handlers, disarm as disarm_signal_handlers};

/// Windows has no `sigaction`; console control handlers are a different
/// mechanism and aube's progress display is the only thing that would want
/// one, so the signal half is Unix-only. [`restore_now`] still covers panics
/// everywhere.
#[cfg(not(unix))]
pub(crate) fn arm_signal_handlers() {}

#[cfg(not(unix))]
pub(crate) fn disarm_signal_handlers() {}

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
