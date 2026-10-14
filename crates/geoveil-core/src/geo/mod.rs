//! IP geolocation: query several free providers, vote on the answer, and turn
//! the consensus into a full [`Profile`].
//!
//! All HTTP requests go out through the system route, which means through the
//! user's VPN tunnel when one is up. Geoveil never provides the tunnel itself.

mod providers;

pub use providers::{default_providers, public_ip};

use crate::types::{Profile, ProfileSource};
use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

/// Errors from IP geolocation.
#[derive(Debug, thiserror::Error)]
pub enum GeoError {
    /// Transport-level failure (connect, TLS, timeout, non-2xx status).
    #[error("{provider}: HTTP error: {detail}")]
    Http { provider: String, detail: String },
    /// The provider answered but the payload was unusable or reported an error.
    #[error("{provider}: bad response: {detail}")]
    Parse { provider: String, detail: String },
    /// Providers answered but did not agree well enough to trust the result.
    #[error("no consensus between geolocation providers: {detail}")]
    NoConsensus { detail: String },
    /// No provider produced a usable answer.
    #[error("no geolocation provider returned an answer")]
    NoAnswers,
}

/// One provider's view of where an IP is.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAnswer {
    /// Provider name, e.g. "ipwho.is".
    pub provider: String,
    /// The IP the provider geolocated.
    pub ip: IpAddr,
    pub lat: f64,
    pub lon: f64,
    /// ISO-3166 alpha-2, upper case.
    pub country: String,
    /// IANA zone as reported by the provider (not trusted as primary source).
    pub timezone: Option<String>,
}

/// A source of IP geolocation answers.
#[async_trait::async_trait]
pub trait IpGeoProvider: Send + Sync {
    /// Stable, human-readable provider name.
    fn name(&self) -> &str;
    /// Geolocate `ip`, or the caller's own public IP when `ip` is `None`
    /// (in which case the request itself travels through the VPN).
    async fn lookup(&self, ip: Option<IpAddr>) -> Result<ProviderAnswer, GeoError>;
}

/// The agreed-upon location from a set of provider answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Consensus {
    pub ip: IpAddr,
    pub lat: f64,
    pub lon: f64,
    /// ISO-3166 alpha-2, upper case.
    pub country: String,
    /// Most common timezone reported by the agreeing providers.
    pub provider_timezone: Option<String>,
    /// Names of providers whose country matched the majority.
    pub agreeing: Vec<String>,
    /// Names of providers that reported a different (or no) country.
    pub disagreeing: Vec<String>,
}

/// Tuning knobs for [`resolve`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Per-provider timeout.
    pub timeout: Duration,
    /// Minimum number of agreeing providers (capped at the provider count).
    pub min_agree: usize,
    /// Accuracy in metres written into the resulting profile.
    pub accuracy_m: f64,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            min_agree: 2,
            accuracy_m: 5000.0,
        }
    }
}

/// Majority-vote provider answers.
///
/// 1. Answers are grouped by the IP they describe and only one IP group is
///    kept: the largest IPv4 group, or the largest IPv6 group when no provider
///    saw an IPv4 address (ties go to the first seen). When the caller's own
///    IP is looked up, providers reached over IPv6 may see a different address
///    than those reached over IPv4 (e.g. a VPN that only tunnels IPv4, where
///    the IPv6 address is the user's real one), so answers about different
///    IPs must never be pooled.
/// 2. Within that group the winning country must have strictly more answers
///    than any other country; ties yield [`GeoError::NoConsensus`].
/// 3. Coordinates are those of the medoid agreeing answer (the one closest,
///    in total great-circle distance, to all the others), so the result is
///    always a point some provider actually reported.
pub fn vote(answers: &[ProviderAnswer]) -> Result<Consensus, GeoError> {
    if answers.is_empty() {
        return Err(GeoError::NoAnswers);
    }
    let ip = preferred_ip(answers).ok_or(GeoError::NoAnswers)?;
    let same_ip: Vec<&ProviderAnswer> = answers.iter().filter(|a| a.ip == ip).collect();
    let groups = group_by_country(&same_ip);
    let (country, members) = majority_group(&groups)?;

    let (lat, lon) = medoid(members).ok_or(GeoError::NoAnswers)?;
    let provider_timezone = most_common(members.iter().filter_map(|a| a.timezone.clone()));

    let agreeing: Vec<String> = members.iter().map(|a| a.provider.clone()).collect();
    let disagreeing: Vec<String> = answers
        .iter()
        .filter(|a| a.ip != ip || normalize_country(&a.country) != country)
        .map(|a| a.provider.clone())
        .collect();

    Ok(Consensus {
        ip,
        lat,
        lon,
        country,
        provider_timezone,
        agreeing,
        disagreeing,
    })
}

