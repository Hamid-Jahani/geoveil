//! Windows per-user regional format and home location via PowerShell.
//!
//! - Regional format: `Set-Culture` / `(Get-Culture).Name`.
//! - Home location (region): `Set-WinHomeLocation -GeoId` / `(Get-WinHomeLocation).GeoId`.
//!
//! `Set-WinUserLanguageList` is deliberately NOT used: it replaces the user's
//! input language list, which adds/removes keyboard layouts and input methods.
//! That is disruptive (the user can suddenly type in the wrong layout) and is
//! not reliably restorable, while websites learn nothing extra from it beyond
//! what the browser already exposes.
//!
//! `Set-Culture` also resets the user's own format overrides (short date,
//! time format, first day of week, decimal symbol, ...) stored as string
//! values under `HKCU\Control Panel\International`. The snapshot therefore
//! saves every string value of that key, and restore writes them back after
//! `Set-Culture`, so customised formats survive a round trip. (Apps that are
//! already running may only notice the restored overrides after a restart.)
//!
//! Security: every value interpolated into a PowerShell script is validated
//! first (culture-name shape, a `u32`, or `[A-Za-z0-9]` registry value names)
//! and string values are single-quoted. Registry *values* can contain any
//! character, so they are passed as base64-encoded JSON and decoded inside
//! the script. Nothing from a profile or snapshot file can inject PowerShell.
//! The read script escapes non-ASCII characters in its JSON output, so the
//! console code page cannot corrupt values such as a `€` currency symbol.
//!
//! Snapshot data shape:
//! `{"culture": "en-US", "geo_id": 244, "international": {"sShortDate": "yyyy-MM-dd", ...}}`.

use super::run_checked;
use crate::runner::CommandRunner;
use geoveil_core::locale::windows_geo_id;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

const NAME: &str = "locale";
const POWERSHELL: &str = "powershell";
/// Longest registry value name accepted from a snapshot.
const MAX_VALUE_NAME_LEN: usize = 64;

/// Reads culture, home GeoId and every string value under
/// `HKCU:\Control Panel\International` in one PowerShell call, as compact
/// JSON with non-ASCII characters escaped as `\uXXXX`.
const READ_SCRIPT: &str = concat!(
    r"$ErrorActionPreference = 'Stop'; ",
    r"$k = Get-Item -LiteralPath 'HKCU:\Control Panel\International'; ",
    r"$m = [ordered]@{}; ",
    r"foreach ($n in $k.GetValueNames()) { if ($n -and $k.GetValueKind($n) -eq 'String') { $m[$n] = [string]$k.GetValue($n) } }; ",
    r"$j = [pscustomobject]@{ culture = (Get-Culture).Name; geo_id = (Get-WinHomeLocation).GeoId; international = $m } ",
    r"| ConvertTo-Json -Compress; ",
    r"[regex]::Replace($j, '[^\x00-\x7F]', { param($c) '\u{0:x4}' -f [int][char]$c.Value })"
);

/// Sets the Windows regional format (culture) and home location to match the profile.
pub struct LocaleBackend {
    runner: Arc<dyn CommandRunner>,
}

/// What the OS currently reports; also the snapshot payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LocaleState {
    culture: String,
    geo_id: u32,
    /// String values of `HKCU\Control Panel\International`; `None` in
    /// snapshots written before this was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    international: Option<BTreeMap<String, String>>,
}

impl LocaleBackend {
    /// Creates the backend using `runner` to execute PowerShell.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Runs a PowerShell script non-interactively, without the user's profile.
    fn run_script(&self, display: &str, script: &str) -> Result<String, BackendError> {
        let out = run_checked(
            self.runner.as_ref(),
            NAME,
            display,
            POWERSHELL,
            &["-NoProfile", "-NonInteractive", "-Command", script],
        )?;
        Ok(out.stdout)
    }

    /// Reads the current culture and home GeoId.
    fn read_state(&self) -> Result<LocaleState, BackendError> {
        let stdout = self.run_script("powershell Get-Culture/Get-WinHomeLocation", READ_SCRIPT)?;
        parse_state(&stdout)
    }

