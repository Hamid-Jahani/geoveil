//! macOS region/locale backend via `defaults -g AppleLocale`.
//!
//! Only `AppleLocale` (region formats: dates, numbers, currency) is changed.
//! `AppleLanguages` (UI language order) is deliberately left untouched so the
//! user's interface language never changes. Running apps pick up the new
//! locale after restart.
//!
//! `defaults -g` writes the global domain of the user the process runs as.
//! The timezone and location backends need root, so geoveil is often run
//! with `sudo`; writing root's domain would change nothing the user sees
//! (and verify would read root's value back and wrongly report success).
//! When running under `sudo` (`SUDO_USER` / `SUDO_UID` set to a non-root
//! user), every `defaults` call is therefore made as that user:
//! `launchctl asuser <uid> sudo -u <user> defaults ...`.

use super::{check_snapshot_owner, exit_error, is_defaults_missing, run_checked, run_raw};
use crate::runner::CommandRunner;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "locale";
const DEFAULTS: &str = "defaults";
const KEY: &str = "AppleLocale";

/// Saved state: previous `AppleLocale`, `None` if it was unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocaleSnapshot {
    apple_locale: Option<String>,
}

/// Sets the global `AppleLocale` preference.
pub struct LocaleBackend {
    runner: Arc<dyn CommandRunner>,
    /// Desktop user to act for when running as root via `sudo`.
    user: Option<TargetUser>,
}

/// The invoking (non-root) user of a `sudo` session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetUser {
    uid: String,
    name: String,
}

impl TargetUser {
    /// From `SUDO_UID` / `SUDO_USER`, if both are set and sane.
    fn from_env() -> Option<Self> {
        Self::from_vars(std::env::var("SUDO_UID").ok(), std::env::var("SUDO_USER").ok())
    }

    /// Validates the values: uid is a non-zero decimal number, the name is
    /// `[A-Za-z0-9._-]` not starting with `-` (they become arguments).
    pub(crate) fn from_vars(uid: Option<String>, name: Option<String>) -> Option<Self> {
        let uid = uid?.trim().to_string();
        let name = name?.trim().to_string();
        let uid_ok = !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()) && !uid.trim_start_matches('0').is_empty();
        let name_ok = !name.is_empty()
            && name != "root"
            && !name.starts_with('-')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        (uid_ok && name_ok).then_some(Self { uid, name })
    }
}

/// Convert a BCP-47 tag to macOS `AppleLocale` form.
///
/// `"de-DE"` -> `"de_DE"`, `"zh-Hans-CN"` -> `"zh-Hans_CN"`, `"en"` -> `"en"`.
/// Any POSIX encoding/modifier suffix (`.UTF-8`, `@euro`) is dropped.
pub fn mac_locale(bcp47: &str) -> String {
    let base = bcp47.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    let parts: Vec<&str> = base.split('-').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        [lang] => lang.to_ascii_lowercase(),
        [lang, middle @ .., last] if is_region(last) => {
            let mut head = vec![lang.to_ascii_lowercase()];
            head.extend(middle.iter().map(|p| p.to_string()));
            format!("{}_{}", head.join("-"), last.to_ascii_uppercase())
        }
        [lang, rest @ ..] => {
            let mut all = vec![lang.to_ascii_lowercase()];
            all.extend(rest.iter().map(|p| p.to_string()));
            all.join("-")
        }
    }
}

/// ISO-3166 alpha-2 or UN M.49 numeric region subtag.
fn is_region(s: &str) -> bool {
    (s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic())) || (s.len() == 3 && s.chars().all(|c| c.is_ascii_digit()))
}

/// Characters we accept in a value passed to `defaults write`.
fn is_safe_locale_value(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '@' | '=' | ';'))
}

