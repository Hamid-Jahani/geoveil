//! Exclusive, process-wide lock on the state directory.
//!
//! Every command that can change system settings (`apply`, `restore`, `run`)
//! holds this lock for as long as it may touch the snapshot file. Without it,
//! a second geoveil process could snapshot settings the first one already
//! spoofed and save them as the "originals", losing the real ones for good.
//!
//! The lock is tied to an open file handle, so the OS releases it when the
//! process exits or crashes; a stale lock file on disk is harmless.
//! - Unix: `flock(LOCK_EX | LOCK_NB)` on `<state dir>/geoveil.lock`.
//! - Windows: the lock file is opened with no sharing allowed.
//!
//! Both use only APIs available on the workspace MSRV (no `File::try_lock`).

use anyhow::{anyhow, Context, Result};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Lock file name inside the state directory.
pub const LOCK_FILE: &str = "geoveil.lock";

/// Held lock; released on drop.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
    path: PathBuf,
}

impl StateLock {
    /// Takes the lock for `state_dir`, creating the directory if needed.
    /// Fails immediately (no waiting) when another geoveil holds it.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("creating state directory {}", state_dir.display()))?;
        let path = state_dir.join(LOCK_FILE);
        let file = open_locked(&path).map_err(|e| describe(&path, e))?;
        Ok(Self { _file: file, path })
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Turns an io error into a user-facing explanation.
fn describe(path: &Path, err: io::Error) -> anyhow::Error {
    if is_busy(&err) {
        anyhow!(
            "another geoveil process is running (lock {} is held); stop it first, or use `geoveil status` to inspect",
            path.display()
        )
    } else if err.kind() == io::ErrorKind::PermissionDenied {
        anyhow!(
            "cannot lock {}: {err}; the saved state was probably written by another user (e.g. via sudo), \
             run this command the same way",
            path.display()
        )
    } else {
        anyhow!("cannot lock {}: {err}", path.display())
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::raw::c_int;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }
    // Same values on Linux, macOS and the BSDs.
    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;

    /// Opens (creating if needed) and `flock`s the file. Falls back to a
    /// read-only handle when the file exists but belongs to another user
    /// (flock works on any open descriptor).
    pub(super) fn open_locked(path: &Path) -> io::Result<File> {
        let file = match OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o644).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && path.exists() => {
                OpenOptions::new().read(true).open(path)?
            }
            Err(e) => return Err(e),
        };
        // SAFETY: `flock` only takes a file descriptor and flags; the
        // descriptor is valid because `file` owns it for the whole call.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            Ok(file)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn is_busy(err: &io::Error) -> bool {
        err.kind() == io::ErrorKind::WouldBlock
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    /// Opens (creating if needed) the file with no sharing: any other open
    /// of it fails until this handle is closed.
    pub(super) fn open_locked(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path)
    }

    pub(super) fn is_busy(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) fn open_locked(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
    }

    pub(super) fn is_busy(_err: &io::Error) -> bool {
        false
    }
}

use sys::{is_busy, open_locked};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn second_lock_is_refused_until_released() {
        let dir = TempDir::new("lock");
        let state = dir.path().join("nested").join("state");
        let first = StateLock::acquire(&state).unwrap();
        assert!(first.path().exists());
        let err = StateLock::acquire(&state).unwrap_err();
        assert!(format!("{err:#}").contains("another geoveil process is running"), "{err:#}");
        drop(first);
        let again = StateLock::acquire(&state).unwrap();
        assert_eq!(again.path(), state.join(LOCK_FILE));
    }

    #[test]
    fn different_directories_do_not_conflict() {
        let dir = TempDir::new("lock2");
        let _a = StateLock::acquire(&dir.path().join("a")).unwrap();
        let _b = StateLock::acquire(&dir.path().join("b")).unwrap();
    }
}
