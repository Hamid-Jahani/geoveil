//! macOS location backend: **blocks** Location Services, it does not spoof.
//!
//! CoreLocation cannot be fed a fake position without disabling System
//! Integrity Protection, so the honest option is to switch Location Services
//! off while engaged (apps then fall back to IP geolocation, which already
//! points at the VPN exit) and put the previous setting back afterwards.
//!
//! Caveats, surfaced in [`Backend::verify`] output:
//! - needs root (the preference lives under `/var/db/locationd`);
//! - on recent macOS releases SIP may prevent `locationd` from honouring the
//!   change or block the `launchctl kickstart`, so the setting can be written
//!   yet ineffective until reboot, or not at all.

use super::{check_snapshot_owner, exit_error, is_defaults_missing, run_checked, run_raw};
use crate::runner::CommandRunner;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "location";
const DEFAULTS: &str = "defaults";
const LAUNCHCTL: &str = "launchctl";
/// Preference domain (as a plist path) holding the Location Services switch.
const LOCATIOND_DOMAIN: &str = "/var/db/locationd/Library/Preferences/ByHost/com.apple.locationd";
const KEY: &str = "LocationServicesEnabled";
const LOCATIOND_SERVICE: &str = "system/com.apple.locationd";
const CAVEAT: &str = "blocked, not spoofed; may be ineffective on recent macOS with SIP until reboot";

/// Saved state: previous `LocationServicesEnabled`, `None` if unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocationSnapshot {
    enabled: Option<i64>,
}

/// Turns Location Services off while engaged (block only).
pub struct LocationBackend {
    runner: Arc<dyn CommandRunner>,
}

impl LocationBackend {
    /// Create the backend on top of a command runner.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Current value of the switch, `None` if the key is unset.
    fn current(&self) -> Result<Option<i64>, BackendError> {
        let args = ["read", LOCATIOND_DOMAIN, KEY];
        let out = run_raw(self.runner.as_ref(), NAME, DEFAULTS, &args)?;
        if is_defaults_missing(&out) {
            return Ok(None);
        }
        if !out.success() {
            return Err(exit_error(NAME, &format!("{DEFAULTS} read {LOCATIOND_DOMAIN} {KEY}"), &out));
        }
        let raw = out.stdout.trim();
        raw.parse::<i64>().map(Some).map_err(|e| BackendError::Command {
            backend: NAME.to_string(),
            command: format!("{DEFAULTS} read {LOCATIOND_DOMAIN} {KEY}"),
            detail: format!("unexpected value `{raw}`: {e}"),
        })
    }

    /// Write the switch as an integer.
    fn write(&self, value: i64) -> Result<(), BackendError> {
        let value = value.to_string();
        run_checked(self.runner.as_ref(), NAME, DEFAULTS, &["write", LOCATIOND_DOMAIN, KEY, "-int", &value])?;
        Ok(())
    }

    /// Remove the key; already-absent is fine.
    fn delete(&self) -> Result<(), BackendError> {
        let args = ["delete", LOCATIOND_DOMAIN, KEY];
        let out = run_raw(self.runner.as_ref(), NAME, DEFAULTS, &args)?;
        if out.success() || is_defaults_missing(&out) {
            Ok(())
        } else {
            Err(exit_error(NAME, &format!("{DEFAULTS} delete {LOCATIOND_DOMAIN} {KEY}"), &out))
        }
    }

    /// Restart locationd so it re-reads the switch. Non-fatal: SIP often
    /// refuses this on recent macOS; the change then applies after reboot.
    fn kickstart_locationd(&self) {
        match self.runner.run(LAUNCHCTL, &["kickstart", "-k", LOCATIOND_SERVICE]) {
            Ok(out) if out.success() => {}
            Ok(out) => tracing::warn!(
                status = out.status,
                stderr = out.stderr.trim(),
                "could not restart locationd; the Location Services change may only apply after reboot"
            ),
            Err(e) => tracing::warn!(error = %e, "could not run launchctl to restart locationd"),
        }
    }
}

