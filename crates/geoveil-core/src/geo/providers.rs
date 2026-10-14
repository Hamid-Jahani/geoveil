//! Free, key-less HTTP geolocation providers and public-IP discovery.
//!
//! Every request is an ordinary HTTPS (or, for ip-api.com, HTTP) request on
//! the system route, so while a VPN is up it travels through the tunnel and
//! the provider sees the VPN exit IP. ip-api.com's free tier only supports
//! plain HTTP; the request is still carried inside the VPN tunnel, it is just
//! not end-to-end encrypted between the VPN exit and ip-api.com.

use super::{first_ok, BoxFuture, GeoError, IpGeoProvider, ProviderAnswer};
use serde::Deserialize;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

const USER_AGENT: &str = concat!("geoveil/", env!("CARGO_PKG_VERSION"));

/// Safety cap on one provider request. The effective per-request limit is the
/// caller's [`super::ResolveOptions::timeout`], enforced by [`super::resolve`];
/// the client cap must be at least as large as the largest timeout the CLI
/// accepts (120 s), or a longer `--provider-timeout-secs` would have no effect.
const PROVIDER_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

const IPWHOIS: &str = "ipwho.is";
const IPAPI_CO: &str = "ipapi.co";
const IP_API: &str = "ip-api.com";
const IPINFO: &str = "ipinfo.io";

/// Plain-text "what is my IP" endpoints used by [`public_ip`].
const PUBLIC_IP_ENDPOINTS: [&str; 3] = [
    "https://api.ipify.org",
    "https://icanhazip.com",
    "https://ifconfig.me/ip",
];

type ParseFn = fn(&str, Option<IpAddr>) -> Result<ProviderAnswer, GeoError>;
type UrlFn = fn(Option<IpAddr>) -> String;

/// The built-in providers: ipwho.is, ipapi.co, ip-api.com and ipinfo.io,
/// sharing one HTTP client.
pub fn default_providers() -> Vec<Box<dyn IpGeoProvider>> {
    let client = build_client(PROVIDER_CLIENT_TIMEOUT, Family::Any);
    let specs: [(&'static str, UrlFn, ParseFn); 4] = [
        (IPWHOIS, ipwhois_url, parse_ipwhois),
        (IPAPI_CO, ipapi_co_url, parse_ipapi_co),
        (IP_API, ip_api_url, parse_ip_api),
        (IPINFO, ipinfo_url, parse_ipinfo),
    ];
    specs
        .into_iter()
        .map(|(name, url, parse)| {
            Box::new(HttpProvider {
                name,
                client: client.clone(),
                url,
                parse,
            }) as Box<dyn IpGeoProvider>
        })
        .collect()
}

/// Address family used for public-IP discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Connect over IPv4 only.
    V4,
    /// Whatever the system picks (usually IPv6 first on dual-stack hosts).
    Any,
}

/// Discover this machine's public IP (the VPN exit IP while a VPN is up).
///
/// The answer must be deterministic: on a dual-stack connection the
/// endpoints are reachable over both IPv4 and IPv6, and racing them would
/// return the IPv4 address on one check and the IPv6 address on the next,
/// which looks like an exit change (and with a VPN that only tunnels IPv4,
/// the IPv6 address is the user's real one). So all endpoints are first
/// queried over IPv4 only; only if every IPv4 attempt fails are they queried
/// again without restricting the family. Within one round the first valid
/// answer wins.
pub async fn public_ip(timeout: Duration) -> Result<IpAddr, GeoError> {
    public_ip_with(|family| query_public_ip(family, timeout)).await
}

/// IPv4 first, then any family; `query` performs one round of lookups.
async fn public_ip_with<F, Fut>(query: F) -> Result<IpAddr, GeoError>
where
    F: Fn(Family) -> Fut,
    Fut: Future<Output = Result<IpAddr, String>>,
{
    let v4_err = match query(Family::V4).await {
        Ok(ip) => return Ok(ip),
        Err(e) => e,
    };
    tracing::debug!(error = %v4_err, "public IP over IPv4 failed; retrying with any address family");
    query(Family::Any).await.map_err(|any_err| GeoError::Http {
        provider: "public-ip".to_string(),
        detail: format!("over IPv4: {v4_err}; over any family: {any_err}"),
    })
}