impl LocaleBackend {
    /// Create the backend on top of a command runner. Under `sudo` it acts
    /// for the invoking user (see the module docs).
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_user(runner, TargetUser::from_env())
    }

    /// Create the backend acting for `user` (`None`: the current user).
    pub(crate) fn with_user(runner: Arc<dyn CommandRunner>, user: Option<TargetUser>) -> Self {
        Self { runner, user }
    }

    /// Program and arguments for `defaults <args>` as the target user.
    fn defaults_command<'a>(&'a self, args: &[&'a str]) -> (&'static str, Vec<&'a str>) {
        match &self.user {
            Some(u) => {
                let mut full = vec!["asuser", u.uid.as_str(), "sudo", "-u", u.name.as_str(), DEFAULTS];
                full.extend_from_slice(args);
                ("launchctl", full)
            }
            None => (DEFAULTS, args.to_vec()),
        }
    }

    /// Run `defaults <args>` as the target user, whatever the exit status.
    fn defaults_raw(&self, args: &[&str]) -> Result<crate::runner::CommandOutput, BackendError> {
        let (program, full) = self.defaults_command(args);
        run_raw(self.runner.as_ref(), NAME, program, &full)
    }

    /// Whose preferences are read and written, for messages.
    fn whose(&self) -> String {
        match &self.user {
            Some(u) => format!("user `{}`", u.name),
            None => "the current user".to_string(),
        }
    }

    /// Current `AppleLocale`, `None` if unset.
    fn current(&self) -> Result<Option<String>, BackendError> {
        let args = ["read", "-g", KEY];
        let out = self.defaults_raw(&args)?;
        if out.success() {
            let value = out.stdout.trim();
            Ok(if value.is_empty() { None } else { Some(value.to_string()) })
        } else if is_defaults_missing(&out) {
            Ok(None)
        } else {
            Err(exit_error(NAME, &format!("{DEFAULTS} read -g {KEY}"), &out))
        }
    }

    /// Write `AppleLocale` as a string.
    fn write(&self, value: &str) -> Result<(), BackendError> {
        if !is_safe_locale_value(value) {
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: format!("refusing to write malformed AppleLocale `{value}`"),
            });
        }
        let (program, full) = self.defaults_command(&["write", "-g", KEY, "-string", value]);
        run_checked(self.runner.as_ref(), NAME, program, &full)?;
        Ok(())
    }

    /// Remove `AppleLocale`; already-absent is fine.
    fn delete(&self) -> Result<(), BackendError> {
        let args = ["delete", "-g", KEY];
        let out = self.defaults_raw(&args)?;
        if out.success() || is_defaults_missing(&out) {
            Ok(())
        } else {
            Err(exit_error(NAME, &format!("{DEFAULTS} delete -g {KEY}"), &out))
        }
    }
}

