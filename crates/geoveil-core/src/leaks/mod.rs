//! Leak checks: does traffic or identifying information escape the VPN?
//!
//! This module is the pure half of `geoveil leaks`. The platform crate and
//! the CLI gather raw observations ([`LeakInputs`]: interfaces, the public
//! exit IP, an IPv6 exit probe, STUN probes, DNS resolvers, optionally an
//! active DNS test and the orchestrator's verify results), and [`evaluate`]
//! turns them into a [`LeakReport`] with one [`CheckResult`] per check:
//!
//! | Check | Leak when |
//! |---|---|
//! | [`CheckId::Vpn`] | never; reports the VPN interfaces and the exit IP |
//! | [`CheckId::Ipv6`] | a VPN is up and an IPv6-only endpoint sees one of this machine's own (non-VPN) global IPv6 addresses |
//! | [`CheckId::Webrtc`] | a VPN is up and a STUN request bound to a physical interface gets an answer with a public address other than the VPN exit |
//! | [`CheckId::Dns`] | a VPN is up and a public resolver is configured only on a non-VPN interface |
//! | [`CheckId::DnsActive`] | (opt-in) a resolver seen by the unique-name test is one of this machine's own addresses |
//! | [`CheckId::Verify`] | never; reports whether an engaged profile still matches the system |
//!
//! Rules deliberately avoid geolocation where a plain address comparison can
//! decide, because free geolocation providers are often unavailable.
//!
//! When no local VPN interface is found the VPN may still be upstream (on the
//! router) or a proxy; the host then cannot see the real ISP address, so the
//! checks report `info` instead of claiming "no leak".

pub mod dnstest;
pub mod net;
pub mod stun;

use crate::types::{NetInterface, VerifyReport};
use crate::vpn::VpnDetector;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Outcome of one check, from best to worst for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Checked, nothing leaks.
    Pass,
    /// Informational; nothing to compare against (for example no local VPN).
    Info,
    /// The check does not apply (for example no global IPv6).
    NotApplicable,
    /// Suspicious, but not proof of a leak.
    Warn,
    /// Real information escapes the VPN.
    Leak,
    /// The check could not be completed.
    Error,
}

impl Verdict {
    /// Short upper-case label for the human table.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Info => "INFO",
            Verdict::NotApplicable => "N/A",
            Verdict::Warn => "WARN",
            Verdict::Leak => "LEAK",
            Verdict::Error => "ERROR",
        }
    }
}

/// Which check a result belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    Vpn,
    Ipv6,
    Webrtc,
    Dns,
    DnsActive,
    Verify,
}

impl CheckId {
    /// Stable short name, used in the human table.
    pub fn name(self) -> &'static str {
        match self {
            CheckId::Vpn => "vpn",
            CheckId::Ipv6 => "ipv6",
            CheckId::Webrtc => "webrtc",
            CheckId::Dns => "dns",
            CheckId::DnsActive => "dns-active",
            CheckId::Verify => "verify",
        }
    }
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: CheckId,
    pub verdict: Verdict,
    /// One line for the table.
    pub summary: String,
    /// Supporting observations, one per line.
    pub details: Vec<String>,
    /// What the user can do about a `warn` or `leak`.
    pub advice: Option<String>,
}

impl CheckResult {
    fn new(id: CheckId, verdict: Verdict, summary: impl Into<String>) -> Self {
        Self { id, verdict, summary: summary.into(), details: Vec::new(), advice: None }
    }

    fn details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    fn advice(mut self, advice: &str) -> Self {
        self.advice = Some(advice.to_string());
        self
    }
}

/// A network interface with what the leak checks need beyond [`NetInterface`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeakInterface {
    /// OS interface index (0 when unknown).
    pub index: u32,
    pub name: String,
    /// Friendly name and description joined with `" | "` (see
    /// `geoveil_platform::interfaces`).
    pub description: String,
    pub is_up: bool,
    pub is_loopback: bool,
    /// The OS reports a default gateway on this interface.
    pub has_gateway: bool,
    pub addrs: Vec<IpAddr>,
}

impl LeakInterface {
    /// The plain interface record used by VPN detection.
    pub fn to_net_interface(&self) -> NetInterface {
        NetInterface {
            name: self.name.clone(),
            description: self.description.clone(),
            is_up: self.is_up,
            addrs: self.addrs.clone(),
        }
    }

    /// Whether this looks like a VPN tunnel (see [`crate::vpn::is_vpn_interface`]).
    pub fn is_vpn(&self) -> bool {
        crate::vpn::is_vpn_interface(&self.to_net_interface())
    }

    /// Name to show users: the friendly name when known, else the OS name.
    pub fn display_name(&self) -> &str {
        self.description.split(" | ").next().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    /// True if `label` names this interface (OS name, friendly name or
    /// description, case-insensitive).
    pub fn matches_name(&self, label: &str) -> bool {
        let label = label.trim();
        !label.is_empty()
            && (self.name.eq_ignore_ascii_case(label)
                || self.description.split(" | ").any(|p| p.trim().eq_ignore_ascii_case(label)))
    }
}

/// Result of an HTTP "what is my IP" lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExitProbe {
    /// The probe was not run.
    NotRun,
    /// No endpoint answered (no route, blocked, timeout).
    Failed { error: String },
    /// The endpoint saw this address.
    Found { ip: IpAddr },
}

impl ExitProbe {
    /// The address, if found.
    pub fn ip(&self) -> Option<IpAddr> {
        match self {
            ExitProbe::Found { ip } => Some(*ip),
            _ => None,
        }
    }
}

/// What happened to one STUN Binding request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StunOutcome {
    /// The server saw this public address and port.
    Mapped { addr: SocketAddr },
    /// No answer after all retransmissions.
    Timeout,
    /// The OS has no route from this interface (expected for host-only adapters).
    NoRoute { detail: String },
    /// The OS or a firewall refused (permission denied, port unreachable).
    Blocked { detail: String },
    /// Anything else (bad response, resolution failure, socket error).
    Failed { detail: String },
}

/// One STUN probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StunProbe {
    /// The server as given, e.g. `stun.l.google.com:19302`.
    pub server: String,
    /// The resolved server address, when resolution worked.
    pub server_addr: Option<SocketAddr>,
    /// Interface the socket was bound to; `None` for the default route.
    pub interface: Option<String>,
    /// Local address the socket was bound to, if any.
    pub local: Option<IpAddr>,
    pub outcome: StunOutcome,
}