/// One round: query every endpoint concurrently over `family`.
async fn query_public_ip(family: Family, timeout: Duration) -> Result<IpAddr, String> {
    let client = build_client(timeout, family)?;
    let futs: Vec<BoxFuture<'_, Result<IpAddr, String>>> = PUBLIC_IP_ENDPOINTS
        .iter()
        .map(|url| Box::pin(fetch_public_ip(&client, url, timeout)) as BoxFuture<'_, _>)
        .collect();
    first_ok(futs).await.map_err(|errors| errors.join("; "))
}

async fn fetch_public_ip(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<IpAddr, String> {
    let request = async {
        let resp = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("{url}: {e}"))?;
        let body = resp.text().await.map_err(|e| format!("{url}: reading body: {e}"))?;
        parse_plain_ip(&body).map_err(|e| format!("{url}: {e}"))
    };
    tokio::time::timeout(timeout, request)
        .await
        .unwrap_or_else(|_| Err(format!("{url}: timed out after {timeout:?}")))
}

/// Parse a plain-text IP response such as `"203.0.113.7\n"`.
fn parse_plain_ip(body: &str) -> Result<IpAddr, String> {
    let text = body.trim();
    text.parse::<IpAddr>()
        .map_err(|e| format!("not an IP address ({e}): {:?}", snippet(text)))
}

/// HTTP client with a total request `timeout`. [`Family::V4`] binds to
/// `0.0.0.0`, which makes the connector use IPv4 destination addresses only.
fn build_client(timeout: Duration, family: Family) -> Result<reqwest::Client, String> {
    let local = match family {
        Family::V4 => Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        Family::Any => None,
    };
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .local_address(local)
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))
}

/// A provider defined by a URL builder and a pure JSON parser.
struct HttpProvider {
    name: &'static str,
    client: Result<reqwest::Client, String>,
    url: UrlFn,
    parse: ParseFn,
}

impl HttpProvider {
    fn http_err(&self, detail: String) -> GeoError {
        GeoError::Http {
            provider: self.name.to_string(),
            detail,
        }
    }
}