impl Backend for LocationBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let enabled = self.current()?;
        let data = serde_json::to_value(LocationSnapshot { enabled })
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("serialize snapshot: {e}") })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    /// Disables Location Services; the profile coordinates are not used.
    fn apply(&self, _profile: &Profile) -> Result<(), BackendError> {
        self.write(0)?;
        self.kickstart_locationd();
        Ok(())
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        check_snapshot_owner(NAME, &snapshot.backend)?;
        let saved: LocationSnapshot = serde_json::from_value(snapshot.data.clone())
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("invalid snapshot data: {e}") })?;
        match saved.enabled {
            Some(value) => self.write(value)?,
            None => self.delete()?,
        }
        self.kickstart_locationd();
        Ok(())
    }

    fn verify(&self, _profile: &Profile) -> Result<VerifyReport, BackendError> {
        let current = self.current()?;
        let (ok, state) = match current {
            Some(0) => (true, "Location Services disabled (LocationServicesEnabled=0)".to_string()),
            Some(v) => (false, format!("Location Services still enabled (LocationServicesEnabled={v})")),
            None => (false, "LocationServicesEnabled is unset (Location Services likely enabled)".to_string()),
        };
        Ok(VerifyReport { backend: NAME.to_string(), ok, detail: format!("{state}; {CAVEAT}") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;
    use std::io;

    fn profile() -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: "Europe/Berlin".to_string(),
            locale: "de-DE".to_string(),
            country: "DE".to_string(),
            source: ProfileSource::Manual,
        }
    }

    fn missing() -> CommandOutput {
        CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: format!("The domain/default pair of ({LOCATIOND_DOMAIN}, {KEY}) does not exist"),
        }
    }

    fn setup() -> (Arc<FakeRunner>, LocationBackend) {
        let fake = Arc::new(FakeRunner::new());
        let backend = LocationBackend::new(fake.clone());
        (fake, backend)
    }

    fn read_call() -> String {
        format!("defaults read {LOCATIOND_DOMAIN} {KEY}")
    }

    #[test]
    fn snapshot_reads_value() {
        let (fake, b) = setup();
        fake.push_stdout("1\n");
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "location");
        assert_eq!(snap.data, serde_json::json!({ "enabled": 1 }));
        assert_eq!(fake.calls(), vec![read_call()]);
    }

    #[test]
    fn snapshot_unset_is_none() {
        let (fake, b) = setup();
        fake.push(missing());
        assert_eq!(b.snapshot().unwrap().data, serde_json::json!({ "enabled": null }));
    }

    #[test]
    fn snapshot_permission_and_garbage() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "Permission denied".into() });
        assert!(matches!(b.snapshot(), Err(BackendError::NeedsPrivilege { .. })));
        fake.push_stdout("yes\n");
        assert!(matches!(b.snapshot(), Err(BackendError::Command { .. })));
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    #[test]
    fn apply_disables_and_kickstarts() {
        let (fake, b) = setup();
        b.apply(&profile()).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                format!("defaults write {LOCATIOND_DOMAIN} {KEY} -int 0"),
                "launchctl kickstart -k system/com.apple.locationd".to_string(),
            ]
        );
    }

    #[test]
    fn apply_tolerates_kickstart_failure() {
        let (fake, b) = setup();
        fake.push_stdout("");
        fake.push(CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Could not kickstart service: Operation not permitted while System Integrity Protection is engaged".into(),
        });
        assert!(b.apply(&profile()).is_ok());
    }

    #[test]
    fn apply_write_permission_denied_needs_privilege() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "Could not write domain; Permission denied".into() });
        assert!(matches!(b.apply(&profile()), Err(BackendError::NeedsPrivilege { .. })));
        assert_eq!(fake.calls().len(), 1, "no kickstart after failed write");
    }

    #[test]
    fn restore_previous_value() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "location".into(), data: serde_json::json!({ "enabled": 1 }) };
        b.restore(&snap).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                format!("defaults write {LOCATIOND_DOMAIN} {KEY} -int 1"),
                "launchctl kickstart -k system/com.apple.locationd".to_string(),
            ]
        );
    }

    #[test]
    fn restore_unset_deletes_key() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "location".into(), data: serde_json::json!({ "enabled": null }) };
        fake.push(missing()); // already absent: fine
        b.restore(&snap).unwrap();
        assert_eq!(fake.calls()[0], format!("defaults delete {LOCATIOND_DOMAIN} {KEY}"));
    }

    #[test]
    fn restore_rejects_bad_snapshot() {
        let (fake, b) = setup();
        let wrong = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "enabled": 1 }) };
        assert!(b.restore(&wrong).is_err());
        let bad = BackendSnapshot { backend: "location".into(), data: serde_json::json!({ "enabled": "x" }) };
        assert!(b.restore(&bad).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn verify_reports_honestly() {
        let (fake, b) = setup();
        fake.push_stdout("0\n");
        let r = b.verify(&profile()).unwrap();
        assert!(r.ok);
        assert!(r.detail.contains("blocked, not spoofed"));
        assert!(r.detail.contains("SIP"));

        fake.push_stdout("1\n");
        assert!(!b.verify(&profile()).unwrap().ok);
        fake.push(missing());
        assert!(!b.verify(&profile()).unwrap().ok);
    }
}
