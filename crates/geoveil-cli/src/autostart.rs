//! `geoveil autostart enable|disable|status`: start `geoveil-tray` at login.
//!
//! Always per user, never a system service: the locale and region settings
//! belong to the logged-in user, and a service in session 0 has no tray.
//!
//! | OS | Mechanism | Where |
//! |---|---|---|
//! | Windows | `Run` registry value, written with `reg.exe` | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `Geoveil` |
//! | Linux | `systemd --user` unit, `WantedBy=graphical-session.target` | `$XDG_CONFIG_HOME/systemd/user/geoveil-tray.service` |
//! | macOS | LaunchAgent, loaded by launchd at the next login | `~/Library/LaunchAgents/io.geoveil.tray.plist` |
//!
//! The tray starts with `--autostart` and, when no daemon answers, starts
//! `geoveil run` with the options saved by `enable` (see
//! [`geoveil_ipc::launch`]).
//!
//! Every external command goes through [`CommandRunner`] and files are
//! written below a configurable directory, so all of this is tested with
//! [`geoveil_platform::FakeRunner`] and a temporary directory.

use crate::cli::{AutostartAction, AutostartArgs, Cli};
use crate::commands::Exit;
use crate::output::emit;
use anyhow::{anyhow, bail, Context, Result};
use geoveil_ipc::launch::{self, DaemonOptions};
use geoveil_platform::{CommandOutput, CommandRunner, SystemRunner};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry key holding per-user startup programs.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Task Manager records startup entries the user turned off.
pub const APPROVED_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// Registry value name.
pub const VALUE_NAME: &str = "Geoveil";
/// systemd user unit name.
pub const UNIT_NAME: &str = "geoveil-tray.service";
/// launchd label (and plist file stem).
pub const AGENT_LABEL: &str = "io.geoveil.tray";
/// Flag the tray gets when started at login.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// Which mechanism to use, with its location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    /// HKCU Run key.
    Windows,
    /// systemd user unit in `unit_dir`.
    Linux { unit_dir: PathBuf },
    /// LaunchAgent plist in `agents_dir`.
    Macos { agents_dir: PathBuf },
}

/// Result of `enable`, `disable` and `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutostartStatus {
    /// Whether geoveil-tray starts at login.
    pub enabled: bool,
    /// Mechanism name.
    pub mechanism: &'static str,
    /// Registry value or file path.
    pub location: String,
    /// The registered command, when present.
    pub command: Option<String>,
    /// Extra information (for example "turned off in Task Manager").
    pub note: Option<String>,
}

/// Autostart management for one platform.
pub struct Autostart {
    runner: Arc<dyn CommandRunner>,
    platform: Platform,
}

impl Autostart {
    /// Uses `platform` and runs commands through `runner`.
    pub fn new(runner: Arc<dyn CommandRunner>, platform: Platform) -> Self {
        Self { runner, platform }
    }

