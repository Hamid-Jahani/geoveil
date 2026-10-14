//! Windows system time zone via `tzutil`.
//!
//! Snapshot data shape:
//! `{"windows_id": "W. Europe Standard Time", "tzautoupdate_start": 3}`. The id
//! is stored exactly as `tzutil /g` printed it (minus surrounding whitespace),
//! including a `_dstoff` suffix when daylight saving adjustment was disabled,
//! so `tzutil /s` restores the exact original setting.
//!
//! "Set time zone automatically" is the `tzautoupdate` service: its `Start`
//! value is 3 when the setting is on and 4 when it is off. When on, Windows
//! puts the real zone back on the next location trigger, so it is switched
//! off while engaged (best effort: this needs admin rights, while `tzutil /s`
//! does not) and its original `Start` is restored afterwards.
//! [`Backend::verify`] reports it when it is still on.

use super::{classify_failure, run_checked};
use crate::runner::CommandRunner;
use geoveil_core::timezone::iana_to_windows;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "timezone";
const TZUTIL: &str = "tzutil";
/// Suffix `tzutil` appends when DST adjustment is turned off for the zone.
const DST_OFF_SUFFIX: &str = "_dstoff";
const REG: &str = "reg";
const TZAUTOUPDATE_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\tzautoupdate";
/// `Start` value of a disabled service (automatic time zone off).
const SERVICE_DISABLED: u32 = 4;

/// Sets the Windows system time zone to match the profile's IANA zone.
pub struct TimezoneBackend {
    runner: Arc<dyn CommandRunner>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotData {
    windows_id: String,
    /// `Start` of the `tzautoupdate` service; `None` when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tzautoupdate_start: Option<u32>,
}

impl TimezoneBackend {
    /// Creates the backend using `runner` to execute `tzutil`.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Reads the current Windows zone id (`tzutil /g`), trimmed, suffix kept.
    fn current_id(&self) -> Result<String, BackendError> {
        let out = run_checked(self.runner.as_ref(), NAME, "tzutil /g", TZUTIL, &["/g"])?;
        let id = out.stdout.trim().to_string();
        if id.is_empty() {
            return Err(BackendError::Command {
                backend: NAME.to_string(),
                command: "tzutil /g".to_string(),
                detail: "empty output".to_string(),
            });
        }
        Ok(id)
    }

    /// Sets the Windows zone id (`tzutil /s <id>`), passed as one argument.
    fn set_id(&self, id: &str) -> Result<(), BackendError> {
        let display = format!("tzutil /s \"{id}\"");
        let out = run_checked(self.runner.as_ref(), NAME, &display, TZUTIL, &["/s", id])?;
        // tzutil has been seen to print "Error: ..." while still exiting 0.
        if out.stdout.trim_start().to_ascii_lowercase().starts_with("error") {
            return Err(classify_failure(NAME, &display, &out));
        }
        Ok(())
    }

    /// `Start` of the `tzautoupdate` service, `None` if it cannot be read.
    fn tzautoupdate_start(&self) -> Option<u32> {
        let out = self.runner.run(REG, &["query", TZAUTOUPDATE_KEY, "/v", "Start"]).ok()?;
        if !out.success() {
            return None;
        }
        parse_reg_dword(&out.stdout)
    }

    fn set_tzautoupdate_start(&self, start: u32) -> Result<(), BackendError> {
        let value = start.to_string();
        let display = format!("reg add {TZAUTOUPDATE_KEY} /v Start /d {value}");
        run_checked(
            self.runner.as_ref(),
            NAME,
            &display,
            REG,
            &["add", TZAUTOUPDATE_KEY, "/v", "Start", "/t", "REG_DWORD", "/d", &value, "/f"],
        )?;
        Ok(())
    }
}

/// True when the `tzautoupdate` service may run (automatic time zone on).
fn auto_update_on(start: Option<u32>) -> bool {
    start.is_some_and(|s| s != SERVICE_DISABLED)
}

/// Parses the `REG_DWORD` value from `reg query ... /v <name>` output:
/// `    Start    REG_DWORD    0x3`.
fn parse_reg_dword(stdout: &str) -> Option<u32> {
    let line = stdout.lines().find(|l| l.contains("REG_DWORD"))?;
    let raw = line.split_whitespace().last()?;
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => raw.parse().ok(),
    }
}

