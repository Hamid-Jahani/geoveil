//! `geoveil-tray`: a system tray front end for the `geoveil run` daemon.
//!
//! It shows the daemon status (engaged, paused, country / time zone, last
//! error) and offers Pause / Resume, Restore now, Open log folder and Quit.
//! It talks to the daemon over local IPC (see the `geoveil-ipc` crate). If
//! no daemon answers, it starts `geoveil run` itself with the options saved
//! in `<config dir>/geoveil/daemon.json` (written by
//! `geoveil autostart enable`), and Quit stops that daemon again, which
//! restores the original settings.
//!
//! Options:
//! - `--autostart`: passed by the login entry; behaves like a normal start.
//! - `--no-start-daemon`: never start `geoveil run` automatically.
//!
//! Its own log goes to `<local data dir>/geoveil/logs/geoveil-tray.log`, the
//! daemon's to `geoveil.log` in the same folder.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod icon;
mod launcher;
mod model;
mod worker;

use geoveil_ipc::{launch, DaemonOptions};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    auto_start_daemon: bool,
    help: bool,
    version: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args { auto_start_daemon: true, help: false, version: false };
    for arg in args {
        match arg.as_str() {
            "--autostart" => {}
            "--no-start-daemon" => parsed.auto_start_daemon = false,
            "-h" | "--help" => parsed.help = true,
            "-V" | "--version" => parsed.version = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(parsed)
}

const USAGE: &str = "usage: geoveil-tray [--autostart] [--no-start-daemon]\n\
    Tray front end for `geoveil run`. Starts the daemon when none is running\n\
    (unless --no-start-daemon), with the options saved by `geoveil autostart enable`.";

/// Sends logs to `<log_dir>/geoveil-tray.log` (a GUI app has no console).
fn init_logging(log_dir: &Path) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = std::fs::create_dir_all(log_dir).and_then(|()| {
        std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("geoveil-tray.log"))
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    let _ = match file {
        Ok(f) => builder.with_writer(std::sync::Mutex::new(f)).try_init(),
        Err(_) => builder.with_writer(std::io::stderr).try_init(),
    };
}

/// The saved daemon options; unreadable options are logged and replaced by
/// the defaults.
fn load_options() -> DaemonOptions {
    let Some(path) = launch::options_path() else { return DaemonOptions::default() };
    DaemonOptions::load(&path).unwrap_or_else(|e| {
        tracing::warn!("ignoring {}: {e}", path.display());
        DaemonOptions::default()
    })
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("geoveil-tray {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let log_dir = launch::log_dir().unwrap_or_else(|| PathBuf::from("."));
    init_logging(&log_dir);
    let options = load_options();
    tracing::info!("geoveil-tray {} starting", env!("CARGO_PKG_VERSION"));
    let link = worker::IpcLink::new(options, log_dir.clone());
    match app::run(link, args.auto_start_daemon, log_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn arguments() {
        assert_eq!(parse(&[]).unwrap(), Args { auto_start_daemon: true, help: false, version: false });
        assert!(parse(&["--autostart"]).unwrap().auto_start_daemon);
        assert!(!parse(&["--autostart", "--no-start-daemon"]).unwrap().auto_start_daemon);
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["-V"]).unwrap().version);
        assert!(parse(&["--bogus"]).is_err());
    }
}