/// Query all providers concurrently and build a [`Profile`] for the consensus
/// location of `ip` (or of the caller's public IP when `None`).
///
/// Timezone comes from offline boundary data, falling back to the providers'
/// majority timezone and finally to `"UTC"`.
pub async fn resolve(
    providers: &[Box<dyn IpGeoProvider>],
    ip: Option<IpAddr>,
    opts: &ResolveOptions,
) -> Result<Profile, GeoError> {
    let answers = collect_answers(providers, ip, opts.timeout).await;
    if answers.is_empty() {
        return Err(GeoError::NoAnswers);
    }
    let consensus = vote(&answers)?;
    log_disagreements(&answers, &consensus);

    let required = opts.min_agree.min(providers.len()).max(1);
    if consensus.agreeing.len() < required {
        return Err(GeoError::NoConsensus {
            detail: format!(
                "only {} provider(s) agree on {} ({}), need {required}",
                consensus.agreeing.len(),
                consensus.country,
                consensus.agreeing.join(", ")
            ),
        });
    }
    Ok(profile_from_consensus(consensus, opts.accuracy_m))
}

/// Build a profile from user-supplied coordinates, without any network access.
///
/// Country is left empty (there is no offline country lookup), so the locale
/// defaults to `"en-US"`.
pub fn manual_profile(lat: f64, lon: f64, accuracy_m: f64) -> Profile {
    Profile {
        lat,
        lon,
        accuracy_m,
        timezone: crate::timezone::tz_for_coords(lat, lon).unwrap_or_else(|| "UTC".to_string()),
        locale: "en-US".to_string(),
        country: String::new(),
        source: ProfileSource::Manual,
    }
}

// ---------------------------------------------------------------------------
// resolve helpers
// ---------------------------------------------------------------------------

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Run every provider with its own timeout; return the successful answers and
/// log the failures.
async fn collect_answers(
    providers: &[Box<dyn IpGeoProvider>],
    ip: Option<IpAddr>,
    timeout: Duration,
) -> Vec<ProviderAnswer> {
    let futs: Vec<BoxFuture<'_, Result<ProviderAnswer, GeoError>>> = providers
        .iter()
        .map(|p| Box::pin(lookup_with_timeout(p.as_ref(), ip, timeout)) as BoxFuture<'_, _>)
        .collect();

    join_all(futs)
        .await
        .into_iter()
        .filter_map(|r| match r {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(error = %e, "geolocation provider failed");
                None
            }
        })
        .collect()
}

async fn lookup_with_timeout(
    provider: &dyn IpGeoProvider,
    ip: Option<IpAddr>,
    timeout: Duration,
) -> Result<ProviderAnswer, GeoError> {
    match tokio::time::timeout(timeout, provider.lookup(ip)).await {
        Ok(result) => result,
        Err(_) => Err(GeoError::Http {
            provider: provider.name().to_string(),
            detail: format!("timed out after {timeout:?}"),
        }),
    }
}

fn log_disagreements(answers: &[ProviderAnswer], consensus: &Consensus) {
    for a in answers
        .iter()
        .filter(|a| consensus.disagreeing.contains(&a.provider))
    {
        tracing::warn!(
            provider = %a.provider,
            ip = %a.ip,
            reported = %a.country,
            consensus_ip = %consensus.ip,
            consensus = %consensus.country,
            "geolocation provider disagrees with consensus (other IP or country)"
        );
    }
}

fn profile_from_consensus(c: Consensus, accuracy_m: f64) -> Profile {
    let timezone = crate::timezone::tz_for_coords(c.lat, c.lon)
        .or_else(|| {
            c.provider_timezone
                .clone()
                .filter(|tz| crate::timezone::is_valid_iana(tz))
        })
        .unwrap_or_else(|| "UTC".to_string());
    Profile {
        lat: c.lat,
        lon: c.lon,
        accuracy_m,
        timezone,
        locale: crate::locale::locale_for_country(&c.country),
        country: c.country,
        source: ProfileSource::VpnExit { ip: c.ip },
    }
}