    /// The mechanism for the host OS.
    pub fn for_host(runner: Arc<dyn CommandRunner>) -> Result<Self> {
        let platform = if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
            Platform::Macos { agents_dir: home.join("Library").join("LaunchAgents") }
        } else {
            let config = dirs::config_dir().ok_or_else(|| anyhow!("cannot determine the config directory"))?;
            Platform::Linux { unit_dir: config.join("systemd").join("user") }
        };
        Ok(Self::new(runner, platform))
    }

    fn mechanism(&self) -> &'static str {
        match self.platform {
            Platform::Windows => "registry Run key (HKCU)",
            Platform::Linux { .. } => "systemd user unit",
            Platform::Macos { .. } => "LaunchAgent",
        }
    }

    fn location(&self) -> String {
        match &self.platform {
            Platform::Windows => format!(r"{RUN_KEY}\{VALUE_NAME}"),
            Platform::Linux { unit_dir } => unit_dir.join(UNIT_NAME).display().to_string(),
            Platform::Macos { agents_dir } => agents_dir.join(format!("{AGENT_LABEL}.plist")).display().to_string(),
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        self.runner.run(program, args).with_context(|| format!("running {program}"))
    }

    /// Registers `tray` to start at login.
    pub fn enable(&self, tray: &Path) -> Result<AutostartStatus> {
        match &self.platform {
            Platform::Windows => {
                let data = windows_command_line(tray)?;
                let out = self.run("reg", &["add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &data, "/f"])?;
                check(&out, "reg add")?;
            }
            Platform::Linux { unit_dir } => {
                let unit = systemd_unit(tray)?;
                std::fs::create_dir_all(unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
                let path = unit_dir.join(UNIT_NAME);
                std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
                let out = self.run("systemctl", &["--user", "daemon-reload"])?;
                check(&out, "systemctl --user daemon-reload")?;
                let out = self.run("systemctl", &["--user", "enable", UNIT_NAME])?;
                check(&out, "systemctl --user enable")?;
            }
            Platform::Macos { agents_dir } => {
                let plist = launch_agent_plist(tray)?;
                std::fs::create_dir_all(agents_dir).with_context(|| format!("creating {}", agents_dir.display()))?;
                let path = agents_dir.join(format!("{AGENT_LABEL}.plist"));
                std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;
            }
        }
        self.status()
    }

    /// Removes the login entry. Not being registered is not an error.
    pub fn disable(&self) -> Result<AutostartStatus> {
        match &self.platform {
            Platform::Windows => {
                let out = self.run("reg", &["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])?;
                // Exit code 1 with "unable to find" just means it was not registered.
                if !out.success() && !is_reg_not_found(&out) {
                    check(&out, "reg delete")?;
                }
            }
            Platform::Linux { unit_dir } => {
                let path = unit_dir.join(UNIT_NAME);
                if path.exists() {
                    let out = self.run("systemctl", &["--user", "disable", UNIT_NAME])?;
                    if !out.success() {
                        tracing::warn!("systemctl --user disable failed: {}", out.stderr.trim());
                    }
                    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                    let out = self.run("systemctl", &["--user", "daemon-reload"])?;
                    if !out.success() {
                        tracing::warn!("systemctl --user daemon-reload failed: {}", out.stderr.trim());
                    }
                }
            }
            Platform::Macos { agents_dir } => {
                let path = agents_dir.join(format!("{AGENT_LABEL}.plist"));
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
                }
            }
        }
        self.status()
    }

    /// Reads the current registration. Changes nothing.
    pub fn status(&self) -> Result<AutostartStatus> {
        let mut status = AutostartStatus {
            enabled: false,
            mechanism: self.mechanism(),
            location: self.location(),
            command: None,
            note: None,
        };
        match &self.platform {
            Platform::Windows => {
                let out = self.run("reg", &["query", RUN_KEY, "/v", VALUE_NAME])?;
                if out.success() {
                    status.command = parse_reg_value(&out.stdout, VALUE_NAME).map(|(_, data)| data);
                }
                status.enabled = status.command.is_some();
                if status.enabled {
                    let approved = self.run("reg", &["query", APPROVED_KEY, "/v", VALUE_NAME])?;
                    if approved.success() && approved_is_disabled(&approved.stdout) {
                        status.enabled = false;
                        status.note = Some(
                            "registered, but turned off in Task Manager > Startup apps; turn it on there".into(),
                        );
                    }
                }
            }
            Platform::Linux { unit_dir } => {
                let path = unit_dir.join(UNIT_NAME);
                if let Ok(text) = std::fs::read_to_string(&path) {
                    status.command = text.lines().find_map(|l| l.strip_prefix("ExecStart=")).map(str::to_string);
                    let out = self.run("systemctl", &["--user", "is-enabled", UNIT_NAME])?;
                    let state = out.stdout.trim().to_string();
                    status.enabled = state == "enabled";
                    if !status.enabled {
                        let shown = if state.is_empty() { out.stderr.trim().to_string() } else { state };
                        status.note = Some(format!("unit file present but not enabled ({shown})"));
                    }
                }
            }
            Platform::Macos { agents_dir } => {
                let path = agents_dir.join(format!("{AGENT_LABEL}.plist"));
                if let Ok(text) = std::fs::read_to_string(&path) {
                    status.enabled = text.contains(&format!("<string>{AGENT_LABEL}</string>"));
                    status.command = plist_program(&text);
                    status.note = Some("launchd loads it at the next login".into());
                }
            }
        }
        Ok(status)
    }
}

/// Fails with the tool's own message when it exited non-zero.
fn check(out: &CommandOutput, what: &str) -> Result<()> {
    if out.success() {
        return Ok(());
    }
    let msg = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
    bail!("{what} failed (exit {}): {msg}", out.status)
}

/// `reg delete` / `reg query` output for a missing value.
fn is_reg_not_found(out: &CommandOutput) -> bool {
    out.status == 1 && format!("{}{}", out.stdout, out.stderr).to_ascii_lowercase().contains("unable to find")
}

/// Checks that `path` can be written into a registry value, unit file or
/// plist without quoting surprises: absolute, UTF-8, no control characters,
/// no double quotes.
pub fn validate_tray_path(path: &Path) -> Result<&str> {
    let s = path.to_str().ok_or_else(|| anyhow!("the tray path {} is not valid UTF-8", path.display()))?;
    if !path.is_absolute() {
        bail!("the tray path must be absolute, got {s}");
    }
    if s.chars().any(|c| c.is_control() || c == '"') {
        bail!("the tray path contains a control character or a double quote: {s:?}");
    }
    Ok(s)
}

/// Run-key data: the quoted executable and [`AUTOSTART_FLAG`].
pub fn windows_command_line(tray: &Path) -> Result<String> {
    let s = validate_tray_path(tray)?;
    Ok(format!("\"{s}\" {AUTOSTART_FLAG}"))
}

/// Quotes one systemd `ExecStart` word: `\` and `"` escaped, `%` and `$`
/// doubled so systemd expands nothing.
fn systemd_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The systemd user unit that starts the tray with the graphical session.
pub fn systemd_unit(tray: &Path) -> Result<String> {
    let s = validate_tray_path(tray)?;
    Ok(format!(
        "# Written by `geoveil autostart enable`; remove with `geoveil autostart disable`.\n\
         [Unit]\n\
         Description=Geoveil tray (follows the VPN with geoveil run)\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} {AUTOSTART_FLAG}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        systemd_quote(s)
    ))
}

/// Escapes text for an XML element.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The LaunchAgent plist. `KeepAlive.SuccessfulExit=false` restarts the tray
/// after a crash but not after the user chose Quit.
pub fn launch_agent_plist(tray: &Path) -> Result<String> {
    let s = validate_tray_path(tray)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `geoveil autostart enable`; remove with `geoveil autostart disable`. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>{AUTOSTART_FLAG}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        xml_escape(s)
    ))
}

/// The first `ProgramArguments` entry of a plist we wrote.
fn plist_program(text: &str) -> Option<String> {
    let after = text.split("<key>ProgramArguments</key>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].to_string())
}

/// Finds `name` in `reg query` output: `    Name    REG_SZ    data`.
/// Returns the type and the data.
pub fn parse_reg_value(stdout: &str, name: &str) -> Option<(String, String)> {
    stdout.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(name)?;
        // The name must be followed by the column separator, not more name.
        if !rest.starts_with("    ") && !rest.starts_with('\t') {
            return None;
        }
        let rest = rest.trim_start();
        let (kind, data) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        kind.starts_with("REG_").then(|| (kind.to_string(), data.trim().to_string()))
    })
}

/// Whether a `StartupApproved\Run` value says the entry is turned off.
/// Task Manager stores 12 bytes; the first byte is `02` (on) or `03` (off).
pub fn approved_is_disabled(stdout: &str) -> bool {
    match parse_reg_value(stdout, VALUE_NAME) {
        Some((kind, data)) if kind == "REG_BINARY" => data.get(..2).is_some_and(|b| b == "03"),
        _ => false,
    }
}

/// `geoveil-tray` next to the running `geoveil`.
pub fn default_tray_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the geoveil executable")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("{} has no parent directory", exe.display()))?;
    Ok(dir.join(format!("geoveil-tray{}", std::env::consts::EXE_SUFFIX)))
}