/// Strips a trailing `_dstoff` marker for comparison purposes.
fn base_id(id: &str) -> &str {
    id.strip_suffix(DST_OFF_SUFFIX).unwrap_or(id)
}

/// Maps the profile's IANA zone to a Windows id or explains why it cannot.
fn windows_id_for(profile: &Profile) -> Result<&'static str, BackendError> {
    iana_to_windows(&profile.timezone).ok_or_else(|| BackendError::Other {
        backend: NAME.to_string(),
        detail: format!(
            "no Windows time zone mapping for IANA zone `{}` (CLDR windowsZones has no entry)",
            profile.timezone
        ),
    })
}

impl Backend for TimezoneBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let windows_id = self.current_id()?;
        let tzautoupdate_start = self.tzautoupdate_start();
        let data = serde_json::to_value(SnapshotData { windows_id, tzautoupdate_start }).map_err(|e| BackendError::Other {
            backend: NAME.to_string(),
            detail: format!("cannot serialize snapshot: {e}"),
        })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        let id = windows_id_for(profile)?;
        self.set_id(id)?;
        if auto_update_on(self.tzautoupdate_start()) {
            if let Err(e) = self.set_tzautoupdate_start(SERVICE_DISABLED) {
                tracing::warn!(
                    error = %e,
                    "\"Set time zone automatically\" is on and could not be turned off (needs admin); Windows may revert the time zone"
                );
            }
        }
        Ok(())
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        let data: SnapshotData = serde_json::from_value(snapshot.data.clone()).map_err(|e| BackendError::Other {
            backend: NAME.to_string(),
            detail: format!("invalid timezone snapshot: {e}"),
        })?;
        let id = data.windows_id.trim();
        if id.is_empty() {
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: "timezone snapshot has an empty Windows zone id".to_string(),
            });
        }
        self.set_id(id)?;
        if let Some(start) = data.tzautoupdate_start {
            if self.tzautoupdate_start() != Some(start) {
                self.set_tzautoupdate_start(start)?;
            }
        }
        Ok(())
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let current = self.current_id()?;
        let mut report = match iana_to_windows(&profile.timezone) {
            Some(expected) => {
                let ok = base_id(&current).eq_ignore_ascii_case(expected);
                VerifyReport {
                    backend: NAME.to_string(),
                    ok,
                    detail: format!("current `{current}`, expected `{expected}` (from {})", profile.timezone),
                }
            }
            None => VerifyReport {
                backend: NAME.to_string(),
                ok: false,
                detail: format!("current `{current}`, no Windows mapping for `{}`", profile.timezone),
            },
        };
        if auto_update_on(self.tzautoupdate_start()) {
            report.ok = false;
            report.detail.push_str("; \"Set time zone automatically\" is ON and may revert it");
        }
        Ok(report)
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

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn snapshot_trims_crlf() {
        let (fake, b) = setup();
        fake.push_stdout("Pacific Standard Time\r\n");
        fake.push_stdout(REG_QUERY_OFF);
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "timezone");
        assert_eq!(snap.data, serde_json::json!({"windows_id": "Pacific Standard Time", "tzautoupdate_start": 4}));
        assert_eq!(
            fake.raw_calls(),
            vec![
                ("tzutil".to_string(), args(&["/g"])),
                ("reg".to_string(), args(&["query", TZAUTOUPDATE_KEY, "/v", "Start"])),
            ]
        );
    }

    const REG_QUERY_ON: &str = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\tzautoupdate\r\n    Start    REG_DWORD    0x3\r\n\r\n";
    const REG_QUERY_OFF: &str = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\tzautoupdate\r\n    Start    REG_DWORD    0x4\r\n\r\n";

    #[test]
    fn parses_reg_dword() {
        assert_eq!(parse_reg_dword(REG_QUERY_ON), Some(3));
        assert_eq!(parse_reg_dword(REG_QUERY_OFF), Some(4));
        assert_eq!(parse_reg_dword("    Start    REG_DWORD    0x1f"), Some(31));
        assert_eq!(parse_reg_dword("ERROR: The system was unable to find the specified registry key or value."), None);
        assert_eq!(parse_reg_dword(""), None);
    }

    #[test]
    fn automatic_time_zone_is_disabled_and_restored() {
        let (fake, b) = setup();
        fake.push_stdout("GMT Standard Time\r\n");
        fake.push_stdout(REG_QUERY_ON);
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data["tzautoupdate_start"], 3);

        fake.push_stdout(""); // tzutil /s
        fake.push_stdout(REG_QUERY_ON);
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(
            fake.raw_calls()[4],
            ("reg".to_string(), args(&["add", TZAUTOUPDATE_KEY, "/v", "Start", "/t", "REG_DWORD", "/d", "4", "/f"]))
        );

        fake.push_stdout(""); // tzutil /s
        fake.push_stdout(REG_QUERY_OFF);
        b.restore(&snap).unwrap();
        let calls = fake.raw_calls();
        assert_eq!(calls[5], ("tzutil".to_string(), args(&["/s", "GMT Standard Time"])));
        assert_eq!(
            calls[7],
            ("reg".to_string(), args(&["add", TZAUTOUPDATE_KEY, "/v", "Start", "/t", "REG_DWORD", "/d", "3", "/f"]))
        );
    }

    #[test]
    fn apply_succeeds_when_auto_update_cannot_be_disabled() {
        let (fake, b) = setup();
        fake.push_stdout(""); // tzutil /s
        fake.push_stdout(REG_QUERY_ON);
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "ERROR: Access is denied.".into() });
        assert!(b.apply(&profile("Europe/Berlin")).is_ok());
    }

    #[test]
    fn restore_reports_failure_to_reenable_auto_update() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "timezone".into(),
            data: serde_json::json!({"windows_id": "GMT Standard Time", "tzautoupdate_start": 3}),
        };
        fake.push_stdout("");
        fake.push_stdout(REG_QUERY_OFF);
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "ERROR: Access is denied.".into() });
        assert!(matches!(b.restore(&snap), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn verify_flags_automatic_time_zone() {
        let (fake, b) = setup();
        fake.push_stdout("W. Europe Standard Time\r\n");
        fake.push_stdout(REG_QUERY_ON);
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(!r.ok);
        assert!(r.detail.contains("automatically"));
    }

    #[test]
    fn snapshot_preserves_dstoff_and_restore_uses_it() {
        let (fake, b) = setup();
        fake.push_stdout("Pacific Standard Time_dstoff\r\n");
        fake.push_err(io::ErrorKind::NotFound); // reg unavailable
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({"windows_id": "Pacific Standard Time_dstoff"}));
        b.restore(&snap).unwrap();
        assert_eq!(
            fake.raw_calls()[2],
            ("tzutil".to_string(), args(&["/s", "Pacific Standard Time_dstoff"]))
        );
    }

    #[test]
    fn apply_maps_iana_to_single_arg() {
        let (fake, b) = setup();
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(fake.raw_calls()[0], ("tzutil".to_string(), args(&["/s", "W. Europe Standard Time"])));
    }

    #[test]
    fn apply_unknown_zone_is_other_and_runs_nothing() {
        let (fake, b) = setup();
        let err = b.apply(&profile("Not/AZone")).unwrap_err();
        match err {
            BackendError::Other { detail, .. } => assert!(detail.contains("Not/AZone")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn access_denied_maps_to_needs_privilege() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 5, stdout: "Error: Access is denied.\r\n".into(), stderr: String::new() });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn error_text_with_zero_exit_is_failure() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 0, stdout: "Error: Access is denied.\r\n".into(), stderr: String::new() });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn other_failure_maps_to_command() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 1, stdout: "Error: Invalid time zone identifier".into(), stderr: String::new() });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::Command { .. })));
    }

    #[test]
    fn missing_tzutil_is_unsupported() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    #[test]
    fn empty_output_is_error() {
        let (fake, b) = setup();
        fake.push_stdout("\r\n");
        assert!(b.snapshot().is_err());
    }

    #[test]
    fn verify_ignores_dstoff() {
        let (fake, b) = setup();
        fake.push_stdout("W. Europe Standard Time_dstoff\r\n");
        fake.push_stdout(REG_QUERY_OFF);
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(r.ok, "{}", r.detail);
        fake.push_stdout("Pacific Standard Time\r\n");
        fake.push_stdout(REG_QUERY_OFF);
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(!r.ok);
    }

    #[test]
    fn restore_rejects_bad_snapshot() {
        let (fake, b) = setup();
        let bad = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({"nope": 1}) };
        assert!(matches!(b.restore(&bad), Err(BackendError::Other { .. })));
        let empty = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({"windows_id": " "}) };
        assert!(matches!(b.restore(&empty), Err(BackendError::Other { .. })));
        assert!(fake.calls().is_empty());
    }
}
