//! Starting `geoveil run` and opening the log folder.
//!
//! The command line is built by pure functions (tested); only
//! [`spawn_daemon`], [`request_stop`] and [`open_folder`] touch the system.

use geoveil_ipc::DaemonOptions;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Log file of a tray-started daemon, inside the log directory.
pub const LOG_FILE: &str = "geoveil.log";

/// The log is moved to `geoveil.log.1` when it grows past this.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Program and arguments for the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonCommand {
    /// Executable.
    pub program: PathBuf,
    /// Arguments, starting with `run`.
    pub args: Vec<OsString>,
}

/// Which `geoveil` to run: the configured path, else the one next to the
/// tray executable (if `exists` says so), else `geoveil` from `PATH`.
pub fn daemon_command(opts: &DaemonOptions, tray_dir: Option<&Path>, exists: impl Fn(&Path) -> bool) -> DaemonCommand {
    let name = format!("geoveil{}", std::env::consts::EXE_SUFFIX);
    let program = opts
        .geoveil_path
        .clone()
        .or_else(|| tray_dir.map(|d| d.join(&name)).filter(|p| exists(p)))
        .unwrap_or_else(|| PathBuf::from(name));
    DaemonCommand { program, args: opts.run_args() }
}

/// Whether the log should be rotated before appending.
pub fn needs_rotation(current_len: Option<u64>) -> bool {
    current_len.is_some_and(|len| len > MAX_LOG_BYTES)
}

/// Opens `<log_dir>/geoveil.log` for appending, rotating it first when big.
pub fn open_log(log_dir: &Path) -> io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(log_dir)?;
    let path = log_dir.join(LOG_FILE);
    if needs_rotation(std::fs::metadata(&path).ok().map(|m| m.len())) {
        let _ = std::fs::rename(&path, log_dir.join(format!("{LOG_FILE}.1")));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok((file, path))
}

/// Starts the daemon in the background with stdout and stderr appended to
/// the log. On Windows no console window is shown, and the daemon gets its
/// own console process group so [`request_stop`] can send it Ctrl-Break.
pub fn spawn_daemon(cmd: &DaemonCommand, log_dir: &Path) -> io::Result<(Child, PathBuf)> {
    let (mut log, path) = open_log(log_dir)?;
    let shown: Vec<String> = cmd.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let _ = writeln!(log, "--- geoveil-tray starting: {} {}", cmd.program.display(), shown.join(" "));
    let stderr = log.try_clone()?;
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args).stdin(Stdio::null()).stdout(log).stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`: run the console program without a window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        /// `CREATE_NEW_PROCESS_GROUP`: the child leads its own group.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    let child = command.spawn()?;
    Ok((child, path))
}

/// Asks a daemon started by [`spawn_daemon`] to stop the way Ctrl-C would,
/// so it restores the original settings before exiting. Used on Quit when
/// the daemon does not answer over IPC. Never kills it outright: a killed
/// daemon would leave the spoofed settings in place.
///
/// - Unix: `SIGTERM` (sent with `kill`).
/// - Windows: `CTRL_BREAK_EVENT` to the child's process group, after
///   attaching to its hidden console (the tray has none of its own).
pub fn request_stop(child: &Child) -> io::Result<()> {
    request_stop_pid(child.id())
}

#[cfg(unix)]
fn request_stop_pid(pid: u32) -> io::Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("kill -TERM {pid} failed ({status})")))
    }
}

#[cfg(windows)]
fn request_stop_pid(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    // SAFETY: plain Win32 calls without pointers. The tray (a GUI process)
    // has no console, so FreeConsole first is a no-op that makes sure
    // AttachConsole can succeed. CTRL_BREAK_EVENT goes to the process group
    // `pid` only, which the tray is not part of.
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sent = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
        let err = io::Error::last_os_error();
        FreeConsole();
        if sent == 0 {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn request_stop_pid(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "cannot signal processes on this platform"))
}

/// The file manager command that opens `dir`.
pub fn open_folder_command(dir: &Path) -> (&'static str, Vec<OsString>) {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    (program, vec![dir.as_os_str().to_owned()])
}

/// Opens `dir` (created if missing) in the file manager.
pub fn open_folder(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let (program, args) = open_folder_command(dir);
    // Not waited for: explorer.exe's exit code is meaningless and xdg-open
    // may block until the file manager exits.
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(name: &str) -> String {
        format!("{name}{}", std::env::consts::EXE_SUFFIX)
    }

    #[test]
    fn configured_path_wins() {
        let opts = DaemonOptions { geoveil_path: Some(PathBuf::from("/custom/geoveil")), ..Default::default() };
        let c = daemon_command(&opts, Some(Path::new("/opt/geoveil")), |_| true);
        assert_eq!(c.program, PathBuf::from("/custom/geoveil"));
        assert_eq!(c.args, vec![OsString::from("run")]);
    }

    #[test]
    fn sibling_of_tray_is_preferred_over_path() {
        let opts = DaemonOptions { locale: true, ..Default::default() };
        let c = daemon_command(&opts, Some(Path::new("/opt/geoveil")), |_| true);
        assert_eq!(c.program, Path::new("/opt/geoveil").join(exe("geoveil")));
        assert_eq!(c.args, vec![OsString::from("run"), OsString::from("--locale")]);
    }

    #[test]
    fn falls_back_to_path_lookup() {
        let c = daemon_command(&DaemonOptions::default(), Some(Path::new("/opt/geoveil")), |_| false);
        assert_eq!(c.program, PathBuf::from(exe("geoveil")));
        let c = daemon_command(&DaemonOptions::default(), None, |_| true);
        assert_eq!(c.program, PathBuf::from(exe("geoveil")));
    }

    #[test]
    fn log_rotation_threshold() {
        assert!(!needs_rotation(None));
        assert!(!needs_rotation(Some(MAX_LOG_BYTES)));
        assert!(needs_rotation(Some(MAX_LOG_BYTES + 1)));
    }

    #[test]
    fn log_is_appended_and_rotated() {
        let dir = std::env::temp_dir().join(format!("geoveil-tray-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut f, path) = open_log(&dir).unwrap();
        writeln!(f, "one").unwrap();
        drop(f);
        let (mut f, _) = open_log(&dir).unwrap();
        writeln!(f, "two").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        let (_f, _) = open_log(&dir).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(dir.join("geoveil.log.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn request_stop_sends_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        request_stop(&child).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(15), "{status:?}");
    }

    #[test]
    fn folder_opener_per_os() {
        let (program, args) = open_folder_command(Path::new("logs"));
        let expected = if cfg!(windows) {
            "explorer.exe"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert_eq!(program, expected);
        assert_eq!(args, vec![OsString::from("logs")]);
    }
}
