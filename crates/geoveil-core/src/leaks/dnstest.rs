//! Opt-in active DNS leak test (`geoveil leaks --active-dns`).
//!
//! Uses Mullvad's public connection check, which works for any VPN:
//!
//! 1. `GET https://am.i.mullvad.net/json` describes the exit (IP, country,
//!    organization).
//! 2. For each round, a fresh random label `<label>` is generated and
//!    `GET https://<label>.dnsleak.am.i.mullvad.net/` is requested. Resolving
//!    that never-seen name goes through the **system resolver** (reqwest uses
//!    `getaddrinfo`), so Mullvad's authoritative server sees which recursive
//!    resolver asked, and the response lists those resolvers as JSON:
//!    `[{"ip", "country", "city", "organization", "mullvad_dns", ...}]`.
//!
//! What Mullvad learns: your exit IP, the resolvers your system uses, and the
//! random labels. Nothing runs unless the user passes `--active-dns`.
//!
//! The endpoints are undocumented; parsing is lenient and covered by fixture
//! tests so drift shows up as a clear error, not a panic.

use super::net::{fetch_text, probe_client, random_bytes};
use super::{ActiveDnsResult, ExitInfo, ObservedResolver};
use serde::Deserialize;
use std::net::IpAddr;
use std::time::Duration;

/// Provider name used in the report.
pub const PROVIDER: &str = "mullvad";
/// Exit description endpoint.
pub const EXIT_URL: &str = "https://am.i.mullvad.net/json";
/// Parent zone of the unique test names.
pub const LEAK_ZONE: &str = "dnsleak.am.i.mullvad.net";
/// Rounds (unique names) per run; several catch load-balanced resolvers.
pub const DEFAULT_ROUNDS: usize = 3;
/// The dnsleak endpoint returns JSON only for exactly this `Accept` value
/// (anything else gets a plain-text sentence).
const JSON: &str = "application/json";

/// Hosts the active test contacts, for the report.
pub fn contacted_hosts() -> Vec<String> {
    vec!["am.i.mullvad.net".to_string(), format!("<random>.{LEAK_ZONE}")]
}

/// A fresh random DNS label (32 lower-case hex characters).
pub fn random_label() -> String {
    let bytes: [u8; 16] = random_bytes();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The unique test URL for `label`.
pub fn leak_url(label: &str) -> String {
    format!("https://{label}.{LEAK_ZONE}/")
}

#[derive(Deserialize)]
struct RawResolver {
    ip: String,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    mullvad_dns: bool,
}

#[derive(Deserialize)]
struct RawExit {
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    organization: Option<String>,
}

/// Parses the dnsleak JSON array. Entries with an unparsable IP are skipped.
pub fn parse_leak_response(body: &str) -> Result<Vec<ObservedResolver>, String> {
    let raw: Vec<RawResolver> =
        serde_json::from_str(body.trim()).map_err(|e| format!("unexpected dnsleak response: {e}"))?;
    Ok(raw
        .into_iter()
        .filter_map(|r| {
            let ip: IpAddr = r.ip.trim().parse().ok()?;
            Some(ObservedResolver {
                ip,
                country: non_empty(r.country),
                organization: non_empty(r.organization),
                provider_dns: r.mullvad_dns,
            })
        })
        .collect())
}

/// Parses `am.i.mullvad.net/json`.
pub fn parse_exit_response(body: &str) -> Result<ExitInfo, String> {
    let raw: RawExit = serde_json::from_str(body.trim()).map_err(|e| format!("unexpected exit response: {e}"))?;
    Ok(ExitInfo {
        ip: raw.ip.and_then(|s| s.trim().parse().ok()),
        country: non_empty(raw.country),
        organization: non_empty(raw.organization),
    })
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Adds resolvers not already in `all` (by IP).
pub fn merge_resolvers(all: &mut Vec<ObservedResolver>, more: Vec<ObservedResolver>) {
    for r in more {
        if !all.iter().any(|a| a.ip == r.ip) {
            all.push(r);
        }
    }
}

/// Runs the test: one exit lookup plus `rounds` unique names.
pub async fn run(timeout: Duration, rounds: usize) -> ActiveDnsResult {
    let mut result = ActiveDnsResult {
        provider: PROVIDER.to_string(),
        contacted: contacted_hosts(),
        exit: None,
        resolvers: Vec::new(),
        error: None,
    };
    let client = match probe_client(timeout, None) {
        Ok(c) => c,
        Err(e) => {
            result.error = Some(e);
            return result;
        }
    };
    match fetch_text(&client, EXIT_URL, JSON, timeout).await.and_then(|b| parse_exit_response(&b)) {
        Ok(exit) => result.exit = Some(exit),
        Err(e) => tracing::debug!("{EXIT_URL}: {e}"),
    }
    let mut errors = Vec::new();
    for _ in 0..rounds.max(1) {
        let url = leak_url(&random_label());
        match fetch_text(&client, &url, JSON, timeout).await.and_then(|b| parse_leak_response(&b)) {
            Ok(found) => merge_resolvers(&mut result.resolvers, found),
            Err(e) => errors.push(e),
        }
    }
    if result.resolvers.is_empty() && !errors.is_empty() {
        result.error = Some(format!("{LEAK_ZONE}: {}", errors.join("; ")));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leak_fixture() {
        let body = r#"[
            {"ip":"2a06:98c0:3600::103","country":"France","city":"Paris","organization":"Cloudflare","mullvad_dns":false,"quad9_dns":false},
            {"ip":"193.138.218.74","country":"Sweden","city":"Malmo","organization":"Mullvad VPN","mullvad_dns":true,"quad9_dns":false},
            {"ip":"not-an-ip","country":"","organization":""}
        ]"#;
        let r = parse_leak_response(body).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].ip, "2a06:98c0:3600::103".parse::<IpAddr>().unwrap());
        assert_eq!(r[0].organization.as_deref(), Some("Cloudflare"));
        assert!(!r[0].provider_dns);
        assert!(r[1].provider_dns);
        assert_eq!(parse_leak_response("[]").unwrap(), vec![]);
        assert!(parse_leak_response("<html>rate limited</html>").is_err());
    }

    #[test]
    fn parses_exit_fixture() {
        let body = r#"{"ip":"82.152.98.44","country":"Germany","city":"Limburg","longitude":8.0,"latitude":50.4,
            "mullvad_exit_ip":false,"blacklisted":{"blacklisted":false,"results":[]},"organization":"OVH SAS"}"#;
        let e = parse_exit_response(body).unwrap();
        assert_eq!(e.ip, Some("82.152.98.44".parse().unwrap()));
        assert_eq!(e.country.as_deref(), Some("Germany"));
        assert_eq!(e.organization.as_deref(), Some("OVH SAS"));
        let sparse = parse_exit_response(r#"{"country":" "}"#).unwrap();
        assert_eq!(sparse, ExitInfo { ip: None, country: None, organization: None });
    }

    #[test]
    fn labels_are_unique_dns_labels() {
        let a = random_label();
        let b = random_label();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(leak_url("abc"), "https://abc.dnsleak.am.i.mullvad.net/");
    }

    #[test]
    fn merge_dedups_by_ip() {
        let r = |ip: &str| ObservedResolver { ip: ip.parse().unwrap(), country: None, organization: None, provider_dns: false };
        let mut all = vec![r("1.1.1.1")];
        merge_resolvers(&mut all, vec![r("1.1.1.1"), r("8.8.8.8")]);
        assert_eq!(all.len(), 2);
    }
}
