//! Linux timezone backend via `timedatectl` (systemd-timedated).
//!
//! GNOME can set the zone automatically from the location
//! (`org.gnome.desktop.datetime automatic-timezone`), which would silently
//! undo the spoof. While engaged that setting is switched off (best effort,
//! via `gsettings` for the user geoveil runs as) and put back on restore;
//! [`Backend::verify`] reports it when it is still on.

use super::{check_snapshot_owner, run_checked};
use crate::runner::CommandRunner;
use geoveil_core::timezone::{is_safe_zone_name, is_valid_iana};
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "timezone";
const TIMEDATECTL: &str = "timedatectl";
const GSETTINGS: &str = "gsettings";
const GNOME_DATETIME_SCHEMA: &str = "org.gnome.desktop.datetime";
const GNOME_AUTO_TZ_KEY: &str = "automatic-timezone";

/// Saved state: the IANA zone that was configured before engaging, and
/// GNOME's automatic-timezone switch (`None` when unknown / not GNOME).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TimezoneSnapshot {
    timezone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gnome_auto_timezone: Option<bool>,
}

/// Sets the system timezone with `timedatectl set-timezone`.
///
/// Changing the zone requires root or polkit authorization; a polkit refusal
/// is reported as [`BackendError::NeedsPrivilege`].
pub struct TimezoneBackend {
    runner: Arc<dyn CommandRunner>,
}

impl TimezoneBackend {
    /// Create the backend on top of a command runner.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Current zone as reported by `timedatectl show -p Timezone --value`.
    fn current(&self) -> Result<String, BackendError> {
        let out = run_checked(self.runner.as_ref(), NAME, TIMEDATECTL, &["show", "-p", "Timezone", "--value"])?;
        Ok(out.stdout.trim().to_string())
    }

    /// Set a zone that passed validation (always one argument, no shell).
    fn set(&self, zone: &str) -> Result<(), BackendError> {
        run_checked(self.runner.as_ref(), NAME, TIMEDATECTL, &["set-timezone", zone])?;
        Ok(())
    }

