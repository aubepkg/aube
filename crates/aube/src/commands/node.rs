use clap::Args;
#[cfg(not(unix))]
use miette::IntoDiagnostic;
use miette::miette;
use std::ffi::OsString;

#[derive(Debug, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct NodeArgs {
    /// Arguments to pass to Node.js
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

pub async fn run(args: NodeArgs) -> miette::Result<Option<i32>> {
    crate::runtime::ensure_for_cwd(&crate::dirs::cwd()?).await?;
    let node = crate::runtime::node_program();

    #[cfg(unix)]
    let mut cmd = std::process::Command::new(&node);
    #[cfg(not(unix))]
    let mut cmd = tokio::process::Command::new(&node);

    cmd.args(&args.args);
    let runtime_dirs = crate::runtime::path_entries();
    if !runtime_dirs.is_empty() {
        cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
            "failed to exec node at {}: {err}",
            node.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().await.into_diagnostic().map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
                "failed to spawn node at {}: {e}",
                node.display()
            )
        })?;
        Ok(status.code())
    }
}
