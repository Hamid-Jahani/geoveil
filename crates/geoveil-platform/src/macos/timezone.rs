//! macOS timezone backend via `systemsetup` (requires root).
//!
//! "Set time zone automatically using your current location"
//! (`/Library/Preferences/com.apple.timezone.auto` `Active`) would silently
//! undo the spoof, so it is switched off while engaged (best effort) and put
//! back on restore; [`Backend::verify`] reports it when it may still be on.

use super::{check_snapshot_owner, exit_error, is_defaults_missing, run_checked, run_raw};
use crate::runner::CommandRunner;
use geoveil_core::timezone::{is_safe_zone_name, is_valid_iana};
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "timezone";
const SYSTEMSETUP: &str = "systemsetup";
const TIME_ZONE_LABEL: &str = "Time Zone:";
const DEFAULTS: &str = "defaults";
const AUTO_TZ_DOMAIN: &str = "/Library/Preferences/com.apple.timezone.auto";
const AUTO_TZ_KEY: &str = "Active";

/// State of the automatic-timezone preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AutoTimezone {
    /// Could not be read; never touched.
    #[default]
    Unknown,
    /// Key not set (system default, which is "on").
    Absent,
    /// Explicitly set.
    Set(bool),
}

impl AutoTimezone {
    /// Whether macOS may change the zone on its own.
    fn may_be_on(self) -> bool {
        matches!(self, AutoTimezone::Absent | AutoTimezone::Set(true))
    }
}

/// Saved state: the zone configured before engaging and the
/// automatic-timezone preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TimezoneSnapshot {
    timezone: String,
    #[serde(default, skip_serializing_if = "is_unknown")]
    auto_timezone: AutoTimezone,
}

fn is_unknown(a: &AutoTimezone) -> bool {
    *a == AutoTimezone::Unknown
}

/// Sets the system timezone with `systemsetup -settimezone`.
///
/// `systemsetup` needs administrator rights; without them it prints
/// "You need administrator access" which maps to
/// [`BackendError::NeedsPrivilege`].
pub struct TimezoneBackend {
    runner: Arc<dyn CommandRunner>,
}

impl TimezoneBackend {
    /// Create the backend on top of a command runner.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Current zone from `systemsetup -gettimezone`.
    fn current(&self) -> Result<String, BackendError> {
        let out = run_checked(self.runner.as_ref(), NAME, SYSTEMSETUP, &["-gettimezone"])?;
        parse_gettimezone(&out.stdout).ok_or_else(|| BackendError::Command {
            backend: NAME.to_string(),
            command: format!("{SYSTEMSETUP} -gettimezone"),
            detail: format!("unexpected output: {}", out.stdout.trim()),
        })
    }

    /// Set a zone that passed validation (always one argument, no shell).
    fn set(&self, zone: &str) -> Result<(), BackendError> {
        run_checked(self.runner.as_ref(), NAME, SYSTEMSETUP, &["-settimezone", zone])?;
        Ok(())
    }

    /// Current automatic-timezone preference.
    fn auto_timezone(&self) -> AutoTimezone {
        let out = match self.runner.run(DEFAULTS, &["read", AUTO_TZ_DOMAIN, AUTO_TZ_KEY]) {
            Ok(out) => out,
            Err(_) => return AutoTimezone::Unknown,
        };
        if is_defaults_missing(&out) {
            return AutoTimezone::Absent;
        }
        if !out.success() {
            return AutoTimezone::Unknown;
        }
        match out.stdout.trim() {
            "1" | "true" | "YES" => AutoTimezone::Set(true),
            "0" | "false" | "NO" => AutoTimezone::Set(false),
            _ => AutoTimezone::Unknown,
        }
    }

    fn write_auto_timezone(&self, on: bool) -> Result<(), BackendError> {
        let value = if on { "true" } else { "false" };
        run_checked(self.runner.as_ref(), NAME, DEFAULTS, &["write", AUTO_TZ_DOMAIN, AUTO_TZ_KEY, "-bool", value])?;
        Ok(())
    }

    fn delete_auto_timezone(&self) -> Result<(), BackendError> {
        let args = ["delete", AUTO_TZ_DOMAIN, AUTO_TZ_KEY];
        let out = run_raw(self.runner.as_ref(), NAME, DEFAULTS, &args)?;
        if out.success() || is_defaults_missing(&out) {
            Ok(())
        } else {
            Err(exit_error(NAME, &format!("{DEFAULTS} delete {AUTO_TZ_DOMAIN} {AUTO_TZ_KEY}"), &out))
        }
    }

