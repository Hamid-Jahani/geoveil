//! Engage/disengage a set of backends safely.
//!
//! Safety rules:
//! - Original settings are snapshotted and persisted to disk **before** any
//!   change is applied.
//! - When already engaged (e.g. the VPN exit changed), the original snapshots
//!   are kept; the spoofed state is never snapshotted.
//! - A backend that cannot be snapshotted is never applied, so we never change
//!   something we cannot restore.
//! - Restores run in reverse order; the store is cleared only when every
//!   restore succeeded, so a failed restore can be retried (even after a crash).

use crate::backend::{Backend, BackendError};
use crate::snapshot::{EngagedState, SnapshotError, SnapshotStore};
use crate::types::{BackendSnapshot, Profile, VerifyReport};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of one backend operation (apply or restore).
#[derive(Debug, Clone, Serialize)]
pub struct BackendOutcome {
    pub backend: String,
    pub ok: bool,
    pub error: Option<String>,
}

impl BackendOutcome {
    fn ok(backend: &str) -> Self {
        Self {
            backend: backend.to_string(),
            ok: true,
            error: None,
        }
    }

    fn failed(backend: &str, error: impl Into<String>) -> Self {
        Self {
            backend: backend.to_string(),
            ok: false,
            error: Some(error.into()),
        }
    }

    fn from_result(backend: &str, result: Result<(), BackendError>) -> Self {
        match result {
            Ok(()) => Self::ok(backend),
            Err(e) => Self::failed(backend, e.to_string()),
        }
    }
}

/// Per-backend outcomes of an engage or disengage.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub outcomes: Vec<BackendOutcome>,
}

impl Report {
    /// True when every backend succeeded (vacuously true when empty).
    pub fn all_ok(&self) -> bool {
        self.outcomes.iter().all(|o| o.ok)
    }
}

/// Errors that abort an orchestrator operation as a whole.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("snapshot store: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("not engaged: no saved snapshot to restore")]
    NotEngaged,
}

/// Coordinates snapshot/apply/restore across all backends.
pub struct Orchestrator {
    backends: Vec<Box<dyn Backend>>,
    store: SnapshotStore,
    state: Option<EngagedState>,
}

/// Seconds since the Unix epoch (0 if the clock is before 1970).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn find_snapshot<'a>(snapshots: &'a [BackendSnapshot], name: &str) -> Option<&'a BackendSnapshot> {
    snapshots.iter().find(|s| s.backend == name)
}

impl Orchestrator {
    /// Creates an orchestrator, loading any state left in `store` by a previous
    /// run (crash recovery: [`is_engaged`](Self::is_engaged) is then true and
    /// [`disengage`](Self::disengage) restores the original settings).
    pub fn new(backends: Vec<Box<dyn Backend>>, store: SnapshotStore) -> Result<Self, OrchestratorError> {
        let state = store.load()?;
        Ok(Self {
            backends,
            store,
            state,
        })
    }

    /// Whether original settings are saved (i.e. a restore is pending).
    pub fn is_engaged(&self) -> bool {
        self.state.is_some()
    }

    /// The profile currently applied, if engaged.
    pub fn current_profile(&self) -> Option<&Profile> {
        self.state.as_ref().map(|s| &s.profile)
    }

