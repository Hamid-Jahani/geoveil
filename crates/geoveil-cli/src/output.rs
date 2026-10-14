//! Human-readable formatting and JSON printing. Formatting functions return
//! `String` so they can be unit tested; `emit` does the actual printing.

use geoveil_core::orchestrator::Report;
use geoveil_core::{NetInterface, Profile, ProfileSource, VerifyReport};
use serde::Serialize;

/// Print `value` as pretty JSON when `json` is set, otherwise print `human`.
pub fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce() -> String) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: could not serialize output: {e}"),
        }
    } else {
        let text = human();
        if !text.is_empty() {
            println!("{}", text.trim_end());
        }
    }
}

/// Multi-line description of a profile.
pub fn format_profile(p: &Profile) -> String {
    let country = if p.country.is_empty() { "(unknown)" } else { p.country.as_str() };
    let source = match &p.source {
        ProfileSource::VpnExit { ip } => format!("VPN exit {ip}"),
        ProfileSource::Manual => "manual".to_string(),
    };
    format!(
        "location  {:.4}, {:.4} (±{:.0} m)\ncountry   {country}\ntimezone  {}\nlocale    {}\nsource    {source}\n",
        p.lat, p.lon, p.accuracy_m, p.timezone, p.locale
    )
}

/// One line per backend outcome.
pub fn format_report(r: &Report) -> String {
    if r.outcomes.is_empty() {
        return "no backends available on this system\n".to_string();
    }
    r.outcomes
        .iter()
        .map(|o| match (&o.error, o.ok) {
            (_, true) => format!("  ok    {}\n", o.backend),
            (Some(e), false) => format!("  FAIL  {}: {e}\n", o.backend),
            (None, false) => format!("  FAIL  {}\n", o.backend),
        })
        .collect()
}

/// One line per verify report.
pub fn format_verify(reports: &[VerifyReport]) -> String {
    reports
        .iter()
        .map(|v| format!("  {}  {}: {}\n", if v.ok { "ok  " } else { "DIFF" }, v.backend, v.detail))
        .collect()
}

/// One line per interface, VPN-like ones marked.
pub fn format_interfaces(ifaces: &[(NetInterface, bool)]) -> String {
    ifaces
        .iter()
        .map(|(i, vpn)| {
            let mark = if *vpn { "VPN " } else { "    " };
            let state = if i.is_up { "up  " } else { "down" };
            let desc = if i.description.is_empty() || i.description == i.name {
                String::new()
            } else {
                format!(" ({})", i.description)
            };
            let addrs: Vec<String> = i.addrs.iter().map(ToString::to_string).collect();
            format!("  {mark}{state}  {}{desc}  [{}]\n", i.name, addrs.join(", "))
        })
        .collect()
}

/// Comma-separated names, or "none".
pub fn format_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoveil_core::orchestrator::BackendOutcome;

    fn profile(source: ProfileSource, country: &str) -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: "Europe/Berlin".into(),
            locale: "de-DE".into(),
            country: country.into(),
            source,
        }
    }

    #[test]
    fn profile_vpn_source() {
        let s = format_profile(&profile(ProfileSource::VpnExit { ip: "198.51.100.1".parse().unwrap() }, "DE"));
        assert!(s.contains("52.5200, 13.4050"));
        assert!(s.contains("±5000 m"));
        assert!(s.contains("country   DE"));
        assert!(s.contains("Europe/Berlin"));
        assert!(s.contains("VPN exit 198.51.100.1"));
    }

    #[test]
    fn profile_manual_unknown_country() {
        let s = format_profile(&profile(ProfileSource::Manual, ""));
        assert!(s.contains("(unknown)"));
        assert!(s.contains("source    manual"));
    }

    #[test]
    fn report_lines() {
        let r = Report {
            outcomes: vec![
                BackendOutcome { backend: "timezone".into(), ok: true, error: None },
                BackendOutcome { backend: "locale".into(), ok: false, error: Some("denied".into()) },
            ],
        };
        let s = format_report(&r);
        assert!(s.contains("ok    timezone"));
        assert!(s.contains("FAIL  locale: denied"));
        assert_eq!(format_report(&Report { outcomes: vec![] }), "no backends available on this system\n");
    }

    #[test]
    fn interface_lines() {
        let i = NetInterface {
            name: "wg0".into(),
            description: String::new(),
            is_up: true,
            addrs: vec!["10.0.0.2".parse().unwrap()],
        };
        let s = format_interfaces(&[(i, true)]);
        assert_eq!(s, "  VPN up    wg0  [10.0.0.2]\n");
    }

    #[test]
    fn verify_lines() {
        let v = VerifyReport { backend: "timezone".into(), ok: false, detail: "UTC vs Europe/Berlin".into() };
        assert_eq!(format_verify(&[v]), "  DIFF  timezone: UTC vs Europe/Berlin\n");
    }

    #[test]
    fn names() {
        assert_eq!(format_names(&[]), "none");
        assert_eq!(format_names(&["a".into(), "b".into()]), "a, b");
    }
}