/// One configured DNS resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsServer {
    /// Interface name or alias as the OS tool printed it; `None` for global
    /// resolvers not tied to an interface (e.g. `/etc/resolv.conf`).
    pub interface: Option<String>,
    /// OS interface index, when the tool printed it.
    pub if_index: Option<u32>,
    pub addr: IpAddr,
}

/// Resolver discovery output from the platform layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsDiscovery {
    /// Which tool produced the list, e.g. `Get-DnsClientServerAddress`.
    pub source: String,
    pub servers: Vec<DnsServer>,
    /// Set when discovery failed (servers is then usually empty).
    pub error: Option<String>,
    /// Extra remarks (fallbacks used and the like).
    pub notes: Vec<String>,
}

/// A resolver observed by the active DNS test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedResolver {
    pub ip: IpAddr,
    pub country: Option<String>,
    pub organization: Option<String>,
    /// The test provider says this is its own (VPN) resolver.
    pub provider_dns: bool,
}

/// The exit as described by the active test provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub ip: Option<IpAddr>,
    pub country: Option<String>,
    pub organization: Option<String>,
}

/// Output of the opt-in unique-subdomain DNS test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveDnsResult {
    /// Which service ran the test.
    pub provider: String,
    /// Hosts contacted, for the report.
    pub contacted: Vec<String>,
    pub exit: Option<ExitInfo>,
    pub resolvers: Vec<ObservedResolver>,
    /// Set when the test could not run at all.
    pub error: Option<String>,
}

/// Everything [`evaluate`] looks at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeakInputs {
    pub interfaces: Vec<LeakInterface>,
    /// The public IP over the default route (IPv4 preferred).
    pub exit: ExitProbe,
    /// The answer of an IPv6-only endpoint.
    pub exit_v6: ExitProbe,
    /// STUN probes; `None` when skipped.
    pub stun: Option<Vec<StunProbe>>,
    pub dns: DnsDiscovery,
    /// Present only when the active DNS test was requested.
    pub active_dns: Option<ActiveDnsResult>,
    /// Orchestrator verify results; present only while a profile is engaged.
    pub verify: Option<Vec<VerifyReport>>,
    /// Hosts contacted while gathering the inputs.
    pub third_parties: Vec<String>,
}

/// The overall outcome, mapped to the CLI exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakOutcome {
    /// No leak and every check completed (exit 0).
    Clean,
    /// At least one check found a leak (exit 3).
    LeaksFound,
    /// No leak found, but at least one check could not run (exit 1).
    Incomplete,
}

/// The full report printed by `geoveil leaks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeakReport {
    pub outcome: LeakOutcome,
    pub vpn_interfaces: Vec<String>,
    pub exit_ip: Option<IpAddr>,
    pub exit_ipv6: Option<IpAddr>,
    pub checks: Vec<CheckResult>,
    /// The raw observations, for `--json` consumers.
    pub inputs: LeakInputs,
}

/// Browser and VPN advice for WebRTC leaks.
pub const WEBRTC_ADVICE: &str = "Browsers using WebRTC can reveal this address to any web page. \
Turn on your VPN's kill switch (block traffic outside the tunnel), or limit WebRTC in the browser: \
Firefox about:config media.peerconnection.ice.default_address_only=true (or media.peerconnection.enabled=false); \
Chrome/Edge/Brave: an extension or policy that sets WebRTC IP handling to \"disable_non_proxied_udp\".";

const IPV6_ADVICE: &str = "Turn on your VPN client's IPv6 leak protection or kill switch, use a VPN \
that tunnels IPv6, or disable IPv6 on the physical adapter while the VPN is up.";

const DNS_ADVICE: &str = "Turn on your VPN client's DNS leak protection, or set the physical \
adapter's DNS server to the VPN's resolver. Run `geoveil leaks --active-dns` to see which \
resolvers actually answer your queries.";

const VERIFY_ADVICE: &str = "Run `geoveil apply` again (or let `geoveil run` re-apply) so the \
system matches the VPN exit.";

/// Turns raw observations into a report. Pure.
pub fn evaluate(inputs: LeakInputs) -> LeakReport {
    let ctx = Ctx::new(&inputs);
    let mut checks = vec![
        check_vpn(&ctx, &inputs.exit),
        check_ipv6(&ctx, &inputs.exit_v6),
        check_webrtc(&ctx, inputs.stun.as_deref()),
        check_dns(&ctx, &inputs.dns),
    ];
    if let Some(active) = &inputs.active_dns {
        checks.push(check_active_dns(&ctx, active));
    }
    if let Some(verify) = &inputs.verify {
        checks.push(check_verify(verify));
    }
    let outcome = outcome_of(&checks);
    LeakReport {
        outcome,
        vpn_interfaces: ctx.vpn_names.clone(),
        exit_ip: ctx.exit,
        exit_ipv6: ctx.exit_v6,
        checks,
        inputs,
    }
}

/// Leak beats error beats everything else.
pub fn outcome_of(checks: &[CheckResult]) -> LeakOutcome {
    if checks.iter().any(|c| c.verdict == Verdict::Leak) {
        LeakOutcome::LeaksFound
    } else if checks.iter().any(|c| c.verdict == Verdict::Error) {
        LeakOutcome::Incomplete
    } else {
        LeakOutcome::Clean
    }
}

/// Facts shared by the checks.
struct Ctx<'a> {
    interfaces: &'a [LeakInterface],
    /// `is_vpn()` for each entry of `interfaces`.
    vpn_flags: Vec<bool>,
    vpn_names: Vec<String>,
    vpn_up: bool,
    exit: Option<IpAddr>,
    exit_v6: Option<IpAddr>,
}

impl<'a> Ctx<'a> {
    fn new(inputs: &'a LeakInputs) -> Self {
        let net: Vec<NetInterface> = inputs.interfaces.iter().map(LeakInterface::to_net_interface).collect();
        let vpn_flags: Vec<bool> = net.iter().map(crate::vpn::is_vpn_interface).collect();
        let vpn_names = VpnDetector::vpn_interfaces(&net);
        let exit_v6 = inputs.exit_v6.ip().or_else(|| inputs.exit.ip().filter(IpAddr::is_ipv6));
        Self {
            interfaces: &inputs.interfaces,
            vpn_up: !vpn_names.is_empty(),
            vpn_flags,
            vpn_names,
            exit: inputs.exit.ip(),
            exit_v6,
        }
    }