    /// GNOME's automatic-timezone switch; `None` if gsettings or the schema
    /// is unavailable (not GNOME, no session bus, ...).
    fn gnome_auto_timezone(&self) -> Option<bool> {
        let out = self
            .runner
            .run(GSETTINGS, &["get", GNOME_DATETIME_SCHEMA, GNOME_AUTO_TZ_KEY])
            .ok()?;
        if !out.success() {
            return None;
        }
        match out.stdout.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    fn set_gnome_auto_timezone(&self, on: bool) -> Result<(), BackendError> {
        let value = if on { "true" } else { "false" };
        run_checked(self.runner.as_ref(), NAME, GSETTINGS, &["set", GNOME_DATETIME_SCHEMA, GNOME_AUTO_TZ_KEY, value])?;
        Ok(())
    }
}

/// Validation for zones read back from the system (snapshot and restore):
/// shape only, so restoring is never stricter than snapshotting.
fn check_saved_zone(zone: &str) -> Result<(), BackendError> {
    if is_safe_zone_name(zone) {
        Ok(())
    } else {
        Err(BackendError::Other {
            backend: NAME.to_string(),
            detail: format!("refusing malformed timezone name `{zone}`"),
        })
    }
}

impl Backend for TimezoneBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let timezone = self.current()?;
        if timezone.is_empty() {
            // Nothing we could restore to with `set-timezone`.
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: "timedatectl reports no configured timezone; cannot snapshot".to_string(),
            });
        }
        // Never save a value restore would refuse: the orchestrator then
        // skips this backend instead of changing what it cannot undo.
        check_saved_zone(&timezone)?;
        let gnome_auto_timezone = self.gnome_auto_timezone();
        let data = serde_json::to_value(TimezoneSnapshot { timezone, gnome_auto_timezone })
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("serialize snapshot: {e}") })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        if !is_valid_iana(&profile.timezone) {
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: format!("refusing to set unknown IANA timezone `{}`", profile.timezone),
            });
        }
        self.set(&profile.timezone)?;
        if self.gnome_auto_timezone() == Some(true) {
            if let Err(e) = self.set_gnome_auto_timezone(false) {
                tracing::warn!(error = %e, "GNOME automatic timezone is on and could not be turned off; it may revert the timezone");
            }
        }
        Ok(())
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        check_snapshot_owner(NAME, &snapshot.backend)?;
        let saved: TimezoneSnapshot = serde_json::from_value(snapshot.data.clone())
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("invalid snapshot data: {e}") })?;
        check_saved_zone(&saved.timezone)?;
        self.set(&saved.timezone)?;
        if saved.gnome_auto_timezone == Some(true) && self.gnome_auto_timezone() != Some(true) {
            self.set_gnome_auto_timezone(true)?;
        }
        Ok(())
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let current = self.current()?;
        let auto_on = self.gnome_auto_timezone() == Some(true);
        let mut detail = format!("system timezone is `{current}`, expected `{}`", profile.timezone);
        if auto_on {
            detail.push_str("; GNOME automatic timezone is ON and may revert it");
        }
        Ok(VerifyReport { backend: NAME.to_string(), ok: current == profile.timezone && !auto_on, detail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;
    use std::io;

    fn profile(tz: &str) -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: tz.to_string(),
            locale: "de-DE".to_string(),
            country: "DE".to_string(),
            source: ProfileSource::Manual,
        }
    }

    fn setup() -> (Arc<FakeRunner>, TimezoneBackend) {
        let fake = Arc::new(FakeRunner::new());
        let backend = TimezoneBackend::new(fake.clone());
        (fake, backend)
    }

    #[test]
    fn snapshot_reads_timezone() {
        let (fake, b) = setup();
        fake.push_stdout("America/New_York\n");
        fake.push_err(io::ErrorKind::NotFound); // no gsettings
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "timezone");
        assert_eq!(snap.data, serde_json::json!({ "timezone": "America/New_York" }));
        assert_eq!(
            fake.calls(),
            vec![
                "timedatectl show -p Timezone --value",
                "gsettings get org.gnome.desktop.datetime automatic-timezone"
            ]
        );
    }

    #[test]
    fn snapshot_accepts_valid_zones_missing_from_table() {
        // Real tzdata names that the CLDR table does not list must snapshot
        // and restore, or the system would stay spoofed forever.
        for zone in ["GMT", "Antarctica/Troll", "Factory", "PST8PDT"] {
            let (fake, b) = setup();
            fake.push_stdout(&format!("{zone}\n"));
            let snap = b.snapshot().unwrap();
            b.restore(&snap).unwrap();
            assert_eq!(fake.calls()[2], format!("timedatectl set-timezone {zone}"), "{zone}");
        }
    }

    #[test]
    fn snapshot_refuses_value_restore_would_refuse() {
        let (fake, b) = setup();
        fake.push_stdout("-evil\n");
        assert!(matches!(b.snapshot(), Err(BackendError::Other { .. })));
    }

    #[test]
    fn gnome_auto_timezone_is_disabled_and_restored() {
        let (fake, b) = setup();
        fake.push_stdout("Europe/London\n");
        fake.push_stdout("true\n");
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "timezone": "Europe/London", "gnome_auto_timezone": true }));

        fake.push_stdout(""); // set-timezone
        fake.push_stdout("true\n"); // gsettings get
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(
            fake.calls()[2..],
            [
                "timedatectl set-timezone Europe/Berlin",
                "gsettings get org.gnome.desktop.datetime automatic-timezone",
                "gsettings set org.gnome.desktop.datetime automatic-timezone false",
            ]
        );

        fake.push_stdout(""); // set-timezone
        fake.push_stdout("false\n"); // gsettings get
        b.restore(&snap).unwrap();
        assert_eq!(
            fake.calls()[5..],
            [
                "timedatectl set-timezone Europe/London",
                "gsettings get org.gnome.desktop.datetime automatic-timezone",
                "gsettings set org.gnome.desktop.datetime automatic-timezone true",
            ]
        );
    }

    #[test]
    fn verify_flags_gnome_auto_timezone() {
        let (fake, b) = setup();
        fake.push_stdout("Europe/Berlin\n");
        fake.push_stdout("true\n");
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(!r.ok);
        assert!(r.detail.contains("automatic timezone"));
    }

    #[test]
    fn snapshot_missing_tool_is_unsupported() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    #[test]
    fn snapshot_empty_zone_is_error() {
        let (fake, b) = setup();
        fake.push_stdout("\n");
        assert!(matches!(b.snapshot(), Err(BackendError::Other { .. })));
    }

    #[test]
    fn apply_sets_timezone() {
        let (fake, b) = setup();
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(fake.calls()[0], "timedatectl set-timezone Europe/Berlin");
    }

    #[test]
    fn apply_rejects_invalid_zone_without_running() {
        let (fake, b) = setup();
        assert!(b.apply(&profile("Not/AZone; rm -rf /")).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn apply_polkit_refusal_needs_privilege() {
        let (fake, b) = setup();
        fake.push(CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Failed to set time zone: Interactive authentication required.".to_string(),
        });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn apply_other_failure_is_command_error() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "boom".to_string() });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::Command { .. })));
    }

    #[test]
    fn restore_sets_saved_zone() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({ "timezone": "Asia/Tokyo" }) };
        b.restore(&snap).unwrap();
        // Old snapshots without the GNOME field leave that setting alone.
        assert_eq!(fake.calls(), vec!["timedatectl set-timezone Asia/Tokyo"]);
    }

    #[test]
    fn restore_rejects_bad_snapshot() {
        let (fake, b) = setup();
        let wrong_owner = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "timezone": "Asia/Tokyo" }) };
        assert!(b.restore(&wrong_owner).is_err());
        let bad_data = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({ "tz": 1 }) };
        assert!(b.restore(&bad_data).is_err());
        let tampered = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({ "timezone": "--help" }) };
        assert!(b.restore(&tampered).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn verify_compares() {
        let (fake, b) = setup();
        fake.push_stdout("Europe/Berlin\n");
        fake.push_stdout("false\n");
        fake.push_stdout("UTC\n");
        fake.push_stdout("false\n");
        assert!(b.verify(&profile("Europe/Berlin")).unwrap().ok);
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(!r.ok);
        assert!(r.detail.contains("UTC"));
    }
}
