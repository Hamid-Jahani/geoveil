//! `geoveil` binary: one-shot commands and the VPN-following daemon.

mod autostart;
mod cli;
mod commands;
mod daemon;
mod ipc;
mod lock;
mod output;
mod paths;
#[cfg(test)]
mod testutil;

use clap::Parser;
use cli::{Cli, Command};
use commands::Ctx;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli);
    let json = cli.json;
    match dispatch(cli).await {
        Ok(exit) => exit.into(),
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({ "error": format!("{e:#}") }));
            }
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<commands::Exit> {
    let ctx = Ctx::from_cli(&cli)?;
    match &cli.command {
        Command::Detect => commands::detect(&ctx).await,
        Command::Apply(args) => commands::apply(&ctx, args).await,
        Command::Restore => commands::restore(&ctx).await,
        Command::Status => commands::status(&ctx).await,
        Command::Run(args) => daemon::run(&ctx, args).await,
        Command::Autostart(args) => autostart::run(&cli, args).await,
    }
}

/// Logs go to stderr so `--json` output on stdout stays parseable.
fn init_tracing(cli: &Cli) {
    let daemon = matches!(cli.command, Command::Run(_));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cli::log_filter(cli.verbose, daemon)));
    // Ignore the error if a global subscriber is already installed.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