    /// Up, non-VPN, non-loopback interfaces.
    fn physical(&self) -> impl Iterator<Item = &'a LeakInterface> + '_ {
        self.interfaces
            .iter()
            .zip(&self.vpn_flags)
            .filter(|(i, vpn)| i.is_up && !i.is_loopback && !**vpn)
            .map(|(i, _)| i)
    }

    /// The physical interface owning `ip` exactly, or (IPv6) sharing its /64.
    fn physical_owner(&self, ip: IpAddr) -> Option<&'a LeakInterface> {
        let ip = canonical(ip);
        self.physical()
            .find(|i| i.addrs.iter().any(|a| canonical(*a) == ip))
            .or_else(|| match ip {
                IpAddr::V6(v6) if is_global_v6(&v6) => self.physical().find(|i| {
                    i.addrs.iter().any(|a| matches!(a, IpAddr::V6(o) if is_global_v6(o) && same_prefix_v6(o, &v6, 64)))
                }),
                _ => None,
            })
    }

    /// Whether `ip` is the known exit address.
    fn is_exit(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.exit.map(canonical) == Some(ip) || self.exit_v6.map(canonical) == Some(ip)
    }

    /// Finds the interface a DNS server entry belongs to.
    fn dns_interface(&self, server: &DnsServer) -> Option<(&'a LeakInterface, bool)> {
        let by_index = server
            .if_index
            .filter(|i| *i != 0)
            .and_then(|idx| self.interfaces.iter().position(|i| i.index == idx));
        let by_name = || {
            server
                .interface
                .as_deref()
                .and_then(|n| self.interfaces.iter().position(|i| i.matches_name(n)))
        };
        by_index.or_else(by_name).map(|pos| (&self.interfaces[pos], self.vpn_flags[pos]))
    }
}

/// Maps IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) to plain IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// Global unicast IPv6 (`2000::/3`).
pub fn is_global_v6(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] & 0xe000 == 0x2000
}

/// An IPv4 address that is routable on the internet (not private, loopback,
/// link-local, CGNAT `100.64/10`, broadcast or unspecified).
pub fn is_public_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || o[0] == 0
        || o[0] >= 224)
}

/// A public (internet-routable) address of either family.
pub fn is_public(ip: &IpAddr) -> bool {
    match canonical(*ip) {
        IpAddr::V4(v4) => is_public_v4(&v4),
        IpAddr::V6(v6) => is_global_v6(&v6),
    }
}

/// Whether two IPv6 addresses share the first `bits` bits.
pub fn same_prefix_v6(a: &Ipv6Addr, b: &Ipv6Addr, bits: u32) -> bool {
    let bits = bits.min(128);
    if bits == 0 {
        return true;
    }
    let mask = u128::MAX << (128 - bits);
    (u128::from(*a) & mask) == (u128::from(*b) & mask)
}

/// Whether two addresses are in the same network (/24 for IPv4, /48 for IPv6).
fn same_network(a: IpAddr, b: IpAddr) -> bool {
    match (canonical(a), canonical(b)) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a.octets()[..3] == b.octets()[..3],
        (IpAddr::V6(a), IpAddr::V6(b)) => same_prefix_v6(&a, &b, 48),
        _ => false,
    }
}

fn join_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// (1) VPN presence and exit IP.
fn check_vpn(ctx: &Ctx<'_>, exit: &ExitProbe) -> CheckResult {
    let names = join_names(&ctx.vpn_names);
    match (exit, ctx.vpn_up) {
        (ExitProbe::Found { ip }, true) => {
            CheckResult::new(CheckId::Vpn, Verdict::Pass, format!("VPN interface(s) {names} up; exit IP {ip}"))
        }
        (ExitProbe::Found { ip }, false) => CheckResult::new(
            CheckId::Vpn,
            Verdict::Info,
            format!("no local VPN interface found; public IP {ip}"),
        )
        .details(vec![
            "If your VPN runs on the router or as a proxy, this machine cannot see your real ISP address; \
             the checks below compare against the public IP above."
                .to_string(),
        ]),
        (ExitProbe::Failed { error }, _) => CheckResult::new(
            CheckId::Vpn,
            Verdict::Error,
            format!("could not determine the public IP (VPN interfaces: {names})"),
        )
        .details(vec![error.clone()]),
        (ExitProbe::NotRun, _) => CheckResult::new(
            CheckId::Vpn,
            Verdict::Error,
            format!("public IP lookup was not run (VPN interfaces: {names})"),
        ),
    }
}

/// (2) IPv6 leak.
fn check_ipv6(ctx: &Ctx<'_>, probe: &ExitProbe) -> CheckResult {
    let local_v6: Vec<String> = ctx
        .physical()
        .flat_map(|i| {
            i.addrs.iter().filter_map(move |a| match a {
                IpAddr::V6(v6) if is_global_v6(v6) => Some(format!("{} {v6}", i.display_name())),
                _ => None,
            })
        })
        .collect();
    let id = CheckId::Ipv6;
    if matches!(probe, ExitProbe::NotRun) {
        return CheckResult::new(id, Verdict::NotApplicable, "IPv6 probe skipped");
    }
    if local_v6.is_empty() {
        return match probe {
            ExitProbe::Found { ip } if ctx.vpn_up => CheckResult::new(
                id,
                Verdict::Pass,
                format!("IPv6 exits as {ip}; no global IPv6 address on physical interfaces"),
            ),
            ExitProbe::Found { ip } => CheckResult::new(id, Verdict::Info, format!("IPv6 exits as {ip}")),
            _ => CheckResult::new(id, Verdict::NotApplicable, "no global IPv6 address on physical interfaces"),
        };
    }
    let details = vec![format!("global IPv6 on physical interfaces: {}", local_v6.join(", "))];
    match (probe, ctx.vpn_up) {
        (ExitProbe::Found { ip }, true) => match ctx.physical_owner(*ip) {
            Some(owner) => CheckResult::new(
                id,
                Verdict::Leak,
                format!("IPv6 traffic bypasses the VPN: sites see {ip}, an address of {}", owner.display_name()),
            )
            .details(details)
            .advice(IPV6_ADVICE),
            None => CheckResult::new(
                id,
                Verdict::Pass,
                format!("IPv6 exits as {ip}, which is not one of this machine's physical addresses"),
            )
            .details(details),
        },
        (ExitProbe::Found { ip }, false) => {
            CheckResult::new(id, Verdict::Info, format!("no VPN interface; IPv6 exits directly as {ip}")).details(details)
        }
        (ExitProbe::Failed { error }, true) => {
            let mut d = details;
            d.push(format!("IPv6-only endpoints: {error}"));
            CheckResult::new(id, Verdict::Pass, "IPv6 is blocked or not routed while the VPN is up").details(d)
        }
        (ExitProbe::Failed { error }, false) => {
            let mut d = details;
            d.push(format!("IPv6-only endpoints: {error}"));
            CheckResult::new(id, Verdict::Info, "IPv6-only endpoints did not answer").details(d)
        }
        (ExitProbe::NotRun, _) => unreachable!("handled above"),
    }
}

