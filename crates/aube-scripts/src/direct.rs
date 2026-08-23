//! Direct-exec fast path for `aube run`.
//!
//! Every package script normally goes through `sh -c "<body>"`. `/bin/sh`
//! does not exec in place — dash stays resident as the script's parent —
//! so a shell costs a whole extra process per invocation. For a body that
//! is one plain command (`tsc -p .`, `vitest run`, `node build.js`) the
//! shell contributes nothing but that process.
//!
//! This module decides when a body can skip the shell. It is deliberately
//! a strict allowlist rather than a tokenizer: a tokenizer splits words
//! but says nothing about shell *semantics*, so it cannot tell us whether
//! the shell was load-bearing. Anything we do not recognize with
//! certainty falls back to `sh -c`, which is the pre-existing behavior.
//! Every bail is a correctness win traded for a process we keep paying.

use std::path::PathBuf;

/// Bytes allowed to appear anywhere in a directly-exec'd command line.
///
/// ASCII alphanumerics plus these. Everything else — `;` `&` `|` `(` `)`
/// `<` `>` `$` backtick `"` `'` `\` `*` `?` `[` `]` `{` `}` `~` `#` `!`
/// `^` `%`, newlines, tabs, other control bytes, and all non-ASCII —
/// means either a shell operator, an expansion, quoting, or something we
/// have not thought about, and sends the body to `sh`.
const EXTRA_ALLOWED: &[u8] = b" ._-+/@:,=";

/// Words that must never be exec'd directly, in two hazard classes.
///
/// The first is builtins with no binary at all: `exit 7` as a script body
/// works today and must keep working. The second is builtins that *do*
/// have a binary whose behavior differs from the shell's — `echo -e`,
/// `printf`, and `test` all diverge between dash, bash, and coreutils, so
/// exec'ing the binary would silently change what a script does.
///
/// Sorted for `binary_search`; `builtin_list_is_sorted_and_deduped`
/// enforces that. Missing an entry is not a correctness hole on its own,
/// because an unresolvable word falls back to the shell anyway — this
/// list is what makes the common builtins *guaranteed* rather than
/// accidentally correct.
#[rustfmt::skip]
const SHELL_WORDS: &[&str] = &[
    ".", ":", "[", "[[", "]]", "alias", "bg", "bind", "break", "builtin",
    "caller", "case", "cd", "command", "compgen", "complete", "continue",
    "coproc", "declare", "dirs", "disown", "do", "done", "echo", "elif",
    "else", "enable", "esac", "eval", "exec", "exit", "export", "false",
    "fg", "fi", "for", "function", "getopts", "hash", "history", "if",
    "in", "jobs", "kill", "let", "local", "logout", "mapfile", "popd",
    "printf", "pushd", "pwd", "read", "readarray", "readonly", "return",
    "select", "set", "shift", "shopt", "source", "suspend", "test",
    "then", "time", "times", "trap", "true", "type", "typeset", "ulimit",
    "umask", "unalias", "unset", "until", "wait", "while", "{", "}",
];

/// Split a script body into `(program, args)` when it is a single plain
/// command that a shell would add nothing to. `None` means "use `sh`".
///
/// Recognizes only bodies built from [`EXTRA_ALLOWED`] bytes whose first
/// word is a bare program name. See the module docs for why this is a
/// scanner and not a parser.
pub(crate) fn simple_command_argv(body: &str) -> Option<(&str, Vec<&str>)> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if !body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || EXTRA_ALLOWED.contains(&b))
    {
        return None;
    }

    // Space is the only separator that survived the scan, so the split is
    // unambiguous — no quoting or escaping can be in play.
    let mut words = body.split_ascii_whitespace();
    let program = words.next()?;

    // A leading `FOO=bar` is a shell assignment prefix, not a program. `=`
    // is still fine in later words (`--target=es2020`).
    if program.contains('=') {
        return None;
    }
    // Looks like a flag, so we have misread the body somehow.
    if program.starts_with('-') {
        return None;
    }
    // A path-shaped program would make us reason about how std resolves a
    // relative program against `current_dir`. Real scripts invoke bare
    // names (`tsc`, `vitest`, `node`), so the case is not worth owning.
    if program.contains('/') {
        return None;
    }
    if SHELL_WORDS.binary_search(&program).is_ok() {
        return None;
    }

    Some((program, words.collect()))
}

