//! The user's saved daemon options: how the tray starts `geoveil run` when no
//! daemon answers. Written by `geoveil autostart enable` (from its global
//! flags) and read by `geoveil-tray`.
//!
//! The file is `<config dir>/geoveil/daemon.json`:
//! `%APPDATA%\geoveil` on Windows, `$XDG_CONFIG_HOME/geoveil` on Linux,
//! `~/Library/Application Support/geoveil` on macOS. Missing fields take
//! their defaults, so a hand-edited file may list only what it changes.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// File name of the saved options inside the geoveil config directory.
pub const OPTIONS_FILE: &str = "daemon.json";

/// Options for a tray-started `geoveil run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonOptions {
    /// Pass `--locale`.
    pub locale: bool,
    /// Pass `--location`.
    pub location: bool,
    /// Pass `--state-dir PATH`.
    pub state_dir: Option<PathBuf>,
    /// Pass `--sticky`.
    pub sticky: bool,
    /// Pass `--interval-secs N`.
    pub interval_secs: Option<u64>,
    /// The `geoveil` executable; default: next to `geoveil-tray`, then `PATH`.
    pub geoveil_path: Option<PathBuf>,
}

impl DaemonOptions {
    /// Arguments for `geoveil`, starting with the `run` subcommand.
    pub fn run_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["run".into()];
        if self.locale {
            args.push("--locale".into());
        }
        if self.location {
            args.push("--location".into());
        }
        if let Some(dir) = &self.state_dir {
            args.push("--state-dir".into());
            args.push(dir.clone().into_os_string());
        }
        if self.sticky {
            args.push("--sticky".into());
        }
        if let Some(n) = self.interval_secs {
            args.push("--interval-secs".into());
            args.push(n.to_string().into());
        }
        args
    }

    /// Loads options from `path`. A missing file gives the defaults.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Saves options to `path`, creating its directory.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        text.push('\n');
        std::fs::write(path, text)
    }
}

/// The geoveil config directory for the current user.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("geoveil"))
}

/// Default path of the saved options.
pub fn options_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(OPTIONS_FILE))
}

/// Directory for the tray-started daemon's log file.
pub fn log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("geoveil").join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_are_just_run() {
        assert_eq!(DaemonOptions::default().run_args(), vec![OsString::from("run")]);
    }

    #[test]
    fn all_options_become_flags() {
        let o = DaemonOptions {
            locale: true,
            location: true,
            state_dir: Some(PathBuf::from("my state")),
            sticky: true,
            interval_secs: Some(10),
            geoveil_path: Some(PathBuf::from("ignored")),
        };
        let args: Vec<String> = o.run_args().into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args,
            ["run", "--locale", "--location", "--state-dir", "my state", "--sticky", "--interval-secs", "10"]
        );
    }

    #[test]
    fn partial_file_uses_defaults() {
        let o: DaemonOptions = serde_json::from_str(r#"{"locale":true}"#).unwrap();
        assert_eq!(o, DaemonOptions { locale: true, ..Default::default() });
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("geoveil-ipc-test-{}", std::process::id()));
        let path = dir.join("sub").join(OPTIONS_FILE);
        assert_eq!(DaemonOptions::load(&path).unwrap(), DaemonOptions::default());
        let o = DaemonOptions { location: true, interval_secs: Some(7), ..Default::default() };
        o.save(&path).unwrap();
        assert_eq!(DaemonOptions::load(&path).unwrap(), o);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(DaemonOptions::load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
