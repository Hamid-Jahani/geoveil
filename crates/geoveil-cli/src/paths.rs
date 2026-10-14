//! Where the snapshot file lives when `--state-dir` is not given.
//!
//! The state must be found again by whoever restores it. On Linux and macOS
//! the privileged backends (geoclue files in `/etc`, `systemsetup`) need
//! root, so a profile is often applied with `sudo` and restored later with
//! or without it. A per-user directory alone would make a plain
//! `geoveil restore` look in the wrong home and report "nothing to restore"
//! while `/etc` stays spoofed. So:
//!
//! 1. any known location that already holds a state file wins, checked in
//!    order: the system directory, the current user's data directory, the
//!    `sudo` invoker's data directory;
//! 2. otherwise root uses the system directory (`/var/lib/geoveil` on Linux,
//!    `/var/db/geoveil` on macOS) and everyone else their data directory.
//!
//! A non-root user who finds root's state can read it (`status` works) but
//! cannot take its lock, and is told to run the command the same way it was
//! applied. Windows has no system directory: the per-user one is used.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Candidate state directories for this process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Candidates {
    /// Machine-wide directory (Unix only).
    pub system: Option<PathBuf>,
    /// The current user's data directory.
    pub user: Option<PathBuf>,
    /// The data directory of the user who invoked `sudo`.
    pub sudo_user: Option<PathBuf>,
    /// Whether this process runs as root.
    pub is_root: bool,
}

impl Candidates {
    /// Candidates for the running process.
    fn detect() -> Self {
        Self {
            system: system_dir(),
            user: dirs::data_local_dir().map(|d| d.join("geoveil")),
            sudo_user: sudo_user_dir(),
            is_root: is_root(),
        }
    }
}

/// The state directory to use when `--state-dir` is not given.
pub fn default_state_dir() -> Result<PathBuf> {
    let state_file = crate::commands::STATE_FILE;
    choose_state_dir(&Candidates::detect(), |dir| dir.join(state_file).exists())
        .ok_or_else(|| anyhow!("cannot determine the local data directory; pass --state-dir"))
}

/// Pure selection logic (see the module docs). `has_state` tells whether a
/// directory already holds a state file.
pub(crate) fn choose_state_dir(c: &Candidates, has_state: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let existing = [&c.system, &c.user, &c.sudo_user]
        .into_iter()
        .flatten()
        .find(|dir| has_state(dir));
    if let Some(dir) = existing {
        return Some(dir.clone());
    }
    if c.is_root {
        c.system.clone().or_else(|| c.user.clone())
    } else {
        c.user.clone()
    }
}

#[cfg(target_os = "linux")]
fn system_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/var/lib/geoveil"))
}

#[cfg(target_os = "macos")]
fn system_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/var/db/geoveil"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_dir() -> Option<PathBuf> {
    None
}

/// Data directory of the `sudo` invoker (`SUDO_USER`), by the platform's
/// home-directory convention.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sudo_user_dir() -> Option<PathBuf> {
    let user = std::env::var("SUDO_USER").ok()?;
    sudo_user_dir_for(&user)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sudo_user_dir() -> Option<PathBuf> {
    None
}

/// `~user`'s geoveil data directory, for a plausible non-root user name.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn sudo_user_dir_for(user: &str) -> Option<PathBuf> {
    let plausible = !user.is_empty()
        && user != "root"
        && !user.starts_with('-')
        && !user.starts_with('.')
        && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !plausible {
        return None;
    }
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(format!("/Users/{user}/Library/Application Support/geoveil")))
    } else {
        Some(PathBuf::from(format!("/home/{user}/.local/share/geoveil")))
    }
}

/// Whether the effective user is root.
#[cfg(unix)]
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions, cannot fail and touches no memory
    // we own; uid_t is a 32-bit unsigned integer on Linux and macOS.
    unsafe { geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(is_root: bool) -> Candidates {
        Candidates {
            system: Some(PathBuf::from("/var/lib/geoveil")),
            user: Some(PathBuf::from("/home/me/.local/share/geoveil")),
            sudo_user: Some(PathBuf::from("/home/alice/.local/share/geoveil")),
            is_root,
        }
    }

    fn with_state(dirs: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p: &Path| dirs.iter().any(|d| Path::new(d) == p)
    }

    #[test]
    fn root_without_state_uses_system_dir() {
        assert_eq!(choose_state_dir(&cands(true), |_| false), Some(PathBuf::from("/var/lib/geoveil")));
    }

    #[test]
    fn user_without_state_uses_own_dir() {
        assert_eq!(
            choose_state_dir(&cands(false), |_| false),
            Some(PathBuf::from("/home/me/.local/share/geoveil"))
        );
    }

    #[test]
    fn plain_restore_finds_state_written_with_sudo() {
        // `sudo geoveil apply` wrote the system dir; a plain restore must find it.
        let chosen = choose_state_dir(&cands(false), with_state(&["/var/lib/geoveil"]));
        assert_eq!(chosen, Some(PathBuf::from("/var/lib/geoveil")));
    }

    #[test]
    fn sudo_run_finds_state_written_without_sudo() {
        // A plain apply wrote alice's dir; `sudo geoveil ...` must not start a
        // second engagement in the system dir (it would snapshot spoofed values).
        let chosen = choose_state_dir(&cands(true), with_state(&["/home/alice/.local/share/geoveil"]));
        assert_eq!(chosen, Some(PathBuf::from("/home/alice/.local/share/geoveil")));
    }

    #[test]
    fn system_state_wins_over_user_state() {
        let chosen = choose_state_dir(
            &cands(false),
            with_state(&["/var/lib/geoveil", "/home/me/.local/share/geoveil"]),
        );
        assert_eq!(chosen, Some(PathBuf::from("/var/lib/geoveil")));
    }

    #[test]
    fn windows_like_candidates() {
        let c = Candidates { user: Some(PathBuf::from(r"C:\Users\me\AppData\Local\geoveil")), ..Default::default() };
        assert_eq!(choose_state_dir(&c, |_| false), c.user.clone());
        assert_eq!(choose_state_dir(&Candidates::default(), |_| false), None);
    }

    #[test]
    fn sudo_user_names_are_validated() {
        assert!(sudo_user_dir_for("alice").is_some());
        assert!(sudo_user_dir_for("first.last").is_some());
        for bad in ["", "root", "../etc", "-x", "a/b", "a b", ".hidden"] {
            assert!(sudo_user_dir_for(bad).is_none(), "{bad}");
        }
    }
}