/// Find `program` on `path`, mirroring how the shell would resolve it.
///
/// This is a correctness requirement, not an optimization: on Unix
/// `Command::new("tsc")` resolves through `execvp`, which searches the
/// *parent's* environ and ignores the `PATH` we hand the child — so
/// without resolving here ourselves, a `node_modules/.bin` program would
/// not be found at all.
///
/// Costs one `stat` per PATH entry until a hit (typically one for a
/// project-local bin, three for `node`), which is noise next to the fork
/// and shell startup it replaces. Deliberately uncached: a cache would
/// have to be invalidated on every install, and there is nothing to win.
pub fn resolve_program(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        // POSIX reads an empty entry as the cwd. Rather than reason about
        // a cwd-relative match, treat the whole search as inconclusive
        // and let the shell handle the body.
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            return None;
        }
        let candidate = dir.join(program);
        // `metadata` follows symlinks, so a `.bin/tsc -> ../pkg/cli.js`
        // link resolves to the real file and its mode.
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if !is_executable(&meta) {
            continue;
        }
        return Some(candidate);
    }
    None
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    // Only reachable from tests; the fast path itself is Unix-only.
    true
}

/// Plan a direct exec of `body` against `path`, or `None` to use `sh`.
///
/// Returns `(resolved_program, program_as_written, args)`. The second
/// element becomes `argv[0]`, matching what the shell would have passed.
pub fn direct_argv<'a>(
    body: &'a str,
    path: &std::ffi::OsStr,
) -> Option<(PathBuf, &'a str, Vec<&'a str>)> {
    // Windows would need PATHEXT plus the `.cmd`/`.ps1`/bare-sh shim
    // triple, and `CreateProcess` cannot run a `.cmd` at all — Windows
    // re-enters `cmd.exe` for batch files, so the process we skipped
    // comes right back. `cfg!` rather than `#[cfg]` so this module's
    // tests still compile and run on the Windows CI job.
    if cfg!(windows) {
        return None;
    }

    // Scan first. It is a pure pass over the body, where every check below
    // reads settings (cloning the snapshot) or the environment — so a body
    // that was always going to need a shell pays nothing for asking.
    let (program, args) = simple_command_argv(body)?;

    let settings = crate::script_settings();
    // The user pointed scripts at a specific shell; run them in it.
    if settings.script_shell.is_some() {
        return None;
    }
    // Signals intent about shell semantics even though aube does not
    // currently emulate one.
    if settings.shell_emulator {
        return None;
    }
    // Where `/bin/sh` is bash (macOS), bash sources `$BASH_ENV` for
    // non-interactive shells, so a script body can legitimately depend on
    // functions or PATH edits from that file. Same for `$ENV` under a
    // POSIX sh. If either is set, the shell is load-bearing.
    if std::env::var_os("BASH_ENV").is_some() || std::env::var_os("ENV").is_some() {
        return None;
    }

    // Resolution failure is not an error — falling back to `sh` preserves
    // the shell's exit 127 and its exact `sh: 1: foo: not found` stderr,
    // and 126 for a hit that is not executable.
    let resolved = resolve_program(program, path)?;
    Some((resolved, program, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn argv(body: &str) -> Option<(String, Vec<String>)> {
        simple_command_argv(body)
            .map(|(p, a)| (p.to_string(), a.into_iter().map(String::from).collect()))
    }

    #[test]
    fn accepts_plain_commands() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("tsc -p .", "tsc", &["-p", "."]),
            ("vitest run", "vitest", &["run"]),
            ("next dev", "next", &["dev"]),
            ("node hello.js", "node", &["hello.js"]),
            ("eslint . --fix", "eslint", &[".", "--fix"]),
            (
                "esbuild src/x.ts --target=es2020",
                "esbuild",
                &["--target=es2020"],
            ),
            ("husky", "husky", &[]),
            ("  tsc  -p  .  ", "tsc", &["-p", "."]),
        ];
        for (body, program, _) in cases {
            let (got, _) = argv(body).unwrap_or_else(|| panic!("{body} should be direct"));
            assert_eq!(&got, program, "{body}");
        }
        // Spot-check full argv, including the `/`-containing later word
        // that the first-word `/` rule must not reject.
        assert_eq!(
            argv("esbuild src/x.ts --target=es2020"),
            Some((
                "esbuild".to_string(),
                vec!["src/x.ts".to_string(), "--target=es2020".to_string()]
            ))
        );
        assert_eq!(argv("husky"), Some(("husky".to_string(), vec![])));
    }

    #[test]
    fn bails_on_anything_a_shell_would_interpret() {
        let cases = [
            ("foo && bar", "and-chain"),
            ("foo; bar", "semicolon"),
            ("foo | bar", "pipe"),
            ("foo &", "background"),
            ("foo > out", "redirect out"),
            ("foo < in", "redirect in"),
            ("(foo)", "subshell"),
            ("a $V", "expansion"),
            ("a ${V}", "braced expansion"),
            ("a `b`", "command substitution"),
            ("a ~/x", "tilde"),
            ("a *.ts", "glob star"),
            ("a x?.ts", "glob question"),
            ("a [ab].ts", "glob class"),
            ("a {b,c}", "brace expansion"),
            ("a 'q'", "single quotes"),
            ("a \"q\"", "double quotes"),
            ("a\\b", "backslash"),
            ("FOO=bar node x.js", "assignment prefix"),
            ("# c", "comment"),
            ("node -e \"\"", "quoted -e"),
            ("foo\nbar", "newline"),
            ("foo\tbar", "tab"),
            ("café", "non-ascii"),
            ("-flag x", "leading flag"),
            ("./x.js", "relative path program"),
            ("node_modules/.bin/x", "path program"),
            ("", "empty"),
            ("   ", "blank"),
            ("a %V%", "percent"),
            ("a ^b", "caret"),
            ("a !b", "bang"),
        ];
        for (body, why) in cases {
            assert!(argv(body).is_none(), "{why}: {body:?} must use the shell");
        }
    }

    #[test]
    fn bails_on_shell_builtins_and_keywords() {
        // Split out so a failure names the class. The first group has no
        // binary at all; the second has one that behaves differently.
        for word in [
            "exit", ":", ".", "cd", "export", "unset", "set", "shift", "source", "eval", "exec",
            "read", "local", "readonly", "trap", "wait", "umask", "ulimit", "times", "hash",
            "getopts", "alias", "break", "continue", "return", "command", "type",
        ] {
            assert!(argv(word).is_none(), "builtin without a binary: {word}");
            assert!(argv(&format!("{word} 7")).is_none(), "with args: {word}");
        }
        for word in [
            "echo", "true", "false", "test", "[", "printf", "pwd", "kill",
        ] {
            assert!(
                argv(word).is_none(),
                "builtin with divergent binary: {word}"
            );
        }
        for word in [
            "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case",
            "esac", "in", "function", "select", "time", "[[", "{", "}",
        ] {
            assert!(argv(word).is_none(), "keyword: {word}");
        }
    }

    #[test]
    fn exit_seven_still_reaches_the_shell() {
        // Pins the `"boom": "exit 7"` e2e fixture: `exit` has no binary,
        // so exec'ing it would turn a working script into ENOENT.
        assert!(argv("exit 7").is_none());
    }

    #[test]
    fn builtin_list_is_sorted_and_deduped() {
        let mut sorted = SHELL_WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            SHELL_WORDS,
            &sorted[..],
            "SHELL_WORDS must stay sorted and deduped for binary_search"
        );
    }

    /// Unique scratch dir. `tempfile` is deliberately not a dep of this
    /// crate (see `aborting_script_kills_grandchildren`), so follow the
    /// same `temp_dir` + nanos convention.
    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("aube-direct-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn exe(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn join(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs.iter().map(|d| d.to_path_buf())).unwrap()
    }

    #[test]
    fn resolve_program_finds_an_executable() {
        let dir = scratch("hit");
        exe(&dir.join("tool"));
        assert_eq!(
            resolve_program("tool", &join(&[&dir])),
            Some(dir.join("tool"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_program_skips_non_executable_files() {
        let dir = scratch("noexec");
        std::fs::write(dir.join("tool"), "not executable").unwrap();
        assert_eq!(resolve_program("tool", &join(&[&dir])), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_program_skips_directories() {
        let dir = scratch("isdir");
        std::fs::create_dir(dir.join("tool")).unwrap();
        assert_eq!(resolve_program("tool", &join(&[&dir])), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_program_takes_the_first_hit_in_path_order() {
        let first = scratch("first");
        let second = scratch("second");
        exe(&first.join("tool"));
        exe(&second.join("tool"));
        assert_eq!(
            resolve_program("tool", &join(&[&first, &second])),
            Some(first.join("tool"))
        );
        std::fs::remove_dir_all(&first).ok();
        std::fs::remove_dir_all(&second).ok();
    }

    #[test]
    fn resolve_program_keeps_looking_past_a_dir_without_the_program() {
        let miss = scratch("miss");
        let hit = scratch("late-hit");
        exe(&hit.join("tool"));
        assert_eq!(
            resolve_program("tool", &join(&[&miss, &hit])),
            Some(hit.join("tool"))
        );
        std::fs::remove_dir_all(&miss).ok();
        std::fs::remove_dir_all(&hit).ok();
    }

    #[test]
    fn resolve_program_gives_up_on_a_relative_path_entry() {
        let dir = scratch("relative");
        exe(&dir.join("tool"));
        assert_eq!(
            resolve_program("tool", &join(&[Path::new("relative"), &dir])),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_program_ignores_a_dangling_symlink() {
        let dir = scratch("dangling");
        std::os::unix::fs::symlink(dir.join("nope"), dir.join("tool")).unwrap();
        assert_eq!(resolve_program("tool", &join(&[&dir])), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `direct_argv` reads the task-local settings snapshot, so drive it
    /// through `scope` the way `scoped_settings_tests` does rather than
    /// mutating the process-global fallback.
    async fn plans_under(settings: crate::ScriptSettings, dir: &Path) -> bool {
        let path = join(&[dir]);
        crate::scope(async move {
            crate::set_script_settings(settings);
            direct_argv("tool --flag x", &path).is_some()
        })
        .await
    }

    #[tokio::test]
    async fn direct_argv_declines_when_a_custom_script_shell_is_set() {
        let dir = scratch("script-shell");
        exe(&dir.join("tool"));
        let settings = crate::ScriptSettings {
            script_shell: Some(PathBuf::from("/bin/bash")),
            ..Default::default()
        };
        assert!(!plans_under(settings, &dir).await);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn direct_argv_declines_under_the_shell_emulator() {
        let dir = scratch("shell-emulator");
        exe(&dir.join("tool"));
        let settings = crate::ScriptSettings {
            shell_emulator: true,
            ..Default::default()
        };
        assert!(!plans_under(settings, &dir).await);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_argv_plans_a_bare_command_with_default_settings() {
        // `BASH_ENV` / `ENV` in the ambient environment legitimately veto
        // the fast path, so only assert the plan when this process is
        // clean. Reading them is why this is not a table with the two
        // decline cases above.
        if std::env::var_os("BASH_ENV").is_some() || std::env::var_os("ENV").is_some() {
            return;
        }
        let dir = scratch("plan");
        exe(&dir.join("tool"));
        let path = join(&[&dir]);
        let expected = dir.join("tool");
        crate::scope(async move {
            crate::set_script_settings(crate::ScriptSettings::default());
            let (resolved, word, args) = direct_argv("tool --flag x", &path).unwrap();
            assert_eq!(resolved, expected);
            assert_eq!(word, "tool");
            assert_eq!(args, vec!["--flag", "x"]);
        })
        .await;
        std::fs::remove_dir_all(&dir).ok();
    }
}
