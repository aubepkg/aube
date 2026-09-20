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
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

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

    /// The same dispositions again, reachable from the handler, one slot per
    /// entry in [`HANDLED`]. [`SAVED`] can't serve that purpose: locking a
    /// mutex in a signal handler is not async-signal-safe, and the handler has
    /// to know what it displaced so it can hand the signal back to an
    /// embedding host's handler rather than to `SIG_DFL`.
    ///
    /// Each slot is allocated at most once per process and rewritten in place
    /// by later arms, which only run while disarmed — so no handler can be
    /// reading the value as it changes. Publication happens before aube's
    /// handler is installed, so a slot the handler can reach is always
    /// populated.
    static DISPLACED: [AtomicPtr<libc::sigaction>; HANDLED.len()] =
        [const { AtomicPtr::new(std::ptr::null_mut()) }; HANDLED.len()];

    /// Whether the bar still owns the terminal, readable from the handler
    /// (an atomic load is async-signal-safe; [`SAVED`] is not). Only set
    /// while [`SAVED`] is populated, so it tracks the armed window exactly.
    static ARMED: AtomicBool = AtomicBool::new(false);

    /// aube's own disposition. `SA_NODEFER` matters: without it the kernel
    /// blocks the signal for the duration of the handler, the handler's
    /// `raise` only marks it pending, and the displaced disposition wouldn't
    /// act until after the handler returned — too late to hand control to it
    /// and take the signal back afterwards.
    ///
    /// # Safety
    ///
    /// Async-signal-safe: `sigemptyset` is on the POSIX list and nothing here
    /// allocates, so the handler can rebuild its own action to reinstall.
    unsafe fn our_action() -> libc::sigaction {
        // SAFETY: every field is written before the value is used.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = restore_and_reraise as *const () as usize;
            action.sa_flags = libc::SA_NODEFER;
            libc::sigemptyset(&mut action.sa_mask);
            action
        }
    }

    /// Park `action` in the handler-reachable slot for `index`, reusing the
    /// slot's allocation when it already has one.
    fn publish_displaced(index: usize, action: libc::sigaction) {
        let slot = &DISPLACED[index];
        let parked = slot.load(Ordering::Acquire);
        if parked.is_null() {
            slot.store(Box::into_raw(Box::new(action)), Ordering::Release);
        } else {
            // SAFETY: the pointer came from `Box::into_raw` here and is only
            // written while disarmed, so aube's handler — the only reader —
            // cannot be running.
            unsafe { *parked = action };
        }
    }

    /// Write the terminal back to a usable state, then hand `sig` on to
    /// whatever disposition aube displaced — as if the handler had never been
    /// installed.
    ///
    /// Everything here is async-signal-safe: `write`, `sigaction`, and
    /// `raise`. Putting the displaced disposition back before re-raising is
    /// what keeps the outcome unchanged: `SIG_DFL` for a normal run, so the
    /// parent shell still sees a signal death (`$? == 130` for Ctrl-C), and an
    /// embedding host's own handler when there was one, so aube shadows it
    /// only for the redraw and not for the decision about what the signal
    /// means.
    extern "C" fn restore_and_reraise(sig: libc::c_int) {
        // SAFETY: async-signal-safe calls only. The escape sequences are
        // consts that outlive the process, and the displaced disposition was
        // published before this handler could be reached.
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
            let displaced = HANDLED
                .iter()
                .position(|handled| *handled == sig)
                .map(|index| DISPLACED[index].load(Ordering::Acquire))
                .filter(|parked| !parked.is_null());
            match displaced {
                Some(parked) => {
                    libc::sigaction(sig, parked, std::ptr::null_mut());
                }
                None => {
                    let mut default_action: libc::sigaction = std::mem::zeroed();
                    default_action.sa_sigaction = libc::SIG_DFL;
                    libc::sigaction(sig, &default_action, std::ptr::null_mut());
                }
            }
            // `SA_NODEFER` leaves the signal unblocked here, so this delivers
            // to the restored disposition immediately rather than pending
            // until the handler returns. `SIG_DFL` terminates the process
            // inside this call and never comes back.
            libc::raise(sig);
            // Only an embedding host's handler that chose to return gets
            // here. The renderer is still painting, so aube takes the signal
            // back for the rest of the progress window — otherwise the next
            // one would kill the host with the cursor hidden again.
            if ARMED.load(Ordering::Acquire) {
                let action = our_action();
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    }

    /// Install [`restore_and_reraise`] for the signals that would otherwise
    /// kill aube mid-frame. No-op when stderr isn't a terminal (nothing to
    /// restore) or when already armed.
    ///
    /// Scoped to the window where the renderer owns the terminal rather than
    /// installed for the whole process, so it can't displace the handlers
    /// `process_guard` relies on to forward signals to a `dlx` / `exec` child,
    /// and so an embedding host's own handlers are shadowed only while aube
    /// is actually painting — and even then only for the redraw, since
    /// [`restore_and_reraise`] hands the signal straight back to whatever it
    /// displaced. For the standalone binary that is always `SIG_DFL`: handler
    /// dispositions don't survive `exec`, so only an in-process embedder can
    /// have one here.
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
        ARMED.store(true, Ordering::Release);
        for (index, sig) in HANDLED.into_iter().enumerate() {
            // SAFETY: `action` is fully initialized before use and the
            // handler is a plain `extern "C"` function. The displaced
            // disposition is kept for `disarm`.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut current) != 0 {
                    continue;
                }
                // A disposition of `SIG_IGN` is inherited, not incidental:
                // `nohup`, and any shell backgrounding a job without job
                // control, ignores these in the child on purpose. POSIX is
                // explicit that a process must not install a handler for a
                // signal it inherited as ignored — doing so would make
                // `aube install &` die on a Ctrl-C the shell meant only for
                // the foreground job.
                if current.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                // Publish before installing: once aube's handler is in
                // place it may run at any moment, and it reads this slot to
                // decide who the signal belongs to.
                publish_displaced(index, current);
                let action = our_action();
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
        // Cleared first: a handler already running must not reinstall aube's
        // disposition behind the restore below.
        ARMED.store(false, Ordering::Release);
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

        /// Bumped by the stand-in "embedding host" handler below.
        static HOST_HANDLER_RUNS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);

        extern "C" fn host_handler(_sig: libc::c_int) {
            HOST_HANDLER_RUNS.fetch_add(1, Ordering::Release);
        }

        /// Read a signal's current disposition.
        fn disposition(sig: libc::c_int) -> libc::sigaction {
            // SAFETY: a null `act` only queries; `current` is fully written
            // by the call before it is read.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut current);
                current
            }
        }

        /// Stands in for `cli_main` running inside a host that installed its
        /// own handler: aube's handler must pass the signal on to it rather
        /// than to `SIG_DFL`, and must still be in place for the next one —
        /// otherwise the terminal is only ever restored once.
        ///
        /// Uses SIGHUP because the harness itself doesn't, and drives the
        /// handler directly rather than through `arm`, which no-ops without
        /// an interactive stderr.
        #[test]
        fn a_displaced_host_handler_runs_and_aube_takes_the_signal_back() {
            let sig = libc::SIGHUP;
            let index = HANDLED
                .iter()
                .position(|handled| *handled == sig)
                .expect("SIGHUP is one of the handled signals");

            // SAFETY: every call is a plain libc signal API on a signal this
            // test owns for its duration; the disposition is restored below.
            unsafe {
                let mut host: libc::sigaction = std::mem::zeroed();
                host.sa_sigaction = host_handler as *const () as usize;
                libc::sigemptyset(&mut host.sa_mask);
                let mut displaced: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, &host, &mut displaced);

                publish_displaced(index, disposition(sig));
                ARMED.store(true, Ordering::Release);
                let ours = our_action();
                libc::sigaction(sig, &ours, std::ptr::null_mut());

                libc::raise(sig);
                assert_eq!(
                    HOST_HANDLER_RUNS.load(Ordering::Acquire),
                    1,
                    "the displaced host handler should have run",
                );
                assert_eq!(
                    disposition(sig).sa_sigaction,
                    ours.sa_sigaction,
                    "aube should hold the signal again once the host handler returns",
                );

                libc::raise(sig);
                assert_eq!(
                    HOST_HANDLER_RUNS.load(Ordering::Acquire),
                    2,
                    "a second signal should reach the host handler too",
                );

                ARMED.store(false, Ordering::Release);
                libc::sigaction(sig, &displaced, std::ptr::null_mut());
            }
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
