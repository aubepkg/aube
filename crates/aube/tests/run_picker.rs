//! Exercise the actual CLI in a PTY: pipe-based tests never enter the picker.
//! The redraw fixes live in demand; guard that aube and aubr retain them.
#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const SYNC_START: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

struct Picker {
    child: Child,
    input: File,
    output: Receiver<Vec<u8>>,
}

impl Picker {
    /// Start a script picker with isolated configuration and a controlling PTY.
    fn start(project: &std::path::Path, multicall: bool) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes the two descriptors on success. The
        // size points to a valid winsize; null name/termios use defaults.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: openpty returned fresh descriptors; these Files own them.
        let (input, terminal) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: terminal is a live slave descriptor and attrs is writable.
        assert_eq!(
            unsafe { libc::tcgetattr(terminal.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        // SAFETY: the successful tcgetattr initialized attrs above.
        let mut attrs = unsafe { attrs.assume_init() };
        // Demand prints its first frame before read_key enters raw mode.
        // Disable echo/canonical buffering before spawn so input sent as soon
        // as that frame arrives cannot leak into the captured redraw stream.
        attrs.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
        attrs.c_cc[libc::VMIN] = 1;
        attrs.c_cc[libc::VTIME] = 0;
        // SAFETY: attrs came from this live terminal; only input flags changed.
        assert_eq!(
            unsafe { libc::tcsetattr(terminal.as_raw_fd(), libc::TCSANOW, &attrs) },
            0
        );
        for fd in [input.as_raw_fd(), terminal.as_raw_fd()] {
            // SAFETY: both descriptors are live. Only duplicated stdio
            // should survive exec, not the parent's PTY master.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }

        let mut command = Command::new(env!("CARGO_BIN_EXE_aube"));
        if multicall {
            command.arg0("aubr");
        } else {
            command.arg("run");
        }
        command
            .arg("--no-install")
            .current_dir(project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", project)
            .env("XDG_CONFIG_HOME", project)
            .env("XDG_CACHE_HOME", project.join("cache"))
            .env("XDG_DATA_HOME", project.join("data"))
            .env("AUBE_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env("TERM", "xterm-ghostty")
            .stdin(Stdio::from(terminal.try_clone().unwrap()))
            .stdout(Stdio::from(terminal.try_clone().unwrap()))
            .stderr(Stdio::from(terminal));
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        // stdio has already been installed by Command at this point.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let mut reader = input.try_clone().unwrap();
        let (send, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if send.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Linux returns EIO when the last slave closes.
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(e) => panic!("read PTY: {e}"),
                }
            }
        });
        Self {
            child,
            input,
            output,
        }
    }

    /// Collect through a frame terminator, or through EOF after confirmation.
    /// The latter retains every trailing PTY read so redraw regressions cannot
    /// hide outside a synchronized frame.
    fn output(&self, until_exit: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let chunk = match self.output.recv_timeout(remaining) {
                Ok(chunk) => chunk,
                Err(mpsc::RecvTimeoutError::Disconnected) if until_exit => {
                    return String::from_utf8(bytes).unwrap();
                }
                Err(error) => panic!(
                    "picker failed to finish output: {error}; output: {:?}",
                    String::from_utf8_lossy(&bytes)
                ),
            };
            bytes.extend(chunk);
            if !until_exit
                && bytes
                    .windows(SYNC_END.len())
                    .any(|w| w == SYNC_END.as_bytes())
            {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }

    /// Wait for the chosen script to exit with a bounded deadline.
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "picker did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        // Also clean up when an assertion fails while the picker awaits input.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
/// Guard redraw framing and script selection through both CLI entry points.
fn script_pickers_redraw_atomically_without_reprinting_the_heading() {
    for multicall in [false, true] {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{
            "name": "picker-regression",
            "scripts": {
                "aaa": "echo wrong > selected",
                "bbb": "echo selected-b > selected",
                "ccc": "echo wrong > selected"
            }
        }"#,
        )
        .unwrap();
        let mut picker = Picker::start(project.path(), multicall);
        let initial = picker.output(false);
        assert!(initial.contains("Select a script to run"), "{initial:?}");

        picker.input.write_all(b"\x1b[B").unwrap();
        let moved = picker.output(false);
        picker.input.write_all(b"\r").unwrap();
        let confirmed = picker.output(true);
        assert!(
            confirmed.contains("\x1b[?25h"),
            "cursor not restored: {confirmed:?}"
        );
        assert!(picker.wait().success());
        // Read through child exit, then delimit the navigation redraw by the
        // next frame's start. Any trailing repaint after its terminator stays
        // in this slice regardless of which PTY read delivered it. Confirmation
        // deliberately prints the title again, inside its own frame.
        let session = format!("{initial}{moved}{confirmed}");
        let frames: Vec<_> = session.split(SYNC_START).collect();
        assert_eq!(frames.len(), 4, "{session:?}");
        assert!(
            frames[0].is_empty(),
            "output before first frame: {session:?}"
        );
        let moved = frames[2];
        assert!(
            !moved.contains("Select a script to run"),
            "heading repainted: {moved:?}"
        );
        assert!(
            !moved.contains("package.json scripts"),
            "description repainted: {moved:?}"
        );
        assert!(
            moved.ends_with(SYNC_END),
            "redraw escaped the synchronized frame: {moved:?}"
        );
        assert_eq!(moved.matches(SYNC_END).count(), 1, "{moved:?}");
        assert!(
            moved.contains("\x1b[2K"),
            "selection never redrew: {moved:?}"
        );

        assert_eq!(
            std::fs::read_to_string(project.path().join("selected"))
                .unwrap()
                .trim(),
            "selected-b"
        );
    }
}