    /// Applies `profile` to every backend.
    ///
    /// On first engagement, all backends are snapshotted and the state is
    /// persisted before anything is applied. When already engaged, the original
    /// snapshots are kept and only the profile is updated; backends that were
    /// never snapshotted (and so never changed) get a fresh snapshot attempt.
    /// Backends without a snapshot are skipped and reported as failures.
    ///
    /// When this instance holds no state it re-reads the store first: another
    /// process (or an earlier run) may have engaged since [`new`](Self::new),
    /// and snapshotting now would record its spoofed values as "originals"
    /// and overwrite the real ones on disk.
    pub fn engage(&mut self, profile: &Profile) -> Result<Report, OrchestratorError> {
        if self.state.is_none() {
            self.state = self.store.load()?;
            if self.state.is_some() {
                tracing::warn!(
                    path = %self.store.path().display(),
                    "found an engagement saved by another run; keeping its original snapshots"
                );
            }
        }
        let previous = self.state.clone();
        let mut state = match self.state.take() {
            Some(mut s) => {
                s.profile = profile.clone();
                s
            }
            None => EngagedState {
                profile: profile.clone(),
                snapshots: Vec::new(),
                engaged_at_unix: now_unix(),
            },
        };

        let snapshot_errors = self.snapshot_missing(&mut state);

        if let Err(e) = self.store.save(&state) {
            // Nothing applied in this call. Keep the previous engagement (if
            // any) so its original settings can still be restored.
            self.state = previous;
            return Err(e.into());
        }

        let outcomes = self
            .backends
            .iter()
            .map(|b| {
                let name = b.name();
                if find_snapshot(&state.snapshots, name).is_some() {
                    BackendOutcome::from_result(name, b.apply(profile))
                } else {
                    let why = snapshot_errors
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, e)| e.as_str())
                        .unwrap_or("no snapshot");
                    BackendOutcome::failed(name, format!("skipped (cannot snapshot original): {why}"))
                }
            })
            .collect();

        self.state = Some(state);
        Ok(Report { outcomes })
    }

    /// Snapshots every backend that has no saved snapshot yet. Returns the
    /// snapshot errors by backend name.
    ///
    /// Safe on re-engage: a backend without a snapshot was never applied, so
    /// its current state is still the original.
    fn snapshot_missing(&self, state: &mut EngagedState) -> Vec<(&'static str, String)> {
        let mut errors = Vec::new();
        for b in &self.backends {
            let name = b.name();
            if find_snapshot(&state.snapshots, name).is_some() {
                continue;
            }
            match b.snapshot() {
                Ok(mut snap) => {
                    // The snapshot key must match the backend we restore with.
                    snap.backend = name.to_string();
                    state.snapshots.push(snap);
                }
                Err(e) => {
                    tracing::warn!(backend = name, error = %e, "snapshot failed; backend will not be changed");
                    errors.push((name, e.to_string()));
                }
            }
        }
        errors
    }

    /// Restores every saved snapshot in reverse order.
    ///
    /// The store is cleared only when all restores succeed; otherwise the state
    /// is kept (in memory and on disk) so the restore can be retried. A saved
    /// snapshot with no matching backend counts as a failure.
    pub fn disengage(&mut self) -> Result<Report, OrchestratorError> {
        let state = self.state.as_ref().ok_or(OrchestratorError::NotEngaged)?;
        let outcomes: Vec<BackendOutcome> = state
            .snapshots
            .iter()
            .rev()
            .map(|snap| match self.backends.iter().find(|b| b.name() == snap.backend) {
                Some(b) => BackendOutcome::from_result(&snap.backend, b.restore(snap)),
                None => BackendOutcome::failed(
                    &snap.backend,
                    "no such backend enabled; enable it to restore this setting",
                ),
            })
            .collect();
        let report = Report { outcomes };
        if report.all_ok() {
            self.store.clear()?;
            self.state = None;
        } else {
            tracing::warn!("some restores failed; keeping snapshot for retry");
        }
        Ok(report)
    }

    /// Reads back each backend's state and compares it with the current
    /// profile. Empty when not engaged.
    pub fn verify(&self) -> Vec<VerifyReport> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        self.backends
            .iter()
            .map(|b| {
                b.verify(&state.profile).unwrap_or_else(|e| VerifyReport {
                    backend: b.name().to_string(),
                    ok: false,
                    detail: e.to_string(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProfileSource;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Shared, inspectable state for fake backends.
    #[derive(Default)]
    struct World {
        /// Simulated OS setting per backend name.
        values: std::collections::HashMap<&'static str, String>,
        /// Call log, e.g. "apply:timezone".
        log: Vec<String>,
    }

    type Shared = Arc<Mutex<World>>;

    #[derive(Default, Clone, Copy)]
    struct Fail {
        snapshot: bool,
        apply: bool,
        restore: bool,
    }

    struct FakeBackend {
        name: &'static str,
        world: Shared,
        store_path: PathBuf,
        fail: Fail,
    }

    impl FakeBackend {
        fn boxed(name: &'static str, world: &Shared, store_path: &std::path::Path, fail: Fail) -> Box<dyn Backend> {
            world
                .lock()
                .unwrap()
                .values
                .entry(name)
                .or_insert_with(|| format!("orig-{name}"));
            Box::new(Self {
                name,
                world: world.clone(),
                store_path: store_path.to_path_buf(),
                fail,
            })
        }

        fn err(&self, what: &str) -> BackendError {
            BackendError::Other {
                backend: self.name.into(),
                detail: format!("{what} failed"),
            }
        }
    }

    impl Backend for FakeBackend {
        fn name(&self) -> &'static str {
            self.name
        }

        fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
            let mut w = self.world.lock().unwrap();
            w.log.push(format!("snapshot:{}", self.name));
            if self.fail.snapshot {
                return Err(self.err("snapshot"));
            }
            Ok(BackendSnapshot {
                backend: self.name.into(),
                data: serde_json::json!({ "value": w.values[self.name] }),
            })
        }

        fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
            let mut w = self.world.lock().unwrap();
            w.log.push(format!("apply:{}", self.name));
            assert!(self.store_path.exists(), "store must be persisted before apply");
            if self.fail.apply {
                return Err(self.err("apply"));
            }
            w.values.insert(self.name, profile.timezone.clone());
            Ok(())
        }

        fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
            let mut w = self.world.lock().unwrap();
            w.log.push(format!("restore:{}", self.name));
            if self.fail.restore {
                return Err(self.err("restore"));
            }
            let v = snapshot.data["value"].as_str().unwrap().to_string();
            w.values.insert(self.name, v);
            Ok(())
        }

        fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
            let w = self.world.lock().unwrap();
            let cur = &w.values[self.name];
            Ok(VerifyReport {
                backend: self.name.into(),
                ok: *cur == profile.timezone,
                detail: format!("current {cur}, expected {}", profile.timezone),
            })
        }
    }

    fn profile(tz: &str) -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: tz.into(),
            locale: "de-DE".into(),
            country: "DE".into(),
            source: ProfileSource::VpnExit {
                ip: "1.2.3.4".parse().unwrap(),
            },
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        path: PathBuf,
        world: Shared,
    }

    impl Setup {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state").join("engaged.json");
            Self {
                _dir: dir,
                path,
                world: Shared::default(),
            }
        }

        fn orch(&self, specs: &[(&'static str, Fail)]) -> Orchestrator {
            let backends = specs
                .iter()
                .map(|&(n, f)| FakeBackend::boxed(n, &self.world, &self.path, f))
                .collect();
            Orchestrator::new(backends, SnapshotStore::new(&self.path)).unwrap()
        }

        fn value(&self, name: &str) -> String {
            self.world.lock().unwrap().values[name].clone()
        }

        fn log(&self) -> Vec<String> {
            self.world.lock().unwrap().log.clone()
        }

        fn clear_log(&self) {
            self.world.lock().unwrap().log.clear();
        }
    }

    const OK: Fail = Fail {
        snapshot: false,
        apply: false,
        restore: false,
    };

    #[test]
    fn engage_snapshots_all_then_persists_then_applies() {
        let s = Setup::new();
        let mut o = s.orch(&[("timezone", OK), ("locale", OK)]);
        assert!(!o.is_engaged());
        let r = o.engage(&profile("Europe/Berlin")).unwrap();
        assert!(r.all_ok());
        assert_eq!(
            s.log(),
            vec!["snapshot:timezone", "snapshot:locale", "apply:timezone", "apply:locale"]
        );
        assert!(o.is_engaged());
        assert_eq!(o.current_profile().unwrap().timezone, "Europe/Berlin");
        assert_eq!(s.value("timezone"), "Europe/Berlin");
        let saved = SnapshotStore::new(&s.path).load().unwrap().unwrap();
        assert_eq!(saved.snapshots.len(), 2);
        assert!(saved.engaged_at_unix > 0);
    }

    #[test]
    fn re_engage_keeps_original_snapshots() {
        let s = Setup::new();
        let mut o = s.orch(&[("timezone", OK)]);
        o.engage(&profile("Europe/Berlin")).unwrap();
        s.clear_log();
        o.engage(&profile("Asia/Tokyo")).unwrap();
        assert_eq!(s.log(), vec!["apply:timezone"], "must not re-snapshot spoofed state");
        let saved = SnapshotStore::new(&s.path).load().unwrap().unwrap();
        assert_eq!(saved.profile.timezone, "Asia/Tokyo");
        assert_eq!(saved.snapshots[0].data["value"], "orig-timezone");
        o.disengage().unwrap();
        assert_eq!(s.value("timezone"), "orig-timezone");
    }

    #[test]
    fn stale_instance_never_snapshots_another_runs_spoofed_state() {
        let s = Setup::new();
        // A long-running instance created while nothing was engaged...
        let mut daemon = s.orch(&[("timezone", OK)]);
        assert!(!daemon.is_engaged());
        // ...then a second process engages (true originals saved, system spoofed).
        let mut other = s.orch(&[("timezone", OK)]);
        other.engage(&profile("Asia/Tokyo")).unwrap();
        drop(other);
        assert_eq!(s.value("timezone"), "Asia/Tokyo");
        s.clear_log();

        // The stale instance must adopt the saved state instead of snapshotting.
        daemon.engage(&profile("Europe/Berlin")).unwrap();
        assert_eq!(s.log(), vec!["apply:timezone"], "must not snapshot the spoofed state");
        let saved = SnapshotStore::new(&s.path).load().unwrap().unwrap();
        assert_eq!(saved.snapshots[0].data["value"], "orig-timezone");
        assert_eq!(saved.profile.timezone, "Europe/Berlin");

        assert!(daemon.disengage().unwrap().all_ok());
        assert_eq!(s.value("timezone"), "orig-timezone");
    }

    #[test]
    fn engage_with_corrupt_store_applies_nothing() {
        let s = Setup::new();
        let mut o = s.orch(&[("timezone", OK)]);
        std::fs::create_dir_all(s.path.parent().unwrap()).unwrap();
        std::fs::write(&s.path, b"{ not json").unwrap();
        assert!(matches!(o.engage(&profile("Europe/Berlin")), Err(OrchestratorError::Snapshot(_))));
        assert!(s.log().is_empty());
        assert_eq!(s.value("timezone"), "orig-timezone");
    }

    #[test]
    fn snapshot_failure_skips_apply() {
        let s = Setup::new();
        let bad = Fail { snapshot: true, ..OK };
        let mut o = s.orch(&[("timezone", OK), ("locale", bad)]);
        let r = o.engage(&profile("Europe/Berlin")).unwrap();
        assert!(!r.all_ok());
        assert!(r.outcomes[0].ok);
        assert!(!r.outcomes[1].ok);
        assert!(r.outcomes[1].error.as_deref().unwrap().contains("snapshot failed"));
        assert!(!s.log().contains(&"apply:locale".to_string()));
        assert_eq!(s.value("locale"), "orig-locale");
    }

    #[test]
    fn apply_failure_is_reported_but_snapshot_kept() {
        let s = Setup::new();
        let bad = Fail { apply: true, ..OK };
        let mut o = s.orch(&[("timezone", bad)]);
        let r = o.engage(&profile("Europe/Berlin")).unwrap();
        assert!(!r.all_ok());
        assert!(o.is_engaged());
        assert!(o.disengage().unwrap().all_ok());
    }

    #[test]
    fn disengage_restores_in_reverse_and_clears_store() {
        let s = Setup::new();
        let mut o = s.orch(&[("timezone", OK), ("locale", OK), ("location", OK)]);
        o.engage(&profile("Europe/Berlin")).unwrap();
        s.clear_log();
        let r = o.disengage().unwrap();
        assert!(r.all_ok());
        assert_eq!(
            s.log(),
            vec!["restore:location", "restore:locale", "restore:timezone"]
        );
        assert_eq!(s.value("timezone"), "orig-timezone");
        assert!(!o.is_engaged());
        assert!(!s.path.exists());
        assert!(matches!(o.disengage(), Err(OrchestratorError::NotEngaged)));
    }

    #[test]
    fn partial_restore_failure_keeps_store() {
        let s = Setup::new();
        let bad = Fail { restore: true, ..OK };
        let mut o = s.orch(&[("timezone", OK), ("locale", bad)]);
        o.engage(&profile("Europe/Berlin")).unwrap();
        let r = o.disengage().unwrap();
        assert!(!r.all_ok());
        assert_eq!(s.value("timezone"), "orig-timezone");
        assert!(o.is_engaged());
        assert!(s.path.exists());
        drop(o);
        // Retry with a working backend succeeds and clears.
        let mut o2 = s.orch(&[("timezone", OK), ("locale", OK)]);
        assert!(o2.disengage().unwrap().all_ok());
        assert_eq!(s.value("locale"), "orig-locale");
        assert!(!s.path.exists());
    }

    #[test]
    fn crash_recovery_restores_from_disk() {
        let s = Setup::new();
        {
            let mut o = s.orch(&[("timezone", OK), ("locale", OK)]);
            o.engage(&profile("Europe/Berlin")).unwrap();
            // Simulated crash: orchestrator dropped without disengage.
        }
        assert_eq!(s.value("timezone"), "Europe/Berlin");
        let mut o = s.orch(&[("timezone", OK), ("locale", OK)]);
        assert!(o.is_engaged());
        assert_eq!(o.current_profile().unwrap().timezone, "Europe/Berlin");
        assert!(o.disengage().unwrap().all_ok());
        assert_eq!(s.value("timezone"), "orig-timezone");
        assert_eq!(s.value("locale"), "orig-locale");
    }

    #[test]
    fn missing_backend_on_restore_keeps_store() {
        let s = Setup::new();
        let mut o = s.orch(&[("timezone", OK), ("locale", OK)]);
        o.engage(&profile("Europe/Berlin")).unwrap();
        drop(o);
        let mut o = s.orch(&[("timezone", OK)]);
        let r = o.disengage().unwrap();
        assert!(!r.all_ok());
        assert!(s.path.exists());
    }

    #[test]
    fn verify_reports_per_backend() {
        let s = Setup::new();
        let bad = Fail { apply: true, ..OK };
        let mut o = s.orch(&[("timezone", OK), ("locale", bad)]);
        assert!(o.verify().is_empty());
        o.engage(&profile("Europe/Berlin")).unwrap();
        let v = o.verify();
        assert_eq!(v.len(), 2);
        assert!(v[0].ok);
        assert!(!v[1].ok);
    }
}