/// (3) WebRTC / STUN.
fn check_webrtc(ctx: &Ctx<'_>, probes: Option<&[StunProbe]>) -> CheckResult {
    let id = CheckId::Webrtc;
    let Some(probes) = probes else {
        return CheckResult::new(id, Verdict::NotApplicable, "STUN probes skipped");
    };
    if probes.is_empty() {
        return CheckResult::new(id, Verdict::NotApplicable, "no interface to probe");
    }
    let mut details = Vec::new();
    let mut worst = Verdict::Pass;
    let mut leaked: BTreeSet<IpAddr> = BTreeSet::new();
    let mut mapped_any = false;
    for p in probes {
        let via = match (&p.interface, p.local) {
            (Some(i), Some(l)) => format!("{i} ({l})"),
            (Some(i), None) => i.clone(),
            (None, _) => "default route".to_string(),
        };
        let addr = match &p.outcome {
            StunOutcome::Mapped { addr } => *addr,
            other => {
                let what = match other {
                    StunOutcome::Timeout => "no answer".to_string(),
                    StunOutcome::NoRoute { detail } => format!("no route ({detail})"),
                    StunOutcome::Blocked { detail } => format!("blocked ({detail})"),
                    StunOutcome::Failed { detail } => format!("failed ({detail})"),
                    StunOutcome::Mapped { .. } => unreachable!(),
                };
                details.push(format!("{via} -> {}: {what}", p.server));
                continue;
            }
        };
        mapped_any = true;
        let ip = canonical(addr.ip());
        let verdict = if ctx.is_exit(ip) {
            Verdict::Pass
        } else if ctx.vpn_up {
            if p.interface.is_some() || ctx.physical_owner(ip).is_some() {
                Verdict::Leak
            } else {
                // The default route's UDP leaves with another address: a VPN
                // with a separate UDP egress, or a partial tunnel.
                Verdict::Warn
            }
        } else if ctx.exit.is_some() {
            // No local VPN, and UDP leaves with another address than HTTPS:
            // a proxy-style VPN would leak this through WebRTC.
            Verdict::Warn
        } else {
            Verdict::Info
        };
        if verdict == Verdict::Leak || verdict == Verdict::Warn {
            leaked.insert(ip);
        }
        let note = match verdict {
            Verdict::Pass => " (same as the exit IP)",
            Verdict::Leak => " (NOT the VPN exit)",
            Verdict::Warn => " (differs from the exit IP)",
            _ => "",
        };
        details.push(format!("{via} -> {}: sees {ip}{note}", p.server));
        worst = worst.max(verdict);
    }
    let shown: Vec<String> = leaked.iter().map(ToString::to_string).collect();
    match worst {
        Verdict::Leak => CheckResult::new(
            id,
            Verdict::Leak,
            format!("STUN outside the tunnel reveals {}; WebRTC can expose your real IP", shown.join(", ")),
        )
        .details(details)
        .advice(WEBRTC_ADVICE),
        Verdict::Warn => CheckResult::new(
            id,
            Verdict::Warn,
            format!("UDP (STUN) leaves with {}, not the exit IP; WebRTC would show that address", shown.join(", ")),
        )
        .details(details)
        .advice(WEBRTC_ADVICE),
        _ if !mapped_any && ctx.vpn_up => {
            CheckResult::new(id, Verdict::Pass, "no STUN answer outside the VPN (UDP blocked or no route)").details(details)
        }
        _ if !mapped_any => CheckResult::new(id, Verdict::Info, "no STUN server answered (UDP blocked?)").details(details),
        Verdict::Pass if ctx.vpn_up => {
            CheckResult::new(id, Verdict::Pass, "every STUN answer shows the VPN exit IP").details(details)
        }
        _ => CheckResult::new(id, Verdict::Info, "no local VPN; STUN shows the same address as HTTPS").details(details),
    }
}