    /// Sets culture, optionally home location and saved International
    /// values, in one PowerShell call.
    fn write_state(
        &self,
        culture: &str,
        geo_id: Option<u32>,
        international: Option<&BTreeMap<String, String>>,
    ) -> Result<(), BackendError> {
        let script = build_set_script(culture, geo_id, international)?;
        self.run_script("powershell Set-Culture/Set-WinHomeLocation", &script)?;
        Ok(())
    }
}

/// Parses the JSON printed by [`READ_SCRIPT`].
fn parse_state(stdout: &str) -> Result<LocaleState, BackendError> {
    let text = stdout.trim().trim_start_matches('\u{feff}').trim();
    serde_json::from_str(text).map_err(|e| BackendError::Other {
        backend: NAME.to_string(),
        detail: format!("cannot parse PowerShell locale output `{text}`: {e}"),
    })
}

/// True if `tag` looks like a BCP-47 tag: `^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$`.
///
/// Hand-written so no regex dependency is needed. The allowed alphabet
/// (ASCII letters, digits, `-`) cannot break out of a single-quoted
/// PowerShell string.
pub(crate) fn is_valid_locale_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let primary_ok = parts
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()));
    primary_ok && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// True if `name` looks like a Windows culture name as `(Get-Culture).Name`
/// reports it: a BCP-47-shaped tag whose later subtags may also carry an
/// alternate sort suffix after `_` (`de-DE_phoneb`, `es-ES_tradnl`).
///
/// Used for culture names read back from the OS (snapshot and restore), so
/// restoring is never stricter than snapshotting. The alphabet (ASCII
/// letters, digits, `-`, `_`) cannot break out of a single-quoted
/// PowerShell string.
pub(crate) fn is_valid_culture_name(name: &str) -> bool {
    let mut parts = name.split('-');
    let primary_ok = parts
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()));
    primary_ok
        && parts.all(|p| (1..=16).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Registry value names we are willing to write back: `[A-Za-z0-9]{1,64}`.
fn is_valid_value_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_VALUE_NAME_LEN && name.chars().all(|c| c.is_ascii_alphanumeric())
}

fn invalid(detail: String) -> BackendError {
    BackendError::Other { backend: NAME.to_string(), detail }
}

/// Builds the apply/restore script after validating every interpolated value.
///
/// `culture` must already have passed [`is_valid_locale_tag`] (apply) or
/// [`is_valid_culture_name`] (restore); it is re-checked against the looser
/// of the two here as a last line of defence.
fn build_set_script(
    culture: &str,
    geo_id: Option<u32>,
    international: Option<&BTreeMap<String, String>>,
) -> Result<String, BackendError> {
    if !is_valid_culture_name(culture) {
        return Err(invalid(format!("refusing invalid locale tag `{culture}`")));
    }
    let mut script = format!("$ErrorActionPreference = 'Stop'; Set-Culture -CultureInfo '{culture}'");
    if let Some(id) = geo_id {
        // `id` is a u32, so its decimal form is digits only.
        script.push_str(&format!("; Set-WinHomeLocation -GeoId {id}"));
    }
    if let Some(values) = international.filter(|v| !v.is_empty()) {
        if let Some(bad) = values.keys().find(|k| !is_valid_value_name(k)) {
            return Err(invalid(format!("refusing invalid International value name `{bad}`")));
        }
        let json = serde_json::to_string(values)
            .map_err(|e| invalid(format!("cannot encode International values: {e}")))?;
        // Base64 alphabet is [A-Za-z0-9+/=]: safe inside single quotes.
        let b64 = base64_encode(json.as_bytes());
        script.push_str(&format!(
            "; $v = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{b64}')) | ConvertFrom-Json\
             ; foreach ($e in $v.PSObject.Properties) {{ Set-ItemProperty -LiteralPath 'HKCU:\\Control Panel\\International' \
             -Name $e.Name -Value ([string]$e.Value) -Type String }}"
        ));
    }
    Ok(script)
}