/// Await all futures concurrently, returning outputs in input order.
///
/// Minimal replacement for `futures::future::join_all`.
pub(crate) async fn join_all<'a, T>(futs: Vec<BoxFuture<'a, T>>) -> Vec<T> {
    let mut slots: Vec<Option<BoxFuture<'a, T>>> = futs.into_iter().map(Some).collect();
    let mut outputs: Vec<Option<T>> = slots.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut pending = false;
        for (slot, out) in slots.iter_mut().zip(outputs.iter_mut()) {
            if let Some(fut) = slot {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(v) => {
                        *out = Some(v);
                        *slot = None;
                    }
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(outputs.drain(..).flatten().collect())
        }
    })
    .await
}

/// Await futures concurrently and return the first `Ok`; if all fail, return
/// every error in input order.
pub(crate) async fn first_ok<'a, T, E>(futs: Vec<BoxFuture<'a, Result<T, E>>>) -> Result<T, Vec<E>> {
    let mut slots: Vec<Option<BoxFuture<'a, Result<T, E>>>> = futs.into_iter().map(Some).collect();
    let mut errors: Vec<Option<E>> = slots.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut pending = false;
        for (slot, err) in slots.iter_mut().zip(errors.iter_mut()) {
            if let Some(fut) = slot {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(v)) => return Poll::Ready(Ok(v)),
                    Poll::Ready(Err(e)) => {
                        *err = Some(e);
                        *slot = None;
                    }
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(Err(errors.drain(..).flatten().collect()))
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// vote helpers
// ---------------------------------------------------------------------------

fn normalize_country(country: &str) -> String {
    country.trim().to_ascii_uppercase()
}

/// Group answers by normalized country, preserving first-seen order.
/// Answers without a country are left out (they count as disagreeing).
fn group_by_country<'a>(answers: &[&'a ProviderAnswer]) -> Vec<(String, Vec<&'a ProviderAnswer>)> {
    let mut groups: Vec<(String, Vec<&ProviderAnswer>)> = Vec::new();
    for &a in answers {
        let country = normalize_country(&a.country);
        if country.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(c, _)| *c == country) {
            Some((_, members)) => members.push(a),
            None => groups.push((country, vec![a])),
        }
    }
    groups
}

fn majority_group<'a>(
    groups: &'a [(String, Vec<&'a ProviderAnswer>)],
) -> Result<(String, &'a [&'a ProviderAnswer]), GeoError> {
    let best = groups.iter().map(|(_, m)| m.len()).max().unwrap_or(0);
    let mut leaders = groups.iter().filter(|(_, m)| m.len() == best);
    match (leaders.next(), leaders.next()) {
        (Some((country, members)), None) => Ok((country.clone(), members.as_slice())),
        (None, _) => Err(GeoError::NoConsensus {
            detail: "no provider reported a country".to_string(),
        }),
        _ => Err(GeoError::NoConsensus {
            detail: format!("tie between countries: {}", describe_groups(groups)),
        }),
    }
}