/// (4) Passive DNS configuration.
fn check_dns(ctx: &Ctx<'_>, dns: &DnsDiscovery) -> CheckResult {
    let id = CheckId::Dns;
    if dns.servers.is_empty() {
        return match &dns.error {
            Some(e) => CheckResult::new(id, Verdict::Error, format!("could not list DNS resolvers ({})", dns.source))
                .details(vec![e.clone()]),
            None => CheckResult::new(id, Verdict::Info, "no DNS resolvers configured").details(dns.notes.clone()),
        };
    }
    let resolved: Vec<(&DnsServer, Option<(&LeakInterface, bool)>)> =
        dns.servers.iter().map(|s| (s, ctx.dns_interface(s))).collect();
    let vpn_addrs: BTreeSet<IpAddr> = resolved
        .iter()
        .filter(|(_, i)| matches!(i, Some((_, true))))
        .map(|(s, _)| canonical(s.addr))
        .collect();

    let mut details = Vec::new();
    let mut worst = Verdict::Pass;
    let mut flagged = Vec::new();
    for (server, iface) in &resolved {
        let label = match (iface, &server.interface) {
            (Some((i, _)), _) => i.display_name().to_string(),
            (None, Some(n)) => n.clone(),
            (None, None) => "global".to_string(),
        };
        let kind = match iface {
            Some((_, true)) => "VPN",
            Some((i, false)) if !i.is_up => "down",
            Some((i, false)) if i.is_loopback => "loopback",
            Some((_, false)) => "physical",
            None => "unknown interface",
        };
        let mut line = format!("{} on {label} ({kind})", server.addr);
        let suspicious = ctx.vpn_up
            && matches!(iface, Some((i, false)) if i.is_up && !i.is_loopback)
            && !vpn_addrs.contains(&canonical(server.addr));
        if suspicious {
            let addr = canonical(server.addr);
            let verdict = if addr.is_loopback() {
                line.push_str(": local stub resolver");
                Verdict::Pass
            } else if is_public(&addr) {
                line.push_str(": public resolver outside the VPN");
                Verdict::Leak
            } else {
                line.push_str(": LAN resolver outside the VPN (your router may forward queries to your ISP)");
                Verdict::Warn
            };
            if verdict != Verdict::Pass {
                flagged.push(format!("{} on {label}", server.addr));
            }
            worst = worst.max(verdict);
        }
        details.push(line);
    }
    details.extend(dns.notes.iter().cloned());
    details.push(format!("source: {}", dns.source));

    if !ctx.vpn_up {
        // Resolvers in use: those on up interfaces or not tied to one.
        let mut in_use: Vec<String> = Vec::new();
        for (server, iface) in &resolved {
            let up = iface.is_none_or(|(i, _)| i.is_up);
            let addr = server.addr.to_string();
            if up && !in_use.contains(&addr) {
                in_use.push(addr);
            }
        }
        return CheckResult::new(id, Verdict::Info, format!("no local VPN; resolvers in use: {}", join_names(&in_use)))
            .details(details);
    }
    match worst {
        Verdict::Leak => CheckResult::new(
            id,
            Verdict::Leak,
            format!("resolver(s) configured only outside the VPN: {}", flagged.join(", ")),
        )
        .details(details)
        .advice(DNS_ADVICE),
        Verdict::Warn => CheckResult::new(
            id,
            Verdict::Warn,
            format!("LAN resolver(s) configured outside the VPN: {}", flagged.join(", ")),
        )
        .details(details)
        .advice(DNS_ADVICE),
        _ if vpn_addrs.is_empty() => CheckResult::new(
            id,
            Verdict::Info,
            "no resolver is set on the VPN interface; queries follow the routing table",
        )
        .details(details),
        _ => CheckResult::new(id, Verdict::Pass, "resolvers are on the VPN interface").details(details),
    }
}

/// (4b) Opt-in active DNS test.
fn check_active_dns(ctx: &Ctx<'_>, active: &ActiveDnsResult) -> CheckResult {
    let id = CheckId::DnsActive;
    let mut details = vec![format!("contacted: {}", active.contacted.join(", "))];
    if let Some(e) = &active.error {
        details.push(e.clone());
        return CheckResult::new(id, Verdict::Error, format!("{} DNS leak test failed", active.provider)).details(details);
    }
    if active.resolvers.is_empty() {
        return CheckResult::new(id, Verdict::Warn, format!("{} saw no resolver for the test names", active.provider))
            .details(details);
    }
    let exit_org = active.exit.as_ref().and_then(|e| e.organization.clone()).filter(|s| !s.is_empty());
    let exit_country = active.exit.as_ref().and_then(|e| e.country.clone()).filter(|s| !s.is_empty());
    let exit_ip = active.exit.as_ref().and_then(|e| e.ip).or(ctx.exit);
    if let Some(e) = &active.exit {
        details.push(format!(
            "exit as seen by {}: {} {} {}",
            active.provider,
            e.ip.map(|i| i.to_string()).unwrap_or_default(),
            e.organization.as_deref().unwrap_or(""),
            e.country.as_deref().unwrap_or("")
        ));
    }
    let mut worst = Verdict::Pass;
    let mut flagged = Vec::new();
    for r in &active.resolvers {
        let org = r.organization.as_deref().unwrap_or("?");
        let country = r.country.as_deref().unwrap_or("?");
        let same_org = matches!((&exit_org, &r.organization), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
        let same_net = exit_ip.is_some_and(|e| same_network(e, r.ip)) || ctx.is_exit(r.ip);
        let (verdict, why) = if ctx.physical_owner(r.ip).is_some() {
            (Verdict::Leak, "one of this machine's own addresses")
        } else if r.provider_dns || same_org || same_net {
            (Verdict::Pass, "same network as the exit")
        } else if ctx.vpn_up
            && matches!((&exit_country, &r.country), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b))
        {
            (Verdict::Warn, "different country than the exit; could be your ISP's resolver")
        } else {
            (Verdict::Info, "third-party resolver")
        };
        if verdict >= Verdict::Warn {
            flagged.push(r.ip.to_string());
        }
        worst = worst.max(verdict);
        details.push(format!("{} {org} ({country}): {why}", r.ip));
    }
    let n = active.resolvers.len();
    match worst {
        Verdict::Leak => {
            CheckResult::new(id, Verdict::Leak, format!("DNS queries leave with your own address: {}", flagged.join(", ")))
                .details(details)
                .advice(DNS_ADVICE)
        }
        Verdict::Warn => CheckResult::new(
            id,
            Verdict::Warn,
            format!("resolver(s) {} are not in the exit's country", flagged.join(", ")),
        )
        .details(details)
        .advice(DNS_ADVICE),
        Verdict::Info => {
            CheckResult::new(id, Verdict::Info, format!("{n} resolver(s) seen, none on this machine's network")).details(details)
        }
        _ => CheckResult::new(id, Verdict::Pass, format!("{n} resolver(s) seen, all on the exit's network")).details(details),
    }
}

