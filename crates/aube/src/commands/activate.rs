use clap::{Args, ValueEnum};
use miette::miette;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct ActivateArgs {
    /// Shell to emit activation code for
    pub shell: ActivateShell,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ActivateShell {
    Bash,
    Fish,
    Zsh,
}

pub fn run(args: ActivateArgs) -> miette::Result<()> {
    let shim_dir = crate::tool_shims::shim_dir().ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "could not locate the aube shim dir"
        )
    })?;
    ensure_shims(&shim_dir)?;
    println!("{}", render_activation(args.shell, &shim_dir));
    Ok(())
}

fn ensure_shims(shim_dir: &Path) -> miette::Result<()> {
    std::fs::create_dir_all(shim_dir).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to create shim dir {}: {e}",
            shim_dir.display()
        )
    })?;
    let exe = std::env::current_exe().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to locate current executable for shim creation: {e}"
        )
    })?;
    for name in crate::tool_shims::TOOL_SHIMS {
        write_shim(shim_dir, &crate::tool_shims::shim_file_name(name), &exe)?;
    }
    Ok(())
}

fn write_shim(shim_dir: &Path, name: &str, exe: &Path) -> miette::Result<()> {
    let dest = shim_dir.join(name);
    let _ = std::fs::remove_file(&dest);
    create_executable_alias(exe, &dest).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to create shim {} -> {}: {e}",
            dest.display(),
            exe.display()
        )
    })
}

#[cfg(unix)]
fn create_executable_alias(exe: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(exe, dest).or_else(|_| {
        std::fs::hard_link(exe, dest).or_else(|_| {
            std::fs::copy(exe, dest)?;
            Ok(())
        })
    })
}

#[cfg(not(unix))]
fn create_executable_alias(exe: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::hard_link(exe, dest).or_else(|_| {
        std::fs::copy(exe, dest)?;
        Ok(())
    })
}

fn render_activation(shell: ActivateShell, shim_dir: &Path) -> String {
    let quoted = shell_double_quote(&shim_dir.display().to_string());
    match shell {
        ActivateShell::Bash | ActivateShell::Zsh => format!(
            "export {env}={quoted}\n\
             case \":$PATH:\" in\n\
             *\":${env}:\"*) ;;\n\
             *) export PATH=\"${env}:$PATH\" ;;\n\
             esac",
            env = crate::tool_shims::SHIM_DIR_ENV,
        ),
        ActivateShell::Fish => format!(
            "set -gx {env} {quoted}\n\
             if not contains -- ${env} $PATH\n\
                 set -gx PATH ${env} $PATH\n\
             end",
            env = crate::tool_shims::SHIM_DIR_ENV,
        ),
    }
}

fn shell_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/tmp/aube shims")
    }

    #[test]
    fn renders_bash_activation() {
        let out = render_activation(ActivateShell::Bash, &path());
        assert!(out.contains("export AUBE_SHIM_DIR=\"/tmp/aube shims\""));
        assert!(out.contains("case \":$PATH:\" in"));
        assert!(out.contains("export PATH=\"$AUBE_SHIM_DIR:$PATH\""));
    }

    #[test]
    fn renders_zsh_activation() {
        let out = render_activation(ActivateShell::Zsh, &path());
        assert!(out.contains("export AUBE_SHIM_DIR=\"/tmp/aube shims\""));
        assert!(out.contains("*\":$AUBE_SHIM_DIR:\"*) ;;"));
    }

    #[test]
    fn renders_fish_activation() {
        let out = render_activation(ActivateShell::Fish, &path());
        assert!(out.contains("set -gx AUBE_SHIM_DIR \"/tmp/aube shims\""));
        assert!(out.contains("if not contains -- $AUBE_SHIM_DIR $PATH"));
        assert!(out.contains("set -gx PATH $AUBE_SHIM_DIR $PATH"));
    }

    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(
            shell_double_quote(r#"/tmp/a"b$c\d`e"#),
            r#""/tmp/a\"b\$c\\d\`e""#
        );
    }
}