/// Standard base64 (RFC 4648) with padding.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Drops International values whose names could not be written back, so the
/// snapshot only holds what restore will accept.
fn restorable_values(values: BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .into_iter()
        .filter(|(name, _)| {
            let ok = is_valid_value_name(name);
            if !ok {
                tracing::warn!(name = %name, "skipping International registry value with an unusual name");
            }
            ok
        })
        .collect()
}

impl Backend for LocaleBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let mut state = self.read_state()?;
        // Never save what restore would refuse: the orchestrator then skips
        // this backend instead of changing something it cannot undo.
        if !is_valid_culture_name(&state.culture) {
            return Err(invalid(format!("current culture name `{}` cannot be restored safely", state.culture)));
        }
        state.international = state.international.map(restorable_values);
        let data = serde_json::to_value(&state).map_err(|e| BackendError::Other {
            backend: NAME.to_string(),
            detail: format!("cannot serialize snapshot: {e}"),
        })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        let geo_id = windows_geo_id(&profile.country);
        if geo_id.is_none() {
            tracing::warn!(
                country = %profile.country,
                "no Windows GeoID for country; setting culture only and leaving home location unchanged"
            );
        }
        if !is_valid_locale_tag(&profile.locale) {
            return Err(invalid(format!("refusing invalid locale tag `{}`", profile.locale)));
        }
        self.write_state(&profile.locale, geo_id, None)
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        let state: LocaleState = serde_json::from_value(snapshot.data.clone()).map_err(|e| BackendError::Other {
            backend: NAME.to_string(),
            detail: format!("invalid locale snapshot: {e}"),
        })?;
        self.write_state(&state.culture, Some(state.geo_id), state.international.as_ref())
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        let state = self.read_state()?;
        let culture_ok = state.culture.eq_ignore_ascii_case(&profile.locale);
        let (geo_ok, geo_detail) = match windows_geo_id(&profile.country) {
            Some(expected) => (
                state.geo_id == expected,
                format!("home GeoId {} (expected {expected})", state.geo_id),
            ),
            None => (true, format!("home GeoId {} (not checked: no GeoID for `{}`)", state.geo_id, profile.country)),
        };
        Ok(VerifyReport {
            backend: NAME.to_string(),
            ok: culture_ok && geo_ok,
            detail: format!("culture `{}` (expected `{}`), {geo_detail}", state.culture, profile.locale),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;
    use std::io;

    fn profile(locale: &str, country: &str) -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: "Europe/Berlin".to_string(),
            locale: locale.to_string(),
            country: country.to_string(),
            source: ProfileSource::Manual,
        }
    }

    fn setup() -> (Arc<FakeRunner>, LocaleBackend) {
        let fake = Arc::new(FakeRunner::new());
        let backend = LocaleBackend::new(fake.clone());
        (fake, backend)
    }

    /// Asserts the call is a PowerShell -Command call and returns its script.
    fn script_of(call: &(String, Vec<String>)) -> String {
        assert_eq!(call.0, "powershell");
        assert_eq!(&call.1[..3], &["-NoProfile", "-NonInteractive", "-Command"]);
        assert_eq!(call.1.len(), 4);
        call.1[3].clone()
    }

    #[test]
    fn snapshot_reads_both_in_one_call() {
        let (fake, b) = setup();
        fake.push_stdout(
            "{\"culture\":\"en-US\",\"geo_id\":244,\"international\":{\"sShortDate\":\"yyyy-MM-dd\",\"sCurrency\":\"\\u20ac\",\"sPositiveSign\":\"\"}}\r\n",
        );
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "locale");
        assert_eq!(
            snap.data,
            serde_json::json!({
                "culture": "en-US",
                "geo_id": 244,
                "international": {"sShortDate": "yyyy-MM-dd", "sCurrency": "\u{20ac}", "sPositiveSign": ""}
            })
        );
        let calls = fake.raw_calls();
        assert_eq!(calls.len(), 1);
        let script = script_of(&calls[0]);
        assert!(script.contains("Get-Culture"));
        assert!(script.contains("Get-WinHomeLocation"));
        assert!(script.contains(r"'HKCU:\Control Panel\International'"));
        assert!(script.contains("ConvertTo-Json -Compress"));
        assert!(script.contains(r"'[^\x00-\x7F]'"), "non-ASCII must be escaped: {script}");
    }

    #[test]
    fn snapshot_refuses_unrestorable_culture_and_drops_odd_value_names() {
        let (fake, b) = setup();
        fake.push_stdout("{\"culture\":\"\",\"geo_id\":244}");
        assert!(matches!(b.snapshot(), Err(BackendError::Other { .. })));

        fake.push_stdout("{\"culture\":\"de-DE_phoneb\",\"geo_id\":94,\"international\":{\"sDate\":\".\",\"bad name\":\"x\"}}");
        let snap = b.snapshot().unwrap();
        assert_eq!(
            snap.data,
            serde_json::json!({"culture": "de-DE_phoneb", "geo_id": 94, "international": {"sDate": "."}})
        );
    }

    #[test]
    fn alternate_sort_culture_round_trips() {
        let (fake, b) = setup();
        fake.push_stdout("{\"culture\":\"es-ES_tradnl\",\"geo_id\":217}");
        let snap = b.snapshot().unwrap();
        b.restore(&snap).unwrap();
        let script = script_of(&fake.raw_calls()[1]);
        assert!(script.contains("Set-Culture -CultureInfo 'es-ES_tradnl'"), "{script}");
    }

    #[test]
    fn restore_writes_back_custom_formats() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "locale".into(),
            data: serde_json::json!({
                "culture": "en-US",
                "geo_id": 244,
                "international": {"sShortDate": "yyyy-MM-dd", "iFirstDayOfWeek": "0", "sCurrency": "it's \u{20ac}"}
            }),
        };
        b.restore(&snap).unwrap();
        let script = script_of(&fake.raw_calls()[0]);
        assert!(script.starts_with(
            "$ErrorActionPreference = 'Stop'; Set-Culture -CultureInfo 'en-US'; Set-WinHomeLocation -GeoId 244; "
        ));
        // Values travel base64-encoded, so quotes in them cannot break out.
        let expected = serde_json::json!({"iFirstDayOfWeek": "0", "sCurrency": "it's \u{20ac}", "sShortDate": "yyyy-MM-dd"});
        let b64 = base64_encode(serde_json::to_string(&expected).unwrap().as_bytes());
        assert!(script.contains(&format!("FromBase64String('{b64}')")), "{script}");
        assert!(!script.contains("it's"));
        assert!(script.contains(r"Set-ItemProperty -LiteralPath 'HKCU:\Control Panel\International'"));
        assert!(script.contains("-Type String"));
    }

    #[test]
    fn restore_rejects_tampered_value_names() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "locale".into(),
            data: serde_json::json!({"culture": "en-US", "geo_id": 244, "international": {"x'; Remove-Item C:\\": "1"}}),
        };
        assert!(matches!(b.restore(&snap), Err(BackendError::Other { .. })));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn base64_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), want, "{input}");
        }
        assert_eq!(base64_encode("\u{20ac}".as_bytes()), "4oKs");
    }

    #[test]
    fn culture_name_validation() {
        for ok in ["en-US", "de-DE_phoneb", "es-ES_tradnl", "zh-Hans-CN", "ja-JP_radstr", "fil-PH"] {
            assert!(is_valid_culture_name(ok), "{ok}");
        }
        for bad in ["", "e", "en-", "en--US", "en-US'", "en US", "x'; Remove-Item", "-US", "en_US"] {
            assert!(!is_valid_culture_name(bad), "{bad}");
        }
    }

    #[test]
    fn parse_handles_bom_and_whitespace() {
        let s = parse_state("\u{feff}{\"culture\":\"de-DE\",\"geo_id\":94}\r\n").unwrap();
        assert_eq!(s, LocaleState { culture: "de-DE".into(), geo_id: 94, international: None });
        assert!(parse_state("garbage").is_err());
        assert!(parse_state("{\"culture\":\"de-DE\",\"geo_id\":-1}").is_err());
    }

    #[test]
    fn apply_sets_culture_and_home_location() {
        let (fake, b) = setup();
        b.apply(&profile("de-DE", "DE")).unwrap();
        let calls = fake.raw_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            script_of(&calls[0]),
            "$ErrorActionPreference = 'Stop'; Set-Culture -CultureInfo 'de-DE'; Set-WinHomeLocation -GeoId 94"
        );
    }

    #[test]
    fn apply_without_geo_id_sets_culture_only() {
        let (fake, b) = setup();
        b.apply(&profile("en-US", "")).unwrap();
        let script = script_of(&fake.raw_calls()[0]);
        assert_eq!(script, "$ErrorActionPreference = 'Stop'; Set-Culture -CultureInfo 'en-US'");
    }

    #[test]
    fn apply_rejects_injection_without_running() {
        let (fake, b) = setup();
        let err = b.apply(&profile("en-US'; Remove-Item x; '", "US")).unwrap_err();
        assert!(matches!(err, BackendError::Other { .. }));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn restore_uses_snapshot_values() {
        let (fake, b) = setup();
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({"culture": "en-GB", "geo_id": 242}) };
        b.restore(&snap).unwrap();
        assert_eq!(
            script_of(&fake.raw_calls()[0]),
            "$ErrorActionPreference = 'Stop'; Set-Culture -CultureInfo 'en-GB'; Set-WinHomeLocation -GeoId 242"
        );
    }

    #[test]
    fn restore_rejects_tampered_snapshot() {
        let (fake, b) = setup();
        let snap = BackendSnapshot {
            backend: "locale".into(),
            data: serde_json::json!({"culture": "x'; Remove-Item C:\\ -Recurse; '", "geo_id": 244}),
        };
        assert!(matches!(b.restore(&snap), Err(BackendError::Other { .. })));
        let snap = BackendSnapshot { backend: "locale".into(), data: serde_json::json!({"culture": "en-US", "geo_id": "244; evil"}) };
        assert!(matches!(b.restore(&snap), Err(BackendError::Other { .. })));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn verify_compares_culture_and_geo() {
        let (fake, b) = setup();
        fake.push_stdout("{\"culture\":\"de-DE\",\"geo_id\":94}");
        assert!(b.verify(&profile("de-DE", "DE")).unwrap().ok);
        fake.push_stdout("{\"culture\":\"de-DE\",\"geo_id\":244}");
        assert!(!b.verify(&profile("de-DE", "DE")).unwrap().ok);
        fake.push_stdout("{\"culture\":\"en-US\",\"geo_id\":94}");
        assert!(!b.verify(&profile("de-DE", "DE")).unwrap().ok);
    }

    #[test]
    fn failures_are_mapped() {
        let (fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
        fake.push(CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Set-WinHomeLocation : Access is denied.".into(),
        });
        assert!(matches!(b.apply(&profile("de-DE", "DE")), Err(BackendError::NeedsPrivilege { .. })));
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "Culture is not supported.".into() });
        assert!(matches!(b.apply(&profile("de-DE", "DE")), Err(BackendError::Command { .. })));
    }

    #[test]
    fn locale_tag_validation() {
        for ok in ["en", "en-US", "de-CH", "zh-Hans-CN", "es-419", "fil-PH", "sr-Latn-RS"] {
            assert!(is_valid_locale_tag(ok), "{ok}");
        }
        for bad in [
            "",
            "e",
            "engl-US",
            "en-",
            "-US",
            "en--US",
            "en_US",
            "en-US'",
            "en-US'; Remove-Item x; '",
            "en-U",
            "en-abcdefghi",
            "en US",
            "ен-US",
        ] {
            assert!(!is_valid_locale_tag(bad), "{bad}");
        }
    }
}