/// (5) Orchestrator verify, while engaged.
fn check_verify(reports: &[VerifyReport]) -> CheckResult {
    let id = CheckId::Verify;
    if reports.is_empty() {
        return CheckResult::new(id, Verdict::NotApplicable, "engaged, but no backend to verify");
    }
    let details: Vec<String> = reports
        .iter()
        .map(|r| format!("{} {}: {}", if r.ok { "ok  " } else { "DIFF" }, r.backend, r.detail))
        .collect();
    let bad: Vec<&str> = reports.iter().filter(|r| !r.ok).map(|r| r.backend.as_str()).collect();
    if bad.is_empty() {
        CheckResult::new(id, Verdict::Pass, "engaged profile matches the system").details(details)
    } else {
        CheckResult::new(id, Verdict::Warn, format!("system differs from the engaged profile: {}", bad.join(", ")))
            .details(details)
            .advice(VERIFY_ADVICE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn iface(index: u32, name: &str, desc: &str, addrs: &[&str]) -> LeakInterface {
        LeakInterface {
            index,
            name: name.into(),
            description: desc.into(),
            is_up: true,
            is_loopback: false,
            has_gateway: true,
            addrs: addrs.iter().map(|a| ip(a)).collect(),
        }
    }

    fn eth() -> LeakInterface {
        iface(4, "{GUID-ETH}", "Ethernet | Intel(R) Ethernet", &["192.168.1.118", "2a01:4f8:1:2::10"])
    }

    fn wg() -> LeakInterface {
        iface(17, "{GUID-WG}", "wg-home | WireGuard Tunnel", &["10.64.0.2"])
    }

    fn base(vpn: bool) -> LeakInputs {
        let mut interfaces = vec![eth()];
        if vpn {
            interfaces.push(wg());
        }
        LeakInputs {
            interfaces,
            exit: ExitProbe::Found { ip: ip("185.65.135.230") },
            exit_v6: ExitProbe::Failed { error: "no route".into() },
            stun: Some(vec![]),
            dns: DnsDiscovery { source: "test".into(), ..Default::default() },
            active_dns: None,
            verify: None,
            third_parties: vec![],
        }
    }

    fn check(report: &LeakReport, id: CheckId) -> &CheckResult {
        report.checks.iter().find(|c| c.id == id).unwrap()
    }

    fn probe(iface: Option<&str>, mapped: &str) -> StunProbe {
        StunProbe {
            server: "stun.example:3478".into(),
            server_addr: None,
            interface: iface.map(String::from),
            local: None,
            outcome: StunOutcome::Mapped { addr: SocketAddr::new(ip(mapped), 40000) },
        }
    }

    #[test]
    fn address_helpers() {
        assert!(is_public(&ip("185.65.135.230")));
        assert!(!is_public(&ip("192.168.1.1")));
        assert!(!is_public(&ip("100.64.0.1")));
        assert!(!is_public(&ip("127.0.0.53")));
        assert!(is_public(&ip("2a01:4f8::1")));
        assert!(!is_public(&ip("fe80::1")));
        assert!(!is_public(&ip("fd00::1")));
        assert!(is_public(&ip("::ffff:8.8.8.8")));
        let a: Ipv6Addr = "2a01:4f8:1:2::10".parse().unwrap();
        let b: Ipv6Addr = "2a01:4f8:1:2:abcd::1".parse().unwrap();
        let c: Ipv6Addr = "2a01:4f8:1:3::1".parse().unwrap();
        assert!(same_prefix_v6(&a, &b, 64));
        assert!(!same_prefix_v6(&a, &c, 64));
        assert!(same_prefix_v6(&a, &c, 48));
        assert!(same_network(ip("1.2.3.4"), ip("1.2.3.200")));
    }

    #[test]
    fn interface_name_matching() {
        let e = eth();
        assert!(e.matches_name("Ethernet"));
        assert!(e.matches_name("ethernet"));
        assert!(e.matches_name("{GUID-ETH}"));
        assert!(!e.matches_name("Ether"));
        assert_eq!(e.display_name(), "Ethernet");
        let bare = iface(2, "eth0", "", &[]);
        assert_eq!(bare.display_name(), "eth0");
    }

    #[test]
    fn clean_vpn_report() {
        let r = evaluate(base(true));
        assert_eq!(r.vpn_interfaces, vec!["{GUID-WG}"]);
        assert_eq!(check(&r, CheckId::Vpn).verdict, Verdict::Pass);
        assert_eq!(check(&r, CheckId::Ipv6).verdict, Verdict::Pass, "IPv6 blocked while VPN up");
        assert_eq!(r.outcome, LeakOutcome::Clean);
        assert!(r.checks.iter().all(|c| c.id != CheckId::Verify && c.id != CheckId::DnsActive));
    }

    #[test]
    fn no_vpn_is_info_not_leak() {
        let mut inputs = base(false);
        inputs.exit_v6 = ExitProbe::Found { ip: ip("2a01:4f8:1:2::10") };
        inputs.stun = Some(vec![probe(Some("Ethernet"), "185.65.135.230")]);
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Vpn).verdict, Verdict::Info);
        assert!(check(&r, CheckId::Vpn).details[0].contains("router"));
        assert_eq!(check(&r, CheckId::Ipv6).verdict, Verdict::Info);
        assert_eq!(check(&r, CheckId::Webrtc).verdict, Verdict::Info);
        assert_eq!(r.outcome, LeakOutcome::Clean);
    }

    #[test]
    fn exit_failure_is_incomplete() {
        let mut inputs = base(true);
        inputs.exit = ExitProbe::Failed { error: "offline".into() };
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Vpn).verdict, Verdict::Error);
        assert_eq!(r.outcome, LeakOutcome::Incomplete);
    }

    #[test]
    fn ipv6_leak_exact_and_same_prefix() {
        for seen in ["2a01:4f8:1:2::10", "2a01:4f8:1:2:dead:beef::7"] {
            let mut inputs = base(true);
            inputs.exit_v6 = ExitProbe::Found { ip: ip(seen) };
            let r = evaluate(inputs);
            let c = check(&r, CheckId::Ipv6);
            assert_eq!(c.verdict, Verdict::Leak, "{seen}");
            assert!(c.summary.contains("Ethernet"));
            assert!(c.advice.is_some());
            assert_eq!(r.outcome, LeakOutcome::LeaksFound);
        }
    }

    #[test]
    fn ipv6_through_vpn_passes() {
        let mut inputs = base(true);
        inputs.exit_v6 = ExitProbe::Found { ip: ip("2a03:1b20:5:f011::a01f") };
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Ipv6).verdict, Verdict::Pass);
    }

    #[test]
    fn ipv6_not_applicable_without_global_v6() {
        let mut inputs = base(true);
        inputs.interfaces[0].addrs = vec![ip("192.168.1.118"), ip("fe80::1")];
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Ipv6).verdict, Verdict::NotApplicable);
        let mut inputs = base(true);
        inputs.exit_v6 = ExitProbe::NotRun;
        assert_eq!(check(&evaluate(inputs), CheckId::Ipv6).verdict, Verdict::NotApplicable);
    }

    #[test]
    fn global_v6_on_down_interface_is_ignored() {
        let mut inputs = base(true);
        inputs.interfaces[0].is_up = false;
        inputs.exit_v6 = ExitProbe::Found { ip: ip("2a01:4f8:1:2::10") };
        let r = evaluate(inputs);
        // The address belongs to a down adapter, so it cannot be the egress.
        assert_ne!(check(&r, CheckId::Ipv6).verdict, Verdict::Leak);
    }

    #[test]
    fn webrtc_leak_on_physical_interface() {
        let mut inputs = base(true);
        inputs.stun = Some(vec![
            probe(None, "185.65.135.230"),
            probe(Some("Ethernet"), "82.152.98.44"),
            StunProbe {
                server: "stun.example:3478".into(),
                server_addr: None,
                interface: Some("vEthernet".into()),
                local: Some(ip("172.26.32.1")),
                outcome: StunOutcome::NoRoute { detail: "unreachable network".into() },
            },
        ]);
        let r = evaluate(inputs);
        let c = check(&r, CheckId::Webrtc);
        assert_eq!(c.verdict, Verdict::Leak);
        assert!(c.summary.contains("82.152.98.44"));
        assert_eq!(c.advice.as_deref(), Some(WEBRTC_ADVICE));
        assert!(c.details.iter().any(|d| d.contains("no route")));
        assert!(c.details.iter().any(|d| d.contains("same as the exit")));
        assert_eq!(r.outcome, LeakOutcome::LeaksFound);
    }

    #[test]
    fn webrtc_kill_switch_passes() {
        let mut inputs = base(true);
        let mut blocked = probe(Some("Ethernet"), "1.1.1.1");
        blocked.outcome = StunOutcome::Timeout;
        inputs.stun = Some(vec![probe(None, "185.65.135.230"), blocked]);
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Webrtc).verdict, Verdict::Pass);
        // Nothing answered at all while the VPN is up: still no leak.
        let mut inputs = base(true);
        let mut t = probe(None, "1.1.1.1");
        t.outcome = StunOutcome::Timeout;
        inputs.stun = Some(vec![t]);
        assert_eq!(check(&evaluate(inputs), CheckId::Webrtc).verdict, Verdict::Pass);
    }

    #[test]
    fn webrtc_default_route_mismatch_warns() {
        let mut inputs = base(true);
        inputs.stun = Some(vec![probe(None, "104.28.1.1")]);
        let c = evaluate(inputs);
        assert_eq!(check(&c, CheckId::Webrtc).verdict, Verdict::Warn);
        // ...unless that address is one of our own physical ones.
        let mut inputs = base(true);
        inputs.interfaces[0].addrs.push(ip("203.0.113.50"));
        inputs.stun = Some(vec![probe(None, "203.0.113.50")]);
        assert_eq!(check(&evaluate(inputs), CheckId::Webrtc).verdict, Verdict::Leak);
    }

    #[test]
    fn webrtc_proxy_style_mismatch_without_vpn_warns() {
        let mut inputs = base(false);
        inputs.stun = Some(vec![probe(Some("Ethernet"), "82.152.98.44")]);
        assert_eq!(check(&evaluate(inputs), CheckId::Webrtc).verdict, Verdict::Warn);
    }

    #[test]
    fn webrtc_ipv4_mapped_counts_as_exit() {
        let mut inputs = base(true);
        inputs.stun = Some(vec![probe(Some("Ethernet"), "::ffff:185.65.135.230")]);
        assert_eq!(check(&evaluate(inputs), CheckId::Webrtc).verdict, Verdict::Pass);
    }

    #[test]
    fn webrtc_skipped() {
        let mut inputs = base(true);
        inputs.stun = None;
        assert_eq!(check(&evaluate(inputs), CheckId::Webrtc).verdict, Verdict::NotApplicable);
    }

    fn dns(entries: &[(Option<&str>, Option<u32>, &str)]) -> DnsDiscovery {
        DnsDiscovery {
            source: "test".into(),
            servers: entries
                .iter()
                .map(|(n, i, a)| DnsServer { interface: n.map(String::from), if_index: *i, addr: ip(a) })
                .collect(),
            error: None,
            notes: vec![],
        }
    }

    #[test]
    fn dns_public_resolver_outside_vpn_is_leak() {
        let mut inputs = base(true);
        inputs.dns = dns(&[(Some("Ethernet"), Some(4), "8.8.8.8"), (Some("wg-home"), Some(17), "10.64.0.1")]);
        let r = evaluate(inputs);
        let c = check(&r, CheckId::Dns);
        assert_eq!(c.verdict, Verdict::Leak);
        assert!(c.summary.contains("8.8.8.8 on Ethernet"));
        assert!(c.details.iter().any(|d| d.contains("10.64.0.1 on wg-home (VPN)")));
    }

    #[test]
    fn dns_lan_resolver_outside_vpn_warns() {
        let mut inputs = base(true);
        inputs.dns = dns(&[(Some("Ethernet"), None, "192.168.1.1"), (None, Some(17), "10.64.0.1")]);
        let c = evaluate(inputs);
        assert_eq!(check(&c, CheckId::Dns).verdict, Verdict::Warn);
    }

    #[test]
    fn dns_same_resolver_on_vpn_is_fine() {
        let mut inputs = base(true);
        inputs.dns = dns(&[(Some("Ethernet"), Some(4), "10.64.0.1"), (Some("wg-home"), Some(17), "10.64.0.1")]);
        assert_eq!(check(&evaluate(inputs), CheckId::Dns).verdict, Verdict::Pass);
    }

    #[test]
    fn dns_down_interface_and_stub_are_ignored() {
        let mut inputs = base(true);
        let mut wifi = iface(9, "{GUID-WIFI}", "Wi-Fi", &["192.168.0.5"]);
        wifi.is_up = false;
        inputs.interfaces.push(wifi);
        inputs.dns = dns(&[
            (Some("Wi-Fi"), Some(9), "8.8.8.8"),
            (Some("Ethernet"), Some(4), "127.0.0.53"),
            (Some("wg-home"), Some(17), "10.64.0.1"),
        ]);
        let c = evaluate(inputs);
        let c = check(&c, CheckId::Dns);
        assert_eq!(c.verdict, Verdict::Pass, "{c:?}");
        assert!(c.details.iter().any(|d| d.contains("(down)")));
    }

    #[test]
    fn dns_loopback_interface_is_not_flagged() {
        let mut inputs = base(true);
        inputs.interfaces.push(LeakInterface { is_loopback: true, ..iface(1, "lo", "Loopback Pseudo-Interface 1", &["127.0.0.1"]) });
        inputs.dns = dns(&[(Some("Loopback Pseudo-Interface 1"), Some(1), "9.9.9.9"), (None, Some(17), "10.64.0.1")]);
        let r = evaluate(inputs);
        let c = check(&r, CheckId::Dns);
        assert_eq!(c.verdict, Verdict::Pass, "{c:?}");
        assert!(c.details.iter().any(|d| d.contains("(loopback)")));
    }

    #[test]
    fn dns_without_vpn_or_vpn_resolver() {
        let mut inputs = base(false);
        let mut wifi = iface(9, "{GUID-WIFI}", "Wi-Fi", &["192.168.0.5"]);
        wifi.is_up = false;
        inputs.interfaces.push(wifi);
        inputs.dns = dns(&[
            (Some("Ethernet"), Some(4), "192.168.1.1"),
            (Some("Ethernet"), Some(4), "192.168.1.1"),
            (Some("Wi-Fi"), Some(9), "8.8.8.8"),
        ]);
        let c = evaluate(inputs);
        assert_eq!(check(&c, CheckId::Dns).verdict, Verdict::Info);
        assert_eq!(check(&c, CheckId::Dns).summary, "no local VPN; resolvers in use: 192.168.1.1");
        // VPN up, resolvers only global (resolv.conf): informational.
        let mut inputs = base(true);
        inputs.dns = dns(&[(None, None, "1.1.1.1")]);
        assert_eq!(check(&evaluate(inputs), CheckId::Dns).verdict, Verdict::Info);
    }

    #[test]
    fn dns_discovery_error() {
        let mut inputs = base(true);
        inputs.dns.error = Some("scutil not found".into());
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::Dns).verdict, Verdict::Error);
        assert_eq!(r.outcome, LeakOutcome::Incomplete);
    }

    fn resolver(addr: &str, org: &str, country: &str) -> ObservedResolver {
        ObservedResolver {
            ip: ip(addr),
            country: Some(country.into()),
            organization: Some(org.into()),
            provider_dns: false,
        }
    }

    fn active(resolvers: Vec<ObservedResolver>) -> ActiveDnsResult {
        ActiveDnsResult {
            provider: "mullvad".into(),
            contacted: vec!["am.i.mullvad.net".into()],
            exit: Some(ExitInfo {
                ip: Some(ip("185.65.135.230")),
                country: Some("Sweden".into()),
                organization: Some("M247".into()),
            }),
            resolvers,
            error: None,
        }
    }

    #[test]
    fn active_dns_classification() {
        let mut inputs = base(true);
        inputs.active_dns = Some(active(vec![resolver("185.65.135.1", "Other", "Sweden")]));
        assert_eq!(check(&evaluate(inputs), CheckId::DnsActive).verdict, Verdict::Pass);

        let mut inputs = base(true);
        inputs.active_dns = Some(active(vec![resolver("81.10.1.1", "Iran Telecom", "Iran")]));
        assert_eq!(check(&evaluate(inputs), CheckId::DnsActive).verdict, Verdict::Warn);

        let mut inputs = base(true);
        inputs.active_dns = Some(active(vec![resolver("2a01:4f8:1:2::99", "Hetzner", "Germany")]));
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::DnsActive).verdict, Verdict::Leak);
        assert_eq!(r.outcome, LeakOutcome::LeaksFound);

        let mut inputs = base(false);
        inputs.active_dns = Some(active(vec![resolver("162.158.1.1", "Cloudflare", "France")]));
        assert_eq!(check(&evaluate(inputs), CheckId::DnsActive).verdict, Verdict::Info);
    }

    #[test]
    fn active_dns_error_and_empty() {
        let mut a = active(vec![]);
        let mut inputs = base(true);
        inputs.active_dns = Some(a.clone());
        assert_eq!(check(&evaluate(inputs), CheckId::DnsActive).verdict, Verdict::Warn);
        a.error = Some("timeout".into());
        let mut inputs = base(true);
        inputs.active_dns = Some(a);
        let r = evaluate(inputs);
        assert_eq!(check(&r, CheckId::DnsActive).verdict, Verdict::Error);
        assert!(check(&r, CheckId::DnsActive).details[0].contains("am.i.mullvad.net"));
    }

    #[test]
    fn verify_included_only_when_engaged() {
        let mut inputs = base(true);
        inputs.verify = Some(vec![
            VerifyReport { backend: "timezone".into(), ok: true, detail: "Europe/Berlin".into() },
            VerifyReport { backend: "locale".into(), ok: false, detail: "en-US vs de-DE".into() },
        ]);
        let r = evaluate(inputs);
        let c = check(&r, CheckId::Verify);
        assert_eq!(c.verdict, Verdict::Warn);
        assert!(c.summary.contains("locale"));
        assert_eq!(r.outcome, LeakOutcome::Clean, "a verify mismatch is not a network leak");
    }

    #[test]
    fn outcome_precedence() {
        let mk = |v| CheckResult::new(CheckId::Vpn, v, "");
        assert_eq!(outcome_of(&[mk(Verdict::Pass), mk(Verdict::Warn)]), LeakOutcome::Clean);
        assert_eq!(outcome_of(&[mk(Verdict::Error), mk(Verdict::Pass)]), LeakOutcome::Incomplete);
        assert_eq!(outcome_of(&[mk(Verdict::Error), mk(Verdict::Leak)]), LeakOutcome::LeaksFound);
    }

    #[test]
    fn report_serializes_snake_case() {
        let r = evaluate(base(true));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"], "clean");
        assert_eq!(v["checks"][0]["id"], "vpn");
        assert_eq!(v["checks"][0]["verdict"], "pass");
        assert_eq!(v["inputs"]["exit"]["status"], "found");
        let back: LeakReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
