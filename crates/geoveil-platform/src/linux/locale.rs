//! Linux system locale backend via `localectl` (systemd-localed).
//!
//! The system locale only affects sessions started after the change; running
//! desktop sessions and processes keep their environment.
//!
//! systemd-localed refuses locales that are not generated ("Locale ... not
//! installed, refusing"), and minimal installs often only have `en_US.UTF-8`.
//! Apply therefore checks `locale -a` first and reports a missing locale as
//! [`BackendError::Unsupported`] with the command that generates it.

use super::{check_snapshot_owner, run_checked};
use crate::runner::CommandRunner;
use geoveil_core::locale::posix_locale;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAME: &str = "locale";
const LOCALECTL: &str = "localectl";
const LOCALE: &str = "locale";
const SYSTEM_LOCALE_LABEL: &str = "System Locale:";

/// Saved state: every `VAR=value` assignment of the system locale, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocaleSnapshot {
    vars: Vec<String>,
}

/// Sets the system locale (`LANG`) with `localectl set-locale`.
pub struct LocaleBackend {
    runner: Arc<dyn CommandRunner>,
}

impl LocaleBackend {
    /// Create the backend on top of a command runner.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Current system locale assignments from `localectl status`.
    fn current(&self) -> Result<Vec<String>, BackendError> {
        let out = run_checked(self.runner.as_ref(), NAME, LOCALECTL, &["status"])?;
        Ok(parse_system_locale(&out.stdout))
    }

    /// Whether `posix` (e.g. `de_DE.UTF-8`) is listed by `locale -a`;
    /// `None` when that cannot be determined (tool missing or failing).
    fn is_installed(&self, posix: &str) -> Option<bool> {
        let out = self.runner.run(LOCALE, &["-a"]).ok()?;
        if !out.success() {
            return None;
        }
        let want = normalize_locale_name(posix);
        Some(out.stdout.lines().any(|l| normalize_locale_name(l.trim()) == want))
    }

    /// Run `localectl set-locale` with the given assignments.
    fn set(&self, vars: &[String]) -> Result<(), BackendError> {
        if let Some(bad) = vars.iter().find(|v| !is_assignment(v)) {
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: format!("refusing to pass malformed locale assignment `{bad}`"),
            });
        }
        let mut args = vec!["set-locale"];
        args.extend(vars.iter().map(String::as_str));
        run_checked(self.runner.as_ref(), NAME, LOCALECTL, &args)?;
        Ok(())
    }
}