/// The saved daemon options after `enable`: the global flags replace
/// `locale`, `location` and `state_dir`; other fields keep what the user set.
pub fn merged_options(existing: DaemonOptions, cli: &Cli) -> DaemonOptions {
    DaemonOptions {
        locale: cli.locale,
        location: cli.location,
        state_dir: cli.state_dir.as_ref().map(|d| std::path::absolute(d).unwrap_or_else(|_| d.clone())),
        ..existing
    }
}

/// `geoveil autostart ...`.
pub async fn run(cli: &Cli, args: &AutostartArgs) -> Result<Exit> {
    let autostart = Autostart::for_host(Arc::new(SystemRunner))?;
    let json = cli.json;
    let (status, saved) = match &args.action {
        AutostartAction::Enable { tray_path } => {
            let tray = match tray_path {
                Some(p) => std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?,
                None => default_tray_path()?,
            };
            if !tray.is_file() {
                bail!(
                    "{} does not exist; build or install geoveil-tray next to geoveil, or pass --tray-path",
                    tray.display()
                );
            }
            let path = launch::options_path().ok_or_else(|| anyhow!("cannot determine the config directory"))?;
            let existing = DaemonOptions::load(&path).with_context(|| format!("reading {}", path.display()))?;
            let options = merged_options(existing, cli);
            options.save(&path).with_context(|| format!("writing {}", path.display()))?;
            let status = tokio::task::spawn_blocking(move || autostart.enable(&tray)).await??;
            (status, Some((path, options)))
        }
        AutostartAction::Disable => (tokio::task::spawn_blocking(move || autostart.disable()).await??, None),
        AutostartAction::Status => (tokio::task::spawn_blocking(move || autostart.status()).await??, None),
    };
    let value = serde_json::json!({
        "autostart": &status,
        "daemon_options": saved.as_ref().map(|(p, o)| serde_json::json!({ "file": p.display().to_string(), "options": o })),
    });
    emit(json, &value, || {
        let mut s = format!(
            "autostart: {}\nmechanism: {}\nlocation:  {}\n",
            if status.enabled { "enabled" } else { "disabled" },
            status.mechanism,
            status.location
        );
        if let Some(c) = &status.command {
            s.push_str(&format!("command:   {c}\n"));
        }
        if let Some(n) = &status.note {
            s.push_str(&format!("note:      {n}\n"));
        }
        if let Some((path, o)) = &saved {
            let args: Vec<String> = o.run_args().iter().map(|a| a.to_string_lossy().into_owned()).collect();
            s.push_str(&format!("the tray starts `geoveil {}` when no daemon runs (saved in {})\n", args.join(" "), path.display()));
        }
        s
    });
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use clap::Parser;
    use geoveil_platform::FakeRunner;

    fn tray() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Program Files\Geoveil\geoveil-tray.exe")
        } else {
            PathBuf::from("/opt/geo veil/geoveil-tray")
        }
    }

    fn windows(fake: &Arc<FakeRunner>) -> Autostart {
        Autostart::new(fake.clone(), Platform::Windows)
    }

    fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput { status, stdout: stdout.into(), stderr: stderr.into() }
    }

    const REG_QUERY: &str = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    Geoveil    REG_SZ    \"C:\\Program Files\\Geoveil\\geoveil-tray.exe\" --autostart\r\n\r\n";

    #[test]
    fn path_validation() {
        assert!(validate_tray_path(&tray()).is_ok());
        assert!(validate_tray_path(Path::new("relative/geoveil-tray")).is_err());
        let bad = tray().with_file_name("a\"b");
        assert!(validate_tray_path(&bad).is_err());
        let bad = tray().with_file_name("a\nb");
        assert!(validate_tray_path(&bad).is_err());
    }

    #[test]
    fn windows_enable_writes_run_value() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_stdout("The operation completed successfully.\r\n");
        fake.push_stdout(REG_QUERY);
        fake.push(out(1, "", "ERROR: The system was unable to find the specified registry key or value.\r\n"));
        let status = windows(&fake).enable(&tray()).unwrap();
        let calls = fake.raw_calls();
        assert_eq!(calls[0].0, "reg");
        let data = format!("\"{}\" --autostart", tray().display());
        assert_eq!(calls[0].1, ["add", RUN_KEY, "/v", "Geoveil", "/t", "REG_SZ", "/d", data.as_str(), "/f"]);
        assert_eq!(fake.calls()[1], format!(r"reg query {RUN_KEY} /v Geoveil"));
        assert_eq!(fake.calls()[2], format!(r"reg query {APPROVED_KEY} /v Geoveil"));
        assert!(status.enabled);
        assert_eq!(status.command.as_deref(), Some(r#""C:\Program Files\Geoveil\geoveil-tray.exe" --autostart"#));
        assert_eq!(status.location, format!(r"{RUN_KEY}\Geoveil"));
    }

    #[test]
    fn windows_enable_reports_reg_failure() {
        let fake = Arc::new(FakeRunner::new());
        fake.push(out(1, "", "ERROR: Access is denied.\r\n"));
        let err = windows(&fake).enable(&tray()).unwrap_err();
        assert!(format!("{err:#}").contains("Access is denied"), "{err:#}");
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn windows_status_not_registered() {
        let fake = Arc::new(FakeRunner::new());
        fake.push(out(1, "", "ERROR: The system was unable to find the specified registry key or value.\r\n"));
        let status = windows(&fake).status().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.command, None);
        assert_eq!(fake.calls().len(), 1, "StartupApproved is only read for a registered entry");
    }

    #[test]
    fn windows_status_turned_off_in_task_manager() {
        let fake = Arc::new(FakeRunner::new());
        fake.push_stdout(REG_QUERY);
        fake.push_stdout("\r\nHKEY_CURRENT_USER\\...\\StartupApproved\\Run\r\n    Geoveil    REG_BINARY    030000000000000000000000\r\n");
        let status = windows(&fake).status().unwrap();
        assert!(!status.enabled);
        assert!(status.note.unwrap().contains("Task Manager"));
    }

    #[test]
    fn windows_disable_tolerates_missing_value() {
        let fake = Arc::new(FakeRunner::new());
        fake.push(out(1, "", "ERROR: The system was unable to find the specified registry key or value.\r\n"));
        fake.push(out(1, "", "ERROR: The system was unable to find the specified registry key or value.\r\n"));
        let status = windows(&fake).disable().unwrap();
        assert!(!status.enabled);
        assert_eq!(fake.calls()[0], format!(r"reg delete {RUN_KEY} /v Geoveil /f"));
    }

    #[test]
    fn windows_disable_reports_other_errors() {
        let fake = Arc::new(FakeRunner::new());
        fake.push(out(1, "", "ERROR: Access is denied.\r\n"));
        assert!(windows(&fake).disable().is_err());
    }

    #[test]
    fn reg_output_parsing() {
        assert_eq!(
            parse_reg_value(REG_QUERY, "Geoveil"),
            Some(("REG_SZ".into(), r#""C:\Program Files\Geoveil\geoveil-tray.exe" --autostart"#.into()))
        );
        // Another value whose name starts with ours does not match.
        assert_eq!(parse_reg_value("    GeoveilOld    REG_SZ    x\r\n", "Geoveil"), None);
        assert_eq!(parse_reg_value("", "Geoveil"), None);
        assert!(approved_is_disabled("    Geoveil    REG_BINARY    030000000000000000000000"));
        assert!(!approved_is_disabled("    Geoveil    REG_BINARY    020000000000000000000000"));
        assert!(!approved_is_disabled("    Geoveil    REG_SZ    03"));
    }

    fn linux(fake: &Arc<FakeRunner>, dir: &TempDir) -> Autostart {
        Autostart::new(fake.clone(), Platform::Linux { unit_dir: dir.path().join("systemd").join("user") })
    }

    #[test]
    fn linux_enable_writes_unit_and_enables_it() {
        let dir = TempDir::new("autostart-linux");
        let fake = Arc::new(FakeRunner::new());
        fake.push_stdout("").push_stdout("").push_stdout("enabled\n");
        let status = linux(&fake, &dir).enable(&tray()).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable geoveil-tray.service",
                "systemctl --user is-enabled geoveil-tray.service"
            ]
        );
        let unit = std::fs::read_to_string(dir.path().join("systemd/user/geoveil-tray.service")).unwrap();
        assert!(unit.contains("WantedBy=graphical-session.target"));
        assert!(status.enabled);
        assert_eq!(status.command, Some(format!("\"{}\" --autostart", tray().display()).replace('\\', "\\\\")));
    }

    #[test]
    fn linux_enable_reports_systemctl_failure() {
        let dir = TempDir::new("autostart-linux-fail");
        let fake = Arc::new(FakeRunner::new());
        fake.push(out(1, "", "Failed to connect to bus: No medium found"));
        let err = linux(&fake, &dir).enable(&tray()).unwrap_err();
        assert!(format!("{err:#}").contains("Failed to connect to bus"));
    }

    #[test]
    fn linux_status_and_disable() {
        let dir = TempDir::new("autostart-linux-dis");
        let fake = Arc::new(FakeRunner::new());
        let a = linux(&fake, &dir);
        // Nothing installed: no commands at all.
        assert!(!a.status().unwrap().enabled);
        assert!(a.disable().is_ok());
        assert!(fake.calls().is_empty());

        a.enable(&tray()).unwrap();
        fake.push(out(1, "disabled\n", ""));
        let s = a.status().unwrap();
        assert!(!s.enabled);
        assert!(s.note.unwrap().contains("disabled"));

        let before = fake.calls().len();
        let s = a.disable().unwrap();
        assert!(!s.enabled);
        assert_eq!(
            fake.calls()[before..],
            ["systemctl --user disable geoveil-tray.service", "systemctl --user daemon-reload"]
        );
        assert!(!dir.path().join("systemd/user/geoveil-tray.service").exists());
    }

    #[test]
    fn systemd_quoting() {
        assert_eq!(systemd_quote("/opt/a b/t"), "\"/opt/a b/t\"");
        assert_eq!(systemd_quote(r"/x\y%z$w"), r#""/x\\y%%z$$w""#);
        let unit = systemd_unit(&tray()).unwrap();
        assert!(unit.contains("ExecStart=\""));
        assert!(unit.contains("--autostart\n"));
    }

    #[test]
    fn macos_enable_status_disable() {
        let dir = TempDir::new("autostart-mac");
        let fake = Arc::new(FakeRunner::new());
        let a = Autostart::new(fake.clone(), Platform::Macos { agents_dir: dir.path().join("LaunchAgents") });
        assert!(!a.status().unwrap().enabled);
        let s = a.enable(&tray()).unwrap();
        assert!(s.enabled);
        assert_eq!(s.command, Some(tray().display().to_string()));
        let plist = std::fs::read_to_string(dir.path().join("LaunchAgents/io.geoveil.tray.plist")).unwrap();
        assert!(plist.contains("<string>io.geoveil.tray</string>"));
        assert!(plist.contains("<string>--autostart</string>"));
        assert!(!a.disable().unwrap().enabled);
        assert!(a.disable().is_ok(), "disabling twice is fine");
        assert!(fake.calls().is_empty(), "macOS registration needs no commands");
    }

    #[test]
    fn plist_escapes_xml() {
        let p = if cfg!(windows) { PathBuf::from(r"C:\a&b<c>\t") } else { PathBuf::from("/a&b<c>/t") };
        let plist = launch_agent_plist(&p).unwrap();
        assert!(plist.contains("a&amp;b&lt;c&gt;"));
        assert_eq!(plist_program(&plist).unwrap(), xml_escape(p.to_str().unwrap()));
    }

    #[test]
    fn enable_saves_global_flags_and_keeps_the_rest() {
        let cli = Cli::try_parse_from(["geoveil", "--locale", "autostart", "enable"]).unwrap();
        let existing = DaemonOptions {
            location: true,
            sticky: true,
            interval_secs: Some(9),
            geoveil_path: Some(PathBuf::from("g")),
            ..Default::default()
        };
        let merged = merged_options(existing, &cli);
        assert!(merged.locale && !merged.location);
        assert!(merged.sticky);
        assert_eq!(merged.interval_secs, Some(9));
        assert_eq!(merged.geoveil_path, Some(PathBuf::from("g")));
        assert_eq!(merged.state_dir, None);

        let cli = Cli::try_parse_from(["geoveil", "--state-dir", "rel", "autostart", "enable"]).unwrap();
        let merged = merged_options(DaemonOptions::default(), &cli);
        assert!(merged.state_dir.unwrap().is_absolute(), "a relative state dir is saved as absolute");
    }

    #[test]
    fn default_tray_path_is_next_to_geoveil() {
        let p = default_tray_path().unwrap();
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("geoveil-tray"));
    }
}