fn describe_groups(groups: &[(String, Vec<&ProviderAnswer>)]) -> String {
    groups
        .iter()
        .map(|(c, m)| {
            let names: Vec<&str> = m.iter().map(|a| a.provider.as_str()).collect();
            format!("{c}=[{}]", names.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mean Earth radius in metres (for great-circle distances).
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Great-circle distance in metres between two WGS-84 points (haversine).
/// Correct across the antimeridian.
fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

/// Coordinates of the medoid: the answer with the smallest total distance to
/// all the others (ties go to the first seen). Unlike a per-axis median or
/// mean, this never invents a point no provider reported (e.g. Kansas from
/// New York + Los Angeles, or longitude 0 from two answers either side of the
/// antimeridian).
fn medoid(members: &[&ProviderAnswer]) -> Option<(f64, f64)> {
    let total = |a: &ProviderAnswer| -> f64 {
        members
            .iter()
            .map(|b| distance_m(a.lat, a.lon, b.lat, b.lon))
            .sum()
    };
    let mut best: Option<(&ProviderAnswer, f64)> = None;
    for &a in members {
        let d = total(a);
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((a, d));
        }
    }
    best.map(|(a, _)| (a.lat, a.lon))
}

/// The IP whose answers are voted on: the largest IPv4 group, else the
/// largest IPv6 group; ties go to the first seen.
fn preferred_ip(answers: &[ProviderAnswer]) -> Option<IpAddr> {
    let mut counts: Vec<(IpAddr, usize)> = Vec::new();
    for a in answers {
        match counts.iter_mut().find(|(ip, _)| *ip == a.ip) {
            Some((_, n)) => *n += 1,
            None => counts.push((a.ip, 1)),
        }
    }
    let mut best: Option<(IpAddr, usize)> = None;
    for (ip, n) in counts {
        let better = match best {
            None => true,
            Some((bip, bn)) => {
                (ip.is_ipv4() && !bip.is_ipv4()) || (ip.is_ipv4() == bip.is_ipv4() && n > bn)
            }
        };
        if better {
            best = Some((ip, n));
        }
    }
    best.map(|(ip, _)| ip)
}

/// Most common string; ties go to the first seen.
fn most_common(items: impl Iterator<Item = String>) -> Option<String> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items {
        let n = counts.entry(item.clone()).or_insert(0);
        if *n == 0 {
            order.push(item);
        }
        *n += 1;
    }
    let mut best: Option<(String, usize)> = None;
    for item in order {
        let n = counts.get(&item).copied().unwrap_or(0);
        if best.as_ref().map_or(true, |(_, bn)| n > *bn) {
            best = Some((item, n));
        }
    }
    best.map(|(s, _)| s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ans(provider: &str, ip: &str, lat: f64, lon: f64, country: &str, tz: Option<&str>) -> ProviderAnswer {
        ProviderAnswer {
            provider: provider.to_string(),
            ip: ip.parse().unwrap(),
            lat,
            lon,
            country: country.to_string(),
            timezone: tz.map(str::to_string),
        }
    }

    #[test]
    fn vote_majority_country_wins() {
        let answers = vec![
            ans("a", "1.2.3.4", 52.5, 13.4, "DE", Some("Europe/Berlin")),
            ans("b", "1.2.3.4", 52.4, 13.3, "de", Some("Europe/Berlin")),
            ans("c", "1.2.3.4", 52.3, 4.9, "NL", Some("Europe/Amsterdam")),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!(c.country, "DE");
        assert_eq!(c.agreeing, vec!["a", "b"]);
        assert_eq!(c.disagreeing, vec!["c"]);
        assert_eq!(c.provider_timezone.as_deref(), Some("Europe/Berlin"));
        assert_eq!(c.ip, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn vote_tie_is_no_consensus() {
        let answers = vec![
            ans("a", "1.2.3.4", 52.5, 13.4, "DE", None),
            ans("b", "1.2.3.4", 52.3, 4.9, "NL", None),
        ];
        assert!(matches!(vote(&answers), Err(GeoError::NoConsensus { .. })));
    }

    #[test]
    fn vote_empty_is_no_answers() {
        assert!(matches!(vote(&[]), Err(GeoError::NoAnswers)));
    }

    #[test]
    fn vote_ignores_empty_country_for_majority() {
        let answers = vec![
            ans("a", "1.2.3.4", 52.5, 13.4, "DE", None),
            ans("b", "1.2.3.4", 0.0, 0.0, "", None),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!(c.country, "DE");
        assert_eq!(c.disagreeing, vec!["b"]);
    }

    #[test]
    fn vote_uses_medoid_not_per_axis_median() {
        // A per-axis median would pair a's latitude with b's longitude.
        let answers = vec![
            ans("a", "1.2.3.4", 50.0, 10.0, "DE", None),
            ans("b", "1.2.3.4", 52.0, 14.0, "DE", None),
            ans("c", "1.2.3.4", 51.0, 99.0, "DE", None),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!((c.lat, c.lon), (52.0, 14.0));

        // New York + Los Angeles must not become Kansas.
        let us = vec![
            ans("ny", "1.2.3.4", 40.7, -74.0, "US", None),
            ans("la", "1.2.3.4", 34.0, -118.2, "US", None),
        ];
        let c = vote(&us).unwrap();
        assert_eq!((c.lat, c.lon), (40.7, -74.0));
    }

    #[test]
    fn vote_medoid_across_antimeridian() {
        let fiji = vec![
            ans("a", "1.2.3.4", -17.8, 179.9, "FJ", None),
            ans("b", "1.2.3.4", -17.7, -179.9, "FJ", None),
            ans("c", "1.2.3.4", -17.75, 179.95, "FJ", None),
        ];
        let c = vote(&fiji).unwrap();
        assert_eq!(c.lon, 179.95);
    }

    #[test]
    fn distance_is_sane() {
        let berlin_paris = distance_m(52.52, 13.405, 48.8566, 2.3522);
        assert!((berlin_paris - 878_000.0).abs() < 10_000.0, "{berlin_paris}");
        let across = distance_m(0.0, 179.9, 0.0, -179.9);
        assert!(across < 25_000.0, "{across}");
        assert_eq!(distance_m(10.0, 10.0, 10.0, 10.0), 0.0);
    }

    #[test]
    fn vote_prefers_ipv4_on_tie() {
        let answers = vec![
            ans("a", "2001:db8::1", 52.5, 13.4, "DE", None),
            ans("b", "5.6.7.8", 52.5, 13.4, "DE", None),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!(c.ip, IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)));
        assert_eq!(c.agreeing, vec!["b"]);
        assert_eq!(c.disagreeing, vec!["a"]);
    }

    #[test]
    fn vote_never_pools_answers_about_different_ips() {
        // IPv4-only VPN: three providers reached over IPv6 see the user's real
        // ISP address in Canada, one sees the VPN's IPv4 exit in Germany.
        let answers = vec![
            ans("ipwho.is", "2001:db8::1", 45.5, -73.6, "CA", None),
            ans("ipapi.co", "2001:db8::1", 45.5, -73.6, "CA", None),
            ans("ipinfo.io", "2001:db8::1", 45.5, -73.6, "CA", None),
            ans("ip-api.com", "185.65.135.230", 52.5, 13.4, "DE", None),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!(c.ip, "185.65.135.230".parse::<IpAddr>().unwrap());
        assert_eq!(c.country, "DE");
        assert_eq!(c.agreeing, vec!["ip-api.com"]);
        assert_eq!(c.disagreeing, vec!["ipwho.is", "ipapi.co", "ipinfo.io"]);
    }

    #[test]
    fn vote_largest_ipv6_group_when_no_ipv4() {
        let answers = vec![
            ans("a", "2001:db8::1", 52.5, 13.4, "DE", None),
            ans("b", "2001:db8::2", 52.5, 13.4, "DE", None),
            ans("c", "2001:db8::2", 52.5, 13.4, "DE", None),
        ];
        let c = vote(&answers).unwrap();
        assert_eq!(c.ip, IpAddr::V6("2001:db8::2".parse::<Ipv6Addr>().unwrap()));
        assert_eq!(c.agreeing, vec!["b", "c"]);
    }

    #[test]
    fn default_options() {
        let o = ResolveOptions::default();
        assert_eq!(o.timeout, Duration::from_secs(5));
        assert_eq!(o.min_agree, 2);
        assert_eq!(o.accuracy_m, 5000.0);
    }

    #[test]
    fn manual_profile_fields() {
        let p = manual_profile(52.52, 13.405, 100.0);
        assert_eq!(p.lat, 52.52);
        assert_eq!(p.lon, 13.405);
        assert_eq!(p.accuracy_m, 100.0);
        assert_eq!(p.country, "");
        assert_eq!(p.locale, "en-US");
        assert_eq!(p.source, ProfileSource::Manual);
        assert!(!p.timezone.is_empty());
    }

    #[test]
    fn manual_profile_invalid_coords_fall_back_to_utc() {
        let p = manual_profile(123.0, 500.0, 10.0);
        assert_eq!(p.timezone, "UTC");
    }

    // ---- fake providers -----------------------------------------------------

    struct Fake {
        name: &'static str,
        answer: Result<ProviderAnswer, &'static str>,
        delay: Duration,
    }

    impl Fake {
        fn ok(name: &'static str, country: &str, lat: f64, lon: f64) -> Box<dyn IpGeoProvider> {
            Box::new(Fake {
                name,
                answer: Ok(ans(name, "9.9.9.9", lat, lon, country, Some("Europe/Berlin"))),
                delay: Duration::ZERO,
            })
        }
        fn failing(name: &'static str) -> Box<dyn IpGeoProvider> {
            Box::new(Fake { name, answer: Err("boom"), delay: Duration::ZERO })
        }
        fn slow(name: &'static str, delay: Duration) -> Box<dyn IpGeoProvider> {
            Box::new(Fake {
                name,
                answer: Ok(ans(name, "9.9.9.9", 52.52, 13.405, "DE", Some("Europe/Berlin"))),
                delay,
            })
        }
    }

    #[async_trait::async_trait]
    impl IpGeoProvider for Fake {
        fn name(&self) -> &str {
            self.name
        }
        async fn lookup(&self, _ip: Option<IpAddr>) -> Result<ProviderAnswer, GeoError> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.answer.clone().map_err(|d| GeoError::Http {
                provider: self.name.to_string(),
                detail: d.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn resolve_builds_profile_from_consensus() {
        let providers = vec![
            Fake::ok("a", "DE", 52.52, 13.405),
            Fake::ok("b", "DE", 52.50, 13.40),
            Fake::ok("c", "NL", 52.37, 4.89),
            Fake::failing("d"),
        ];
        let p = resolve(&providers, None, &ResolveOptions::default()).await.unwrap();
        assert_eq!(p.country, "DE");
        assert_eq!(p.locale, "de-DE");
        assert_eq!(p.timezone, "Europe/Berlin");
        assert_eq!(p.accuracy_m, 5000.0);
        // Medoid of two answers is the first one, never an invented midpoint.
        assert_eq!((p.lat, p.lon), (52.52, 13.405));
        assert_eq!(
            p.source,
            ProfileSource::VpnExit { ip: IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)) }
        );
    }

    #[tokio::test]
    async fn resolve_none_with_mixed_ips_does_not_apply_real_location() {
        // Only one provider saw the IPv4 exit: not enough agreement, so the
        // real (IPv6) location is never applied.
        let fake = |name: &'static str, ip: &str, lat: f64, lon: f64, country: &str| -> Box<dyn IpGeoProvider> {
            Box::new(Fake { name, answer: Ok(ans(name, ip, lat, lon, country, None)), delay: Duration::ZERO })
        };
        let providers = vec![
            fake("a", "2001:db8::1", 45.5, -73.6, "CA"),
            fake("b", "2001:db8::1", 45.5, -73.6, "CA"),
            fake("c", "2001:db8::1", 45.5, -73.6, "CA"),
            fake("d", "185.65.135.230", 52.5, 13.4, "DE"),
        ];
        let err = resolve(&providers, None, &ResolveOptions::default()).await.unwrap_err();
        assert!(matches!(err, GeoError::NoConsensus { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_requires_min_agree() {
        let providers = vec![
            Fake::ok("a", "DE", 52.52, 13.405),
            Fake::failing("b"),
            Fake::failing("c"),
        ];
        let err = resolve(&providers, None, &ResolveOptions::default()).await.unwrap_err();
        assert!(matches!(err, GeoError::NoConsensus { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_single_provider_caps_min_agree() {
        let providers = vec![Fake::ok("a", "DE", 52.52, 13.405)];
        let p = resolve(&providers, None, &ResolveOptions::default()).await.unwrap();
        assert_eq!(p.country, "DE");
    }

    #[tokio::test]
    async fn resolve_all_failing_is_no_answers() {
        let providers = vec![Fake::failing("a"), Fake::failing("b")];
        let err = resolve(&providers, None, &ResolveOptions::default()).await.unwrap_err();
        assert!(matches!(err, GeoError::NoAnswers), "{err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_times_out_slow_provider() {
        let providers = vec![
            Fake::ok("a", "DE", 52.52, 13.405),
            Fake::ok("b", "DE", 52.50, 13.40),
            Fake::slow("slow", Duration::from_secs(60)),
        ];
        let start = tokio::time::Instant::now();
        let p = resolve(&providers, None, &ResolveOptions::default()).await.unwrap();
        assert_eq!(p.country, "DE");
        // The slow provider was cut off at the 5 s timeout, not awaited for 60 s.
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    fn bx<'a, T>(f: impl Future<Output = T> + Send + 'a) -> BoxFuture<'a, T> {
        Box::pin(f)
    }

    #[tokio::test]
    async fn join_all_preserves_order() {
        let futs = vec![
            bx(async { 1 }),
            bx(async {
                tokio::task::yield_now().await;
                2
            }),
            bx(async { 3 }),
        ];
        assert_eq!(join_all(futs).await, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn first_ok_returns_first_success_or_all_errors() {
        let futs: Vec<BoxFuture<'_, Result<u32, &str>>> =
            vec![bx(async { Err("x") }), bx(async { Ok(7) })];
        assert_eq!(first_ok(futs).await, Ok(7));

        let futs: Vec<BoxFuture<'_, Result<u32, &str>>> =
            vec![bx(async { Err("x") }), bx(async { Err("y") })];
        assert_eq!(first_ok(futs).await, Err(vec!["x", "y"]));
    }
}