#[async_trait::async_trait]
impl IpGeoProvider for HttpProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn lookup(&self, ip: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
        let client = self
            .client
            .as_ref()
            .map_err(|e| self.http_err(format!("HTTP client unavailable: {e}")))?;
        let url = (self.url)(ip);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| self.http_err(format!("GET {url}: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| self.http_err(format!("GET {url}: reading body: {e}")))?;
        if !status.is_success() {
            // Providers often explain errors (rate limits etc.) in the body.
            let detail = match (self.parse)(&body, ip) {
                Err(GeoError::Parse { detail, .. }) => format!("HTTP {status}: {detail}"),
                _ => format!("HTTP {status}: {}", snippet(&body)),
            };
            return Err(self.http_err(detail));
        }
        (self.parse)(&body, ip)
    }
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

fn ipwhois_url(ip: Option<IpAddr>) -> String {
    match ip {
        Some(ip) => format!("https://ipwho.is/{ip}"),
        None => "https://ipwho.is/".to_string(),
    }
}

fn ipapi_co_url(ip: Option<IpAddr>) -> String {
    match ip {
        Some(ip) => format!("https://ipapi.co/{ip}/json/"),
        None => "https://ipapi.co/json/".to_string(),
    }
}

/// ip-api.com free tier is HTTP only (see module docs).
fn ip_api_url(ip: Option<IpAddr>) -> String {
    const FIELDS: &str = "status,message,country,countryCode,lat,lon,timezone,query";
    match ip {
        Some(ip) => format!("http://ip-api.com/json/{ip}?fields={FIELDS}"),
        None => format!("http://ip-api.com/json/?fields={FIELDS}"),
    }
}

fn ipinfo_url(ip: Option<IpAddr>) -> String {
    match ip {
        Some(ip) => format!("https://ipinfo.io/{ip}/json"),
        None => "https://ipinfo.io/json".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Parsers (pure)
// ---------------------------------------------------------------------------

/// Provider-independent fields before validation.
struct RawAnswer {
    ip: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    country: Option<String>,
    timezone: Option<String>,
}

fn parse_err(provider: &str, detail: impl Into<String>) -> GeoError {
    GeoError::Parse {
        provider: provider.to_string(),
        detail: detail.into(),
    }
}

fn from_json<'a, T: Deserialize<'a>>(provider: &str, body: &'a str) -> Result<T, GeoError> {
    serde_json::from_str(body)
        .map_err(|e| parse_err(provider, format!("invalid JSON ({e}): {:?}", snippet(body))))
}

/// Validate a raw answer and turn it into a [`ProviderAnswer`].
/// `requested` is used when the payload does not echo the IP back.
fn finish(provider: &str, raw: RawAnswer, requested: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
    let ip = match raw.ip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse::<IpAddr>()
            .map_err(|e| parse_err(provider, format!("bad ip {s:?}: {e}")))?,
        None => requested.ok_or_else(|| parse_err(provider, "response has no ip"))?,
    };
    let lat = raw.lat.ok_or_else(|| parse_err(provider, "response has no latitude"))?;
    let lon = raw.lon.ok_or_else(|| parse_err(provider, "response has no longitude"))?;
    if !(lat.is_finite() && (-90.0..=90.0).contains(&lat)) {
        return Err(parse_err(provider, format!("latitude out of range: {lat}")));
    }
    if !(lon.is_finite() && (-180.0..=180.0).contains(&lon)) {
        return Err(parse_err(provider, format!("longitude out of range: {lon}")));
    }
    let country = raw.country.unwrap_or_default().trim().to_ascii_uppercase();
    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(parse_err(provider, format!("bad country code {country:?}")));
    }
    let timezone = raw
        .timezone
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok(ProviderAnswer {
        provider: provider.to_string(),
        ip,
        lat,
        lon,
        country,
        timezone,
    })
}

#[derive(Deserialize)]
struct IpwhoisResponse {
    ip: Option<String>,
    success: Option<bool>,
    message: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    country_code: Option<String>,
    timezone: Option<IpwhoisTimezone>,
}

#[derive(Deserialize)]
struct IpwhoisTimezone {
    id: Option<String>,
}

/// Parse an ipwho.is response (`success:false` carries a `message`).
pub(crate) fn parse_ipwhois(body: &str, requested: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
    let r: IpwhoisResponse = from_json(IPWHOIS, body)?;
    if r.success == Some(false) {
        let msg = r.message.unwrap_or_else(|| "unknown error".to_string());
        return Err(parse_err(IPWHOIS, format!("success=false: {msg}")));
    }
    let raw = RawAnswer {
        ip: r.ip,
        lat: r.latitude,
        lon: r.longitude,
        country: r.country_code,
        timezone: r.timezone.and_then(|t| t.id),
    };
    finish(IPWHOIS, raw, requested)
}

#[derive(Deserialize)]
struct IpapiCoResponse {
    ip: Option<String>,
    error: Option<bool>,
    reason: Option<String>,
    message: Option<String>,
    reserved: Option<bool>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    country_code: Option<String>,
    country: Option<String>,
    timezone: Option<String>,
}

/// Parse an ipapi.co response (`error:true` carries `reason`/`message`).
pub(crate) fn parse_ipapi_co(body: &str, requested: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
    let r: IpapiCoResponse = from_json(IPAPI_CO, body)?;
    if r.error == Some(true) {
        let reason = r.reason.unwrap_or_else(|| "unknown".to_string());
        let detail = match r.message {
            Some(m) => format!("error: {reason}: {m}"),
            None => format!("error: {reason}"),
        };
        return Err(parse_err(IPAPI_CO, detail));
    }
    if r.reserved == Some(true) {
        return Err(parse_err(IPAPI_CO, "reserved (private) IP address"));
    }
    let raw = RawAnswer {
        ip: r.ip,
        lat: r.latitude,
        lon: r.longitude,
        country: r.country_code.or(r.country),
        timezone: r.timezone,
    };
    finish(IPAPI_CO, raw, requested)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IpApiResponse {
    status: Option<String>,
    message: Option<String>,
    country_code: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    timezone: Option<String>,
    query: Option<String>,
}

/// Parse an ip-api.com response (`status:"fail"` carries a `message`).
pub(crate) fn parse_ip_api(body: &str, requested: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
    let r: IpApiResponse = from_json(IP_API, body)?;
    match r.status.as_deref() {
        Some("success") => {}
        Some(status) => {
            let msg = r.message.unwrap_or_else(|| "unknown error".to_string());
            return Err(parse_err(IP_API, format!("status={status}: {msg}")));
        }
        None => return Err(parse_err(IP_API, "response has no status")),
    }
    let raw = RawAnswer {
        ip: r.query,
        lat: r.lat,
        lon: r.lon,
        country: r.country_code,
        timezone: r.timezone,
    };
    finish(IP_API, raw, requested)
}

#[derive(Deserialize)]
struct IpinfoResponse {
    ip: Option<String>,
    bogon: Option<bool>,
    country: Option<String>,
    loc: Option<String>,
    timezone: Option<String>,
    error: Option<IpinfoError>,
}

/// ipinfo.io reports errors either as an object or as a bare string.
#[derive(Deserialize)]
#[serde(untagged)]
enum IpinfoError {
    Detailed {
        title: Option<String>,
        message: Option<String>,
    },
    Text(String),
}

impl IpinfoError {
    fn describe(self) -> String {
        match self {
            IpinfoError::Text(t) => t,
            IpinfoError::Detailed { title, message } => [title, message]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": "),
        }
    }
}

/// Parse an ipinfo.io response (`"loc": "lat,lon"`).
pub(crate) fn parse_ipinfo(body: &str, requested: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
    let r: IpinfoResponse = from_json(IPINFO, body)?;
    if let Some(err) = r.error {
        return Err(parse_err(IPINFO, format!("error: {}", err.describe())));
    }
    if r.bogon == Some(true) {
        return Err(parse_err(IPINFO, "bogon (private/reserved) IP address"));
    }
    let loc = r
        .loc
        .ok_or_else(|| parse_err(IPINFO, "response has no loc"))?;
    let (lat, lon) = parse_loc(&loc).ok_or_else(|| parse_err(IPINFO, format!("bad loc {loc:?}")))?;
    let raw = RawAnswer {
        ip: r.ip,
        lat: Some(lat),
        lon: Some(lon),
        country: r.country,
        timezone: r.timezone,
    };
    finish(IPINFO, raw, requested)
}

/// Parse `"52.5200,13.4050"` into `(lat, lon)`.
fn parse_loc(loc: &str) -> Option<(f64, f64)> {
    let (lat, lon) = loc.split_once(',')?;
    Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
}

/// First 200 characters of `s`, for error messages.
fn snippet(s: &str) -> String {
    const MAX: usize = 200;
    match s.char_indices().nth(MAX) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const IPWHOIS_OK: &str = r#"{
        "ip": "185.65.135.230", "success": true, "type": "IPv4",
        "continent": "Europe", "continent_code": "EU", "country": "Germany",
        "country_code": "DE", "region": "Berlin", "city": "Berlin",
        "latitude": 52.5200066, "longitude": 13.404954, "is_eu": true,
        "postal": "10178", "calling_code": "49", "capital": "Berlin",
        "borders": "AT,BE,CH,CZ,DK,FR,LU,NL,PL",
        "flag": {"img": "https://cdn.ipwhois.io/flags/de.svg", "emoji": "x"},
        "connection": {"asn": 39351, "org": "31173 Services AB", "isp": "31173 Services AB", "domain": "31173.se"},
        "timezone": {"id": "Europe/Berlin", "abbr": "CEST", "is_dst": true, "offset": 7200, "utc": "+02:00", "current_time": "2026-10-14T12:00:00+02:00"}
    }"#;

    #[test]
    fn ipwhois_ok() {
        let a = parse_ipwhois(IPWHOIS_OK, None).unwrap();
        assert_eq!(a.provider, "ipwho.is");
        assert_eq!(a.ip, IpAddr::V4(Ipv4Addr::new(185, 65, 135, 230)));
        assert_eq!(a.country, "DE");
        assert!((a.lat - 52.52).abs() < 1e-3);
        assert!((a.lon - 13.405).abs() < 1e-3);
        assert_eq!(a.timezone.as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn ipwhois_failure_message() {
        let body = r#"{"ip":"10.0.0.1","success":false,"message":"Reserved range"}"#;
        let err = parse_ipwhois(body, None).unwrap_err();
        match err {
            GeoError::Parse { provider, detail } => {
                assert_eq!(provider, "ipwho.is");
                assert!(detail.contains("Reserved range"), "{detail}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    const IPAPI_CO_OK: &str = r#"{
        "ip": "185.65.135.230", "network": "185.65.135.0/24", "version": "IPv4",
        "city": "Amsterdam", "region": "North Holland", "region_code": "NH",
        "country": "NL", "country_name": "Netherlands", "country_code": "NL",
        "country_code_iso3": "NLD", "country_capital": "Amsterdam", "country_tld": ".nl",
        "continent_code": "EU", "in_eu": true, "postal": "1012",
        "latitude": 52.3759, "longitude": 4.8975, "timezone": "Europe/Amsterdam",
        "utc_offset": "+0200", "country_calling_code": "+31", "currency": "EUR",
        "languages": "nl-NL,fy-NL", "asn": "AS39351", "org": "31173 Services AB"
    }"#;

    #[test]
    fn ipapi_co_ok() {
        let a = parse_ipapi_co(IPAPI_CO_OK, None).unwrap();
        assert_eq!(a.provider, "ipapi.co");
        assert_eq!(a.country, "NL");
        assert!((a.lat - 52.3759).abs() < 1e-9);
        assert!((a.lon - 4.8975).abs() < 1e-9);
        assert_eq!(a.timezone.as_deref(), Some("Europe/Amsterdam"));
    }

    #[test]
    fn ipapi_co_error_reason() {
        let body = r#"{"error": true, "reason": "RateLimited", "message": "Visit https://ipapi.co/ratelimited/ for details"}"#;
        let err = parse_ipapi_co(body, None).unwrap_err();
        assert!(matches!(&err, GeoError::Parse { detail, .. } if detail.contains("RateLimited")), "{err:?}");
    }

    #[test]
    fn ipapi_co_reserved() {
        let body = r#"{"ip": "10.0.0.1", "reserved": true, "version": "IPv4"}"#;
        assert!(matches!(parse_ipapi_co(body, None), Err(GeoError::Parse { .. })));
    }

    const IP_API_OK: &str = r#"{"status":"success","country":"United States","countryCode":"US","lat":40.7128,"lon":-74.006,"timezone":"America/New_York","query":"2001:db8::7"}"#;

    #[test]
    fn ip_api_ok_ipv6() {
        let a = parse_ip_api(IP_API_OK, None).unwrap();
        assert_eq!(a.provider, "ip-api.com");
        assert_eq!(a.ip, "2001:db8::7".parse::<IpAddr>().unwrap());
        assert_eq!(a.country, "US");
        assert!((a.lon + 74.006).abs() < 1e-9);
        assert_eq!(a.timezone.as_deref(), Some("America/New_York"));
    }

    #[test]
    fn ip_api_fail_status() {
        let body = r#"{"status":"fail","message":"private range","query":"192.168.1.1"}"#;
        let err = parse_ip_api(body, None).unwrap_err();
        assert!(
            matches!(&err, GeoError::Parse { detail, .. } if detail.contains("status=fail") && detail.contains("private range")),
            "{err:?}"
        );
    }

    const IPINFO_OK: &str = r#"{
        "ip": "185.65.135.230", "city": "Stockholm", "region": "Stockholm",
        "country": "SE", "loc": "59.3294,18.0687", "org": "AS39351 31173 Services AB",
        "postal": "100 05", "timezone": "Europe/Stockholm",
        "readme": "https://ipinfo.io/missingauth"
    }"#;

    #[test]
    fn ipinfo_ok() {
        let a = parse_ipinfo(IPINFO_OK, None).unwrap();
        assert_eq!(a.provider, "ipinfo.io");
        assert_eq!(a.country, "SE");
        assert!((a.lat - 59.3294).abs() < 1e-9);
        assert!((a.lon - 18.0687).abs() < 1e-9);
        assert_eq!(a.timezone.as_deref(), Some("Europe/Stockholm"));
    }

    #[test]
    fn ipinfo_error_object_and_bogon() {
        let body = r#"{"status": 404, "error": {"title": "Wrong ip", "message": "Please provide a valid IP address"}}"#;
        let err = parse_ipinfo(body, None).unwrap_err();
        assert!(matches!(&err, GeoError::Parse { detail, .. } if detail.contains("Wrong ip")), "{err:?}");

        let bogon = r#"{"ip": "10.0.0.1", "bogon": true}"#;
        assert!(matches!(parse_ipinfo(bogon, None), Err(GeoError::Parse { .. })));
    }

    #[test]
    fn ipinfo_bad_loc() {
        let body = r#"{"ip": "1.2.3.4", "country": "SE", "loc": "nope"}"#;
        assert!(matches!(parse_ipinfo(body, None), Err(GeoError::Parse { .. })));
    }

    #[test]
    fn missing_ip_uses_requested() {
        let body = r#"{"status":"success","countryCode":"de","lat":52.5,"lon":13.4}"#;
        let requested: IpAddr = "203.0.113.9".parse().unwrap();
        let a = parse_ip_api(body, Some(requested)).unwrap();
        assert_eq!(a.ip, requested);
        assert_eq!(a.country, "DE");
        assert_eq!(a.timezone, None);
        assert!(parse_ip_api(body, None).is_err());
    }

    #[test]
    fn rejects_invalid_values() {
        let bad_lat = r#"{"status":"success","countryCode":"DE","lat":123.0,"lon":13.4,"query":"1.2.3.4"}"#;
        assert!(parse_ip_api(bad_lat, None).is_err());
        let bad_country = r#"{"status":"success","countryCode":"Germany","lat":52.0,"lon":13.4,"query":"1.2.3.4"}"#;
        assert!(parse_ip_api(bad_country, None).is_err());
        assert!(parse_ip_api("<html>rate limited</html>", None).is_err());
    }

    #[test]
    fn urls() {
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(ipwhois_url(Some(ip)), "https://ipwho.is/1.2.3.4");
        assert_eq!(ipwhois_url(None), "https://ipwho.is/");
        assert_eq!(ipapi_co_url(Some(ip)), "https://ipapi.co/1.2.3.4/json/");
        assert_eq!(ipapi_co_url(None), "https://ipapi.co/json/");
        assert!(ip_api_url(Some(ip)).starts_with("http://ip-api.com/json/1.2.3.4?fields="));
        assert_eq!(ipinfo_url(Some(ip)), "https://ipinfo.io/1.2.3.4/json");
        assert_eq!(ipinfo_url(None), "https://ipinfo.io/json");
    }

    #[test]
    fn plain_ip_parsing() {
        assert_eq!(
            parse_plain_ip("203.0.113.7\n").unwrap(),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
        );
        assert!(parse_plain_ip("2001:db8::1").unwrap().is_ipv6());
        assert!(parse_plain_ip("<html>").is_err());
    }

    #[test]
    fn default_providers_names() {
        let names: Vec<String> = default_providers().iter().map(|p| p.name().to_string()).collect();
        assert_eq!(names, vec!["ipwho.is", "ipapi.co", "ip-api.com", "ipinfo.io"]);
    }

    #[test]
    fn provider_client_cap_allows_cli_maximum_timeout() {
        // `--provider-timeout-secs` accepts up to 120; the client must not cut it short.
        assert!(PROVIDER_CLIENT_TIMEOUT >= Duration::from_secs(120));
        assert!(build_client(PROVIDER_CLIENT_TIMEOUT, Family::Any).is_ok());
        assert!(build_client(Duration::from_secs(5), Family::V4).is_ok());
    }

    #[tokio::test]
    async fn public_ip_prefers_ipv4_and_does_not_race_families() {
        let calls = std::sync::Mutex::new(Vec::new());
        let ip = public_ip_with(|family| {
            calls.lock().unwrap().push(family);
            async move {
                match family {
                    Family::V4 => Ok("185.65.135.230".parse::<IpAddr>().unwrap()),
                    Family::Any => Ok("2a03:1b20::1".parse::<IpAddr>().unwrap()),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(ip, "185.65.135.230".parse::<IpAddr>().unwrap());
        assert_eq!(*calls.lock().unwrap(), vec![Family::V4]);
    }

    #[tokio::test]
    async fn public_ip_falls_back_to_any_family() {
        let ip = public_ip_with(|family| async move {
            match family {
                Family::V4 => Err("no IPv4 route".to_string()),
                Family::Any => Ok("2a03:1b20::1".parse::<IpAddr>().unwrap()),
            }
        })
        .await
        .unwrap();
        assert!(ip.is_ipv6());

        let err = public_ip_with(|family| async move { Err::<IpAddr, _>(format!("{family:?} down")) })
            .await
            .unwrap_err();
        match err {
            GeoError::Http { provider, detail } => {
                assert_eq!(provider, "public-ip");
                assert!(detail.contains("V4 down") && detail.contains("Any down"), "{detail}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn user_agent_has_version() {
        assert!(USER_AGENT.starts_with("geoveil/"));
        assert!(USER_AGENT.len() > "geoveil/".len());
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long = "é".repeat(300);
        let s = snippet(&long);
        assert!(s.ends_with("..."));
        assert_eq!(s.chars().count(), 203);
    }
}
