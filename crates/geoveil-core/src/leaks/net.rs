//! HTTP probes used by the leak checks, plus a small non-cryptographic random
//! source for STUN transaction IDs and DNS test labels.
//!
//! Every client here calls `no_proxy()`: the checks must measure the real
//! route, not an `HTTP(S)_PROXY` from the environment. (This workspace builds
//! reqwest without its `system-proxy` feature, so the Windows registry proxy
//! is never used either.)

use super::ExitProbe;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USER_AGENT: &str = concat!("geoveil/", env!("CARGO_PKG_VERSION"));

/// IPv6-only "what is my IP" endpoints (they publish AAAA records only).
pub const IPV6_ENDPOINTS: [&str; 2] = ["https://api6.ipify.org", "https://ipv6.icanhazip.com"];

/// HTTP client for the leak probes: proxies off, total `timeout`, bound to
/// `local` (use `::` to force IPv6 destinations).
pub fn probe_client(timeout: Duration, local: Option<IpAddr>) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .no_proxy()
        .local_address(local)
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))
}

/// Asks the IPv6-only endpoints which address they see. The first valid IPv6
/// answer wins; failures of all endpoints become [`ExitProbe::Failed`].
pub async fn ipv6_exit(timeout: Duration) -> ExitProbe {
    let client = match probe_client(timeout, Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))) {
        Ok(c) => c,
        Err(error) => return ExitProbe::Failed { error },
    };
    let mut errors = Vec::new();
    for url in IPV6_ENDPOINTS {
        match fetch_text(&client, url, "text/plain", timeout).await.and_then(|b| parse_ipv6_answer(&b)) {
            Ok(ip) => return ExitProbe::Found { ip },
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    ExitProbe::Failed { error: errors.join("; ") }
}

/// GET `url` with the given `Accept` header and return the body, within
/// `timeout`. (Mullvad's dnsleak endpoint answers JSON only for exactly
/// `application/json`.)
pub(crate) async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    accept: &str,
    timeout: Duration,
) -> Result<String, String> {
    let req = async {
        let resp = client
            .get(url)
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| e.to_string())?;
        resp.text().await.map_err(|e| format!("reading body: {e}"))
    };
    tokio::time::timeout(timeout, req)
        .await
        .unwrap_or_else(|_| Err(format!("timed out after {timeout:?}")))
}

/// Parses a plain-text answer that must be an IPv6 address.
pub fn parse_ipv6_answer(body: &str) -> Result<IpAddr, String> {
    let text = body.trim();
    match text.parse::<IpAddr>() {
        Ok(ip @ IpAddr::V6(_)) => Ok(ip),
        Ok(other) => Err(format!("expected an IPv6 address, got {other}")),
        Err(_) => Err(format!("not an IP address: {:?}", text.chars().take(60).collect::<String>())),
    }
}

/// 64 bits from the standard library's randomly keyed hasher, mixed with a
/// counter and the clock. Unpredictable enough for STUN transaction IDs and
/// unique DNS labels; not for cryptography.
pub fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    h.write_u128(nanos);
    h.finish()
}

/// `N` random bytes from [`random_u64`].
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let r = random_u64().to_le_bytes();
        chunk.copy_from_slice(&r[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv6_answers() {
        assert_eq!(parse_ipv6_answer("2a01:4f8::1\n").unwrap(), "2a01:4f8::1".parse::<IpAddr>().unwrap());
        assert!(parse_ipv6_answer("203.0.113.7").is_err());
        assert!(parse_ipv6_answer("<html>").is_err());
    }

    #[test]
    fn random_values_differ() {
        let a: [u8; 12] = random_bytes();
        let b: [u8; 12] = random_bytes();
        assert_ne!(a, b);
        assert_ne!(random_u64(), random_u64());
    }

    #[test]
    fn client_builds() {
        assert!(probe_client(Duration::from_secs(3), None).is_ok());
        assert!(probe_client(Duration::from_secs(3), Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))).is_ok());
    }
}