/// True for `VAR=value` where VAR is `[A-Z_]+` and value has no whitespace.
fn is_assignment(s: &str) -> bool {
    match s.split_once('=') {
        Some((var, value)) => {
            !var.is_empty()
                && var.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                && !value.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// Canonical form for comparing locale names: case-insensitive, and glibc's
/// `utf8` codeset spelling equals `UTF-8` (`de_DE.UTF-8` == `de_DE.utf8`).
fn normalize_locale_name(name: &str) -> String {
    name.to_ascii_lowercase().replace("utf-8", "utf8")
}

/// Error for a locale that `localectl` would refuse because it is not generated.
fn not_installed(posix: &str) -> BackendError {
    let base = posix.split('.').next().unwrap_or(posix);
    BackendError::Unsupported {
        backend: NAME.to_string(),
        detail: format!(
            "locale `{posix}` is not installed (not listed by `locale -a`); generate it first, e.g. \
             `sudo locale-gen {posix}` (Debian/Ubuntu) or `sudo localedef -i {base} -f UTF-8 {posix}`"
        ),
    }
}

/// Parse the "System Locale:" block of `localectl status`.
///
/// ```text
///    System Locale: LANG=en_US.UTF-8
///                   LC_TIME=de_DE.UTF-8
///        VC Keymap: us
/// ```
/// Continuation lines start with whitespace and hold one `VAR=value` each.
/// "n/a" (no locale configured) yields an empty list.
fn parse_system_locale(status: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut lines = status.lines();
    for line in lines.by_ref() {
        if let Some(rest) = line.trim().strip_prefix(SYSTEM_LOCALE_LABEL) {
            let first = rest.trim();
            if is_assignment(first) {
                vars.push(first.to_string());
            }
            break;
        }
    }
    for line in lines {
        let starts_indented = line.starts_with(char::is_whitespace);
        let candidate = line.trim();
        if !starts_indented || !is_assignment(candidate) {
            break;
        }
        vars.push(candidate.to_string());
    }
    vars
}

/// Value of `LANG` among assignments, if any.
fn lang_of(vars: &[String]) -> Option<&str> {
    vars.iter().find_map(|v| v.strip_prefix("LANG="))
}

impl Backend for LocaleBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let vars = self.current()?;
        if vars.is_empty() {
            // `localectl set-locale` cannot portably restore "unset".
            return Err(BackendError::Other {
                backend: NAME.to_string(),
                detail: "no system locale configured (localectl reports n/a); cannot snapshot".to_string(),
            });
        }
        let data = serde_json::to_value(LocaleSnapshot { vars })
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("serialize snapshot: {e}") })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        let posix = posix_locale(&profile.locale);
        match self.is_installed(&posix) {
            Some(false) => return Err(not_installed(&posix)),
            Some(true) => {}
            None => tracing::debug!("cannot list installed locales; trying localectl anyway"),
        }
        self.set(&[format!("LANG={posix}")])
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        check_snapshot_owner(NAME, &snapshot.backend)?;
        let saved: LocaleSnapshot = serde_json::from_value(snapshot.data.clone())
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("invalid snapshot data: {e}") })?;
        if saved.vars.is_empty() {
            return Err(BackendError::Other { backend: NAME.to_string(), detail: "snapshot has no locale variables".to_string() });
        }
        self.set(&saved.vars)
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let expected = posix_locale(&profile.locale);
        let vars = self.current()?;
        let current = lang_of(&vars).unwrap_or("(unset)");
        Ok(VerifyReport {
            backend: NAME.to_string(),
            ok: current == expected,
            detail: format!("system LANG is `{current}`, expected `{expected}` (affects new sessions only; log out and back in)"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;
    use std::io;

    const STATUS_SINGLE: &str = "   System Locale: LANG=en_US.UTF-8\n       VC Keymap: us\n      X11 Layout: us\n";
    const STATUS_MULTI: &str = "   System Locale: LANG=en_US.UTF-8\n                  LC_TIME=de_DE.UTF-8\n                  LC_PAPER=de_DE.UTF-8\n       VC Keymap: de\n      X11 Layout: de\n";
    const STATUS_NA: &str = "   System Locale: n/a\n       VC Keymap: n/a\n";

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

    fn setup() -> (Arc<FakeRunner>, LocaleBackend) {
        let fake = Arc::new(FakeRunner::new());
        let backend = LocaleBackend::new(fake.clone());
        (fake, backend)
    }

    #[test]
    fn parses_single_var() {
        assert_eq!(parse_system_locale(STATUS_SINGLE), vec!["LANG=en_US.UTF-8"]);
    }

    #[test]
    fn parses_continuation_lines() {
        assert_eq!(
            parse_system_locale(STATUS_MULTI),
            vec!["LANG=en_US.UTF-8", "LC_TIME=de_DE.UTF-8", "LC_PAPER=de_DE.UTF-8"]
        );
    }

    #[test]
    fn parses_na_and_garbage_as_empty() {
        assert!(parse_system_locale(STATUS_NA).is_empty());
        assert!(parse_system_locale("").is_empty());
        assert!(parse_system_locale("something else\n").is_empty());
    }

    #[test]
    fn assignment_validation() {
        assert!(is_assignment("LANG=de_DE.UTF-8"));
        assert!(is_assignment("LC_ALL=C"));
        assert!(!is_assignment("lang=x"));
        assert!(!is_assignment("LANG"));
        assert!(!is_assignment("=x"));
        assert!(!is_assignment("LANG=a b"));
        assert!(!is_assignment("VC Keymap: us"));
    }

    #[test]
    fn snapshot_stores_all_vars() {
        let (fake, b) = setup();
        fake.push_stdout(STATUS_MULTI);
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "locale");
        assert_eq!(
            snap.data,
            serde_json::json!({ "vars": ["LANG=en_US.UTF-8", "LC_TIME=de_DE.UTF-8", "LC_PAPER=de_DE.UTF-8"] })
        );
        assert_eq!(fake.calls(), vec!["localectl status"]);
    }

    #[test]
    fn snapshot_na_is_error() {
        let (fake, b) = setup();
        fake.push_stdout(STATUS_NA);
        assert!(matches!(b.snapshot(), Err(BackendError::Other { .. })));
    }

    #[test]
    fn snapshot_missing_tool_is_unsupported() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    const LOCALE_A: &str = "C\nC.utf8\nde_DE.utf8\nen_US.utf8\nPOSIX\n";

    #[test]
    fn apply_sets_lang() {
        let (fake, b) = setup();
        fake.push_stdout(LOCALE_A);
        b.apply(&profile("de-DE")).unwrap();
        assert_eq!(fake.calls(), vec!["locale -a", "localectl set-locale LANG=de_DE.UTF-8"]);
    }

    #[test]
    fn apply_uninstalled_locale_is_unsupported_with_hint() {
        let (fake, b) = setup();
        fake.push_stdout(LOCALE_A);
        match b.apply(&profile("fr-FR")) {
            Err(BackendError::Unsupported { detail, .. }) => {
                assert!(detail.contains("fr_FR.UTF-8"), "{detail}");
                assert!(detail.contains("locale-gen"), "{detail}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(fake.calls(), vec!["locale -a"], "localectl must not run");
    }

    #[test]
    fn apply_proceeds_when_locale_list_unavailable() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        b.apply(&profile("de-DE")).unwrap();
        assert_eq!(fake.calls()[1], "localectl set-locale LANG=de_DE.UTF-8");
    }

    #[test]
    fn locale_name_normalization() {
        assert_eq!(normalize_locale_name("de_DE.UTF-8"), normalize_locale_name("de_DE.utf8"));
        assert_ne!(normalize_locale_name("de_DE.UTF-8"), normalize_locale_name("de_AT.utf8"));
        assert_eq!(normalize_locale_name("sr_RS.UTF-8@latin"), "sr_rs.utf8@latin");
    }

    #[test]
    fn apply_polkit_needs_privilege() {
        let (fake, b) = setup();
        fake.push_stdout(LOCALE_A);
        fake.push(CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Failed to issue method call: Access denied".to_string(),
        });
        assert!(matches!(b.apply(&profile("de-DE")), Err(BackendError::NeedsPrivilege { .. })));
    }

    #[test]
    fn restore_passes_all_vars() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "locale".into(),
            data: serde_json::json!({ "vars": ["LANG=en_US.UTF-8", "LC_TIME=de_DE.UTF-8"] }),
        };
        b.restore(&snap).unwrap();
        assert_eq!(fake.raw_calls(), vec![(
            "localectl".to_string(),
            vec!["set-locale".to_string(), "LANG=en_US.UTF-8".to_string(), "LC_TIME=de_DE.UTF-8".to_string()]
        )]);
    }

    #[test]
    fn restore_rejects_malformed_vars() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "vars": ["--evil"] }) };
        assert!(b.restore(&snap).is_err());
        let empty = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({ "vars": [] }) };
        assert!(b.restore(&empty).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn verify_reports_and_mentions_new_sessions() {
        let (fake, b) = setup();
        fake.push_stdout("   System Locale: LANG=de_DE.UTF-8\n       VC Keymap: de\n");
        let ok = b.verify(&profile("de-DE")).unwrap();
        assert!(ok.ok);
        assert!(ok.detail.contains("new sessions"));

        fake.push_stdout(STATUS_SINGLE);
        let bad = b.verify(&profile("de-DE")).unwrap();
        assert!(!bad.ok);
        assert!(bad.detail.contains("en_US.UTF-8"));
    }
}