impl Backend for LocaleBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let apple_locale = self.current()?;
        let data = serde_json::to_value(LocaleSnapshot { apple_locale })
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("serialize snapshot: {e}") })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        self.write(&mac_locale(&profile.locale))
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        check_snapshot_owner(NAME, &snapshot.backend)?;
        let saved: LocaleSnapshot = serde_json::from_value(snapshot.data.clone())
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("invalid snapshot data: {e}") })?;
        match saved.apple_locale {
            Some(value) => self.write(&value),
            None => self.delete(),
        }
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let expected = mac_locale(&profile.locale);
        let current = self.current()?;
        let shown = current.as_deref().unwrap_or("(unset)");
        Ok(VerifyReport {
            backend: NAME.to_string(),
            ok: current.as_deref() == Some(expected.as_str()),
            detail: format!(
                "AppleLocale of {} is `{shown}`, expected `{expected}` (AppleLanguages untouched; restart apps to pick it up)",
                self.whose()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;

    const MISSING: &str = "\nThe domain/default pair of (kCFPreferencesAnyApplication, AppleLocale) does not exist\n";

    fn profile(locale: &str) -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: "Europe/Berlin".to_string(),
            locale: locale.to_string(),
            country: "DE".to_string(),
            source: ProfileSource::Manual,
        }
    }

    fn missing() -> CommandOutput {
        CommandOutput { status: 1, stdout: String::new(), stderr: MISSING.to_string() }
    }

    fn setup() -> (Arc<FakeRunner>, LocaleBackend) {
        let fake = Arc::new(FakeRunner::new());
        let backend = LocaleBackend::with_user(fake.clone(), None);
        (fake, backend)
    }

    fn sudo_setup() -> (Arc<FakeRunner>, LocaleBackend) {
        let fake = Arc::new(FakeRunner::new());
        let user = TargetUser::from_vars(Some("501".into()), Some("alice".into()));
        assert!(user.is_some());
        let backend = LocaleBackend::with_user(fake.clone(), user);
        (fake, backend)
    }

    #[test]
    fn under_sudo_acts_for_the_invoking_user() {
        let (fake, b) = sudo_setup();
        fake.push_stdout("en_US\n");
        let snap = b.snapshot().unwrap();
        b.apply(&profile("de-DE")).unwrap();
        fake.push(missing());
        let snap_unset = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "apple_locale": null }) };
        b.restore(&snap_unset).unwrap();
        b.restore(&snap).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                "launchctl asuser 501 sudo -u alice defaults read -g AppleLocale",
                "launchctl asuser 501 sudo -u alice defaults write -g AppleLocale -string de_DE",
                "launchctl asuser 501 sudo -u alice defaults delete -g AppleLocale",
                "launchctl asuser 501 sudo -u alice defaults write -g AppleLocale -string en_US",
            ]
        );
        fake.push_stdout("de_DE\n");
        let r = b.verify(&profile("de-DE")).unwrap();
        assert!(r.ok);
        assert!(r.detail.contains("alice"));
    }

    #[test]
    fn target_user_validation() {
        let tu = |uid: &str, name: &str| TargetUser::from_vars(Some(uid.into()), Some(name.into()));
        assert!(tu("501", "alice").is_some());
        assert!(tu("501", "first.last").is_some());
        assert!(tu("0", "root").is_none());
        assert!(tu("0", "alice").is_none());
        assert!(tu("000", "alice").is_none());
        assert!(tu("501", "root").is_none());
        assert!(tu("501", "-x").is_none());
        assert!(tu("501", "a b").is_none());
        assert!(tu("abc", "alice").is_none());
        assert!(tu("", "alice").is_none());
        assert!(TargetUser::from_vars(None, Some("alice".into())).is_none());
        assert!(TargetUser::from_vars(Some("501".into()), None).is_none());
    }

    #[test]
    fn mac_locale_conversion() {
        assert_eq!(mac_locale("de-DE"), "de_DE");
        assert_eq!(mac_locale("en-us"), "en_US");
        assert_eq!(mac_locale("zh-Hans-CN"), "zh-Hans_CN");
        assert_eq!(mac_locale("es-419"), "es_419");
        assert_eq!(mac_locale("fr"), "fr");
        assert_eq!(mac_locale("de_DE.UTF-8"), "de_DE");
        assert_eq!(mac_locale("sr-Latn"), "sr-Latn");
        assert_eq!(mac_locale(""), "");
    }

    #[test]
    fn safe_value_check() {
        assert!(is_safe_locale_value("de_DE"));
        assert!(is_safe_locale_value("en_US@rg=dezzzz"));
        assert!(!is_safe_locale_value(""));
        assert!(!is_safe_locale_value("-delete"));
        assert!(!is_safe_locale_value("de DE"));
    }

    #[test]
    fn snapshot_reads_value() {
        let (fake, b) = setup();
        fake.push_stdout("en_US\n");
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "apple_locale": "en_US" }));
        assert_eq!(fake.calls(), vec!["defaults read -g AppleLocale"]);
    }

    #[test]
    fn snapshot_unset_is_none() {
        let (fake, b) = setup();
        fake.push(missing());
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "apple_locale": null }));
    }

    #[test]
    fn snapshot_other_failure_is_error() {
        let (fake, b) = setup();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "boom".into() });
        assert!(matches!(b.snapshot(), Err(BackendError::Command { .. })));
    }

    #[test]
    fn apply_writes_mac_form() {
        let (fake, b) = setup();
        b.apply(&profile("de-DE")).unwrap();
        assert_eq!(fake.calls(), vec!["defaults write -g AppleLocale -string de_DE"]);
    }

    #[test]
    fn restore_previous_value() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "apple_locale": "en_US@rg=gbzzzz" }) };
        b.restore(&snap).unwrap();
        assert_eq!(fake.calls(), vec!["defaults write -g AppleLocale -string en_US@rg=gbzzzz"]);
    }

    #[test]
    fn restore_unset_deletes() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "apple_locale": null }) };
        b.restore(&snap).unwrap();
        assert_eq!(fake.calls(), vec!["defaults delete -g AppleLocale"]);
    }

    #[test]
    fn restore_unset_tolerates_already_missing() {
        let (fake, b) = setup();
        fake.push(missing());
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "apple_locale": null }) };
        assert!(b.restore(&snap).is_ok());
    }

    #[test]
    fn verify_compares() {
        let (fake, b) = setup();
        fake.push_stdout("de_DE\n");
        let r = b.verify(&profile("de-DE")).unwrap();
        assert!(r.ok);
        assert!(r.detail.contains("AppleLanguages untouched"));
        fake.push(missing());
        let r = b.verify(&profile("de-DE")).unwrap();
        assert!(!r.ok);
        assert!(r.detail.contains("(unset)"));
    }
}