    /// Put the automatic-timezone preference back to `saved`.
    fn restore_auto_timezone(&self, saved: AutoTimezone) -> Result<(), BackendError> {
        if saved == AutoTimezone::Unknown || self.auto_timezone() == saved {
            return Ok(());
        }
        match saved {
            AutoTimezone::Set(on) => self.write_auto_timezone(on),
            AutoTimezone::Absent => self.delete_auto_timezone(),
            AutoTimezone::Unknown => Ok(()),
        }
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

/// Parse "Time Zone: Europe/Berlin" into "Europe/Berlin".
fn parse_gettimezone(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix(TIME_ZONE_LABEL))
        .map(str::trim)
        .filter(|z| !z.is_empty())
        .map(str::to_string)
}

impl Backend for TimezoneBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let timezone = self.current()?;
        // Never save a value restore would refuse (e.g. `GMT` is common on
        // macOS and must round-trip).
        check_saved_zone(&timezone)?;
        let auto_timezone = self.auto_timezone();
        let data = serde_json::to_value(TimezoneSnapshot { timezone, auto_timezone })
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
        if self.auto_timezone().may_be_on() {
            if let Err(e) = self.write_auto_timezone(false) {
                tracing::warn!(error = %e, "automatic time zone may be on and could not be turned off; macOS may revert the timezone");
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
        self.restore_auto_timezone(saved.auto_timezone)
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let current = self.current()?;
        let auto_on = self.auto_timezone().may_be_on();
        let mut detail = format!("system timezone is `{current}`, expected `{}`", profile.timezone);
        if auto_on {
            detail.push_str("; automatic time zone may be ON and revert it");
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
    fn parses_gettimezone() {
        assert_eq!(parse_gettimezone("Time Zone: Europe/Berlin\n"), Some("Europe/Berlin".to_string()));
        assert_eq!(parse_gettimezone("  Time Zone:   America/New_York  "), Some("America/New_York".to_string()));
        assert_eq!(parse_gettimezone("Time Zone: \n"), None);
        assert_eq!(parse_gettimezone("garbage"), None);
    }

    fn auto_missing() -> CommandOutput {
        CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "The domain/default pair of (/Library/Preferences/com.apple.timezone.auto, Active) does not exist".into(),
        }
    }

    #[test]
    fn snapshot_reads_zone() {
        let (fake, b) = setup();
        fake.push_stdout("Time Zone: Europe/Berlin\n");
        fake.push_stdout("0\n");
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "timezone": "Europe/Berlin", "auto_timezone": { "set": false } }));
        assert_eq!(
            fake.calls(),
            vec!["systemsetup -gettimezone", "defaults read /Library/Preferences/com.apple.timezone.auto Active"]
        );
    }

    #[test]
    fn gmt_round_trips() {
        let (fake, b) = setup();
        fake.push_stdout("Time Zone: GMT\n");
        fake.push_err(io::ErrorKind::NotFound);
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "timezone": "GMT" }));
        b.restore(&snap).unwrap();
        assert_eq!(fake.calls()[2], "systemsetup -settimezone GMT");
    }

    #[test]
    fn auto_timezone_disabled_and_restored_to_absent() {
        let (fake, b) = setup();
        fake.push_stdout("Time Zone: Europe/London\n");
        fake.push(auto_missing());
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data["auto_timezone"], "absent");

        fake.push_stdout(""); // settimezone
        fake.push(auto_missing()); // still default (on)
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(
            fake.calls()[4],
            "defaults write /Library/Preferences/com.apple.timezone.auto Active -bool false"
        );

        fake.push_stdout(""); // settimezone
        fake.push_stdout("0\n"); // we turned it off
        b.restore(&snap).unwrap();
        assert_eq!(
            fake.calls()[5..],
            [
                "systemsetup -settimezone Europe/London",
                "defaults read /Library/Preferences/com.apple.timezone.auto Active",
                "defaults delete /Library/Preferences/com.apple.timezone.auto Active",
            ]
        );
    }

    #[test]
    fn auto_timezone_restored_to_on() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "timezone".into(),
            data: serde_json::json!({ "timezone": "Europe/London", "auto_timezone": { "set": true } }),
        };
        fake.push_stdout("");
        fake.push_stdout("0\n");
        b.restore(&snap).unwrap();
        assert_eq!(fake.calls()[2], "defaults write /Library/Preferences/com.apple.timezone.auto Active -bool true");
    }

    #[test]
    fn snapshot_unexpected_output_is_error() {
        let (fake, b) = setup();
        fake.push_stdout("huh\n");
        assert!(matches!(b.snapshot(), Err(BackendError::Command { .. })));
    }

    #[test]
    fn snapshot_without_admin_needs_privilege() {
        let (fake, b) = setup();
        fake.push_stdout("You need administrator access to run this tool... exiting!\n");
        assert!(matches!(b.snapshot(), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn snapshot_missing_tool_is_unsupported() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    #[test]
    fn apply_sets_zone() {
        let (fake, b) = setup();
        fake.push_stdout("");
        fake.push_stdout("0\n");
        b.apply(&profile("Europe/Berlin")).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                "systemsetup -settimezone Europe/Berlin",
                "defaults read /Library/Preferences/com.apple.timezone.auto Active"
            ]
        );
    }

    #[test]
    fn apply_rejects_invalid_zone() {
        let (fake, b) = setup();
        assert!(b.apply(&profile("Mars/Olympus_Mons")).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn apply_without_admin_needs_privilege() {
        let (fake, b) = setup();
        fake.push(CommandOutput {
            status: 0,
            stdout: "You need administrator access to run this tool... exiting!\n".into(),
            stderr: String::new(),
        });
        assert!(matches!(b.apply(&profile("Europe/Berlin")), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn restore_sets_saved_zone() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({ "timezone": "Asia/Tokyo" }) };
        b.restore(&snap).unwrap();
        // Old snapshots (no auto_timezone field) leave the preference alone.
        assert_eq!(fake.calls(), vec!["systemsetup -settimezone Asia/Tokyo"]);
        let tampered = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({ "timezone": "-x" }) };
        assert!(b.restore(&tampered).is_err());
    }

    #[test]
    fn verify_compares() {
        let (fake, b) = setup();
        fake.push_stdout("Time Zone: Europe/Berlin\n");
        fake.push_stdout("0\n");
        fake.push_stdout("Time Zone: Europe/Paris\n");
        fake.push_stdout("0\n");
        fake.push_stdout("Time Zone: Europe/Berlin\n");
        fake.push_stdout("1\n");
        assert!(b.verify(&profile("Europe/Berlin")).unwrap().ok);
        assert!(!b.verify(&profile("Europe/Berlin")).unwrap().ok);
        let r = b.verify(&profile("Europe/Berlin")).unwrap();
        assert!(!r.ok);
        assert!(r.detail.contains("automatic time zone"));
    }
}
