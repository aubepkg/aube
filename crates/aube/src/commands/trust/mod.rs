//! Commands for inspecting npm publishing trust.

mod check;

use clap::{Args, Subcommand};

#[derive(Debug, usage_derive::Args)]
pub struct TrustArgs {
    #[usage(subcommand)]
    command: TrustCommand,
}

#[derive(Debug, usage_derive::Subcommands)]
enum TrustCommand {
    /// Check one package version for a publishing-trust downgrade
    Check(check::CheckArgs),
}

pub async fn run(args: TrustArgs) -> miette::Result<()> {
    match args.command {
        TrustCommand::Check(args) => check::run(args).await,
    }
}
