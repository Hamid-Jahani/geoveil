//! On-disk persistence of the engaged state (original settings + active
//! profile), so original settings survive crashes and can always be restored.

use crate::types::{BackendSnapshot, Profile};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Everything needed to undo an engagement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngagedState {
    /// The profile currently applied.
    pub profile: Profile,
    /// Original settings captured before the first apply.
    pub snapshots: Vec<BackendSnapshot>,
    /// When the engagement started (seconds since the Unix epoch).
    pub engaged_at_unix: u64,
}

/// Errors from reading or writing the snapshot file.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot file I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// A JSON file holding the [`EngagedState`] while Geoveil is engaged.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    path: PathBuf,
}

/// Adds the path and the attempted operation to an I/O error, keeping its kind.
fn with_context(err: io::Error, op: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{op} {}: {err}", path.display()))
}

impl SnapshotStore {
    /// Creates a store backed by the file at `path` (not touched until used).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The snapshot file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Temporary file used for atomic writes (same directory as the target).
    fn tmp_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "snapshot.json".into());
        name.push(".tmp");
        self.path.with_file_name(name)
    }

    /// Atomically writes `state`: writes a temp file in the same directory,
    /// fsyncs it, then renames it over the target. Creates parent directories.
    pub fn save(&self, state: &EngagedState) -> Result<(), SnapshotError> {
        let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = parent {
            fs::create_dir_all(dir).map_err(|e| with_context(e, "create directory", dir))?;
        }
        let json = serde_json::to_vec_pretty(state)?;
        let tmp = self.tmp_path();
        write_synced(&tmp, &json).map_err(|e| with_context(e, "write", &tmp))?;
        if let Err(e) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(with_context(e, "rename into", &self.path).into());
        }
        if let Some(dir) = parent {
            sync_dir(dir);
        }
        Ok(())
    }

    /// Loads the saved state. A missing file is `Ok(None)`; an unreadable or
    /// corrupted file is an error (never silently discarded, since it may hold
    /// the only copy of the user's original settings).
    pub fn load(&self) -> Result<Option<EngagedState>, SnapshotError> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(with_context(e, "read", &self.path).into()),
        };
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Deletes the snapshot file. Succeeds if it does not exist.
    pub fn clear(&self) -> Result<(), SnapshotError> {
        let _ = fs::remove_file(self.tmp_path());
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(with_context(e, "remove", &self.path).into()),
        }
    }
}

/// Writes `data` to `path` (truncating) and fsyncs the file.
fn write_synced(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Best-effort fsync of a directory so the rename itself is durable (Unix only;
/// directories cannot be opened this way on Windows).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProfileSource;

    fn state(tz: &str) -> EngagedState {
        EngagedState {
            profile: Profile {
                lat: 52.52,
                lon: 13.405,
                accuracy_m: 5000.0,
                timezone: tz.to_string(),
                locale: "de-DE".into(),
                country: "DE".into(),
                source: ProfileSource::VpnExit {
                    ip: "1.2.3.4".parse().unwrap(),
                },
            },
            snapshots: vec![BackendSnapshot {
                backend: "timezone".into(),
                data: serde_json::json!({"tz": "America/New_York"}),
            }],
            engaged_at_unix: 1_700_000_000,
        }
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("state.json"));
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn save_load_round_trip_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("a").join("b").join("state.json"));
        let s = state("Europe/Berlin");
        store.save(&s).unwrap();
        assert_eq!(store.load().unwrap(), Some(s));
        assert!(!store.tmp_path().exists(), "temp file must be renamed away");
    }

    #[test]
    fn save_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("state.json"));
        store.save(&state("Europe/Berlin")).unwrap();
        store.save(&state("Europe/Paris")).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.profile.timezone, "Europe/Paris");
    }

    #[test]
    fn corrupted_json_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"{ not json").unwrap();
        let store = SnapshotStore::new(&path);
        assert!(matches!(store.load(), Err(SnapshotError::Json(_))));
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("state.json"));
        store.clear().unwrap();
        store.save(&state("Europe/Berlin")).unwrap();
        store.clear().unwrap();
        assert!(!store.path().exists());
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
