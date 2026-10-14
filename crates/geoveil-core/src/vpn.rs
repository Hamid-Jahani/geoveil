//! VPN detection: classify network interfaces and track VPN up/changed/down
//! transitions.
//!
//! Everything here is pure: callers pass in interface snapshots, the public IP
//! (fetched through whatever tunnel is active) and the current time. That keeps
//! the state machine fully unit-testable.
//!
//! # Interface classification
//!
//! An interface counts as a VPN exit when it is up, has at least one address,
//! and its name or description (case-insensitive) looks like a VPN adapter:
//!
//! - brand/driver substrings: `wireguard`, `wintun`, `openvpn`, `tap-windows`,
//!   `nordlynx`, `protonvpn`, `mullvad`, `expressvpn`, `surfshark`,
//!   `cisco anyconnect`, `fortinet`, `fortissl`, `globalprotect`, `pangp`;
//! - word tokens of the form `wg<digits>`, `tun<digits>`, `tap<digits>`,
//!   `utun<digits>`, `ppp<digits>`, `proton<digits>` (ProtonVPN's WireGuard
//!   interface on Linux), or starting with `ipsec`.
//!
//! Overlay networks (`tailscale`, `zerotier`) are never VPN exits, and a small
//! deny list (Teredo, ISATAP, Hyper-V, vEthernet, VirtualBox, VMware, loopback,
//! Bluetooth) guards against false positives. On macOS Tailscale uses a plain
//! `utunN` interface with no description, so any interface carrying an address
//! in Tailscale's own ULA range `fd7a:115c:a1e0::/48` is rejected too.
//!
//! **macOS `utun` caveat:** macOS creates `utun0`..`utunN` for system services
//! (iCloud Private Relay, Back to My Mac, etc.) even with no VPN running; those
//! carry only IPv6 link-local addresses. An interface matched *only* by the
//! `utun<digits>` rule therefore counts as a VPN only if it has a
//! non-link-local, non-loopback IPv4 address, which real VPN clients assign.

use crate::types::{NetInterface, VpnEvent};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

/// Distinctive substrings of VPN adapter names/descriptions (lower case).
const VPN_SUBSTRINGS: &[&str] = &[
    "wireguard",
    "wintun",
    "openvpn",
    "tap-windows",
    "nordlynx",
    "protonvpn",
    "mullvad",
    "expressvpn",
    "surfshark",
    "cisco anyconnect",
    "fortinet",
    "fortissl",
    "globalprotect",
    "pangp",
];

/// Token prefixes that must be followed by one or more digits only.
const DIGIT_PREFIXES: &[&str] = &["wg", "tun", "tap", "ppp", "proton"];

/// Tailscale's IPv6 ULA prefix `fd7a:115c:a1e0::/48` (first three segments).
const TAILSCALE_V6_PREFIX: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];

/// Overlay networks and known non-VPN virtual adapters (lower case substrings).
const DENY_SUBSTRINGS: &[&str] = &[
    "tailscale",
    "zerotier",
    "teredo",
    "isatap",
    "hyper-v",
    "vethernet",
    "virtualbox",
    "vmware",
    "loopback",
    "bluetooth",
];

/// How an interface matched the VPN heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// Matched by a rule other than `utun<digits>`.
    Strong,
    /// Matched only by `utun<digits>` (needs an extra address check).
    UtunOnly,
}

/// Returns true if `token` is `prefix` followed by at least one ASCII digit and
/// nothing else.
fn is_prefix_digits(token: &str, prefix: &str) -> bool {
    token
        .strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Splits lower-cased text into alphanumeric word tokens.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
}

/// Classifies the lower-cased `name + description` text.
fn classify(text: &str) -> Option<Match> {
    if DENY_SUBSTRINGS.iter().any(|d| text.contains(d)) {
        return None;
    }
    if VPN_SUBSTRINGS.iter().any(|s| text.contains(s)) {
        return Some(Match::Strong);
    }
    let mut utun = false;
    for token in tokens(text) {
        if DIGIT_PREFIXES.iter().any(|p| is_prefix_digits(token, p)) || token.starts_with("ipsec")
        {
            return Some(Match::Strong);
        }
        if is_prefix_digits(token, "utun") {
            utun = true;
        }
    }
    utun.then_some(Match::UtunOnly)
}

/// True for an IPv4 address that is neither link-local (169.254/16) nor loopback.
fn is_routable_v4(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_loopback() && *v4 != Ipv4Addr::UNSPECIFIED,
        IpAddr::V6(_) => false,
    }
}

/// True for an address inside Tailscale's `fd7a:115c:a1e0::/48`.
fn is_tailscale_addr(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V6(v6) => is_tailscale_v6(v6),
        IpAddr::V4(_) => false,
    }
}

fn is_tailscale_v6(v6: &Ipv6Addr) -> bool {
    v6.segments()[..3] == TAILSCALE_V6_PREFIX
}

/// Returns true if `iface` looks like an active VPN tunnel interface.
///
/// See the module docs for the exact heuristics, including the macOS `utun`
/// rule (a `utun` interface needs a non-link-local IPv4 address).
pub fn is_vpn_interface(iface: &NetInterface) -> bool {
    if !iface.is_up || iface.addrs.is_empty() {
        return false;
    }
    if iface.addrs.iter().any(is_tailscale_addr) {
        // Tailscale overlay (macOS `utunN`), never a VPN exit.
        return false;
    }
    let text = format!("{} {}", iface.name, iface.description).to_ascii_lowercase();
    match classify(&text) {
        Some(Match::Strong) => true,
        Some(Match::UtunOnly) => iface.addrs.iter().any(is_routable_v4),
        None => false,
    }
}

/// Tuning for [`VpnDetector`].
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// How often to re-check the public IP while a VPN is up (default 60 s).
    pub ip_recheck: Duration,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            ip_recheck: Duration::from_secs(60),
        }
    }
}

/// Current VPN state as tracked by the detector.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Down,
    Up { exit_ip: IpAddr },
}

/// State machine turning interface snapshots + public IP observations into
/// [`VpnEvent`]s.
#[derive(Debug, Clone)]
pub struct VpnDetector {
    cfg: DetectorConfig,
    state: State,
    /// VPN interface names at the last conclusive observation.
    last_ifaces: Vec<String>,
    /// When the public IP was last successfully observed.
    last_ip_check: Option<Instant>,
}

impl VpnDetector {
    /// Creates a detector in the "VPN down" state.
    pub fn new(cfg: DetectorConfig) -> Self {
        Self {
            cfg,
            state: State::Down,
            last_ifaces: Vec::new(),
            last_ip_check: None,
        }
    }

    /// Names of the VPN-like interfaces in `ifaces`, sorted and de-duplicated.
    pub fn vpn_interfaces(ifaces: &[NetInterface]) -> Vec<String> {
        let mut names: Vec<String> = ifaces
            .iter()
            .filter(|i| is_vpn_interface(i))
            .map(|i| i.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Whether the caller should fetch the public IP before calling
    /// [`observe`](Self::observe).
    ///
    /// True when the set of VPN interfaces differs from the last conclusive
    /// observation, or when a VPN interface is present and the last successful
    /// IP check is missing or older than `ip_recheck`.
    pub fn needs_ip_check(&self, ifaces: &[NetInterface], now: Instant) -> bool {
        let current = Self::vpn_interfaces(ifaces);
        if current != self.last_ifaces {
            return true;
        }
        if current.is_empty() {
            return false;
        }
        match self.last_ip_check {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= self.cfg.ip_recheck,
        }
    }

    /// Feeds one observation into the state machine.
    ///
    /// - No VPN interfaces: transitions Up -> Down (emits `Down`).
    /// - VPN interfaces and `public_ip` is `None`: the exit is unknown (e.g. the
    ///   IP lookup failed); state is left unchanged and the interface set is not
    ///   recorded, so [`needs_ip_check`](Self::needs_ip_check) keeps asking.
    /// - VPN interfaces and `public_ip` is `Some`: Down -> Up (emits `Up`), or
    ///   Up with a different IP -> `Changed`.
    ///
    /// The last-check time is recorded whenever `public_ip` is `Some`.
    pub fn observe(
        &mut self,
        ifaces: &[NetInterface],
        public_ip: Option<IpAddr>,
        now: Instant,
    ) -> Option<VpnEvent> {
        let current = Self::vpn_interfaces(ifaces);
        if public_ip.is_some() {
            self.last_ip_check = Some(now);
        }

        if current.is_empty() {
            self.last_ifaces = current;
            return match self.state {
                State::Up { .. } => {
                    self.state = State::Down;
                    Some(VpnEvent::Down)
                }
                State::Down => None,
            };
        }

        let ip = public_ip?;
        self.last_ifaces = current.clone();
        match self.state {
            State::Down => {
                self.state = State::Up { exit_ip: ip };
                Some(VpnEvent::Up {
                    exit_ip: ip,
                    interfaces: current,
                })
            }
            State::Up { exit_ip } if exit_ip != ip => {
                self.state = State::Up { exit_ip: ip };
                Some(VpnEvent::Changed {
                    exit_ip: ip,
                    interfaces: current,
                })
            }
            State::Up { .. } => None,
        }
    }

    /// Whether the detector currently considers the VPN up.
    pub fn is_up(&self) -> bool {
        matches!(self.state, State::Up { .. })
    }

    /// The current VPN exit IP, if the VPN is up.
    pub fn exit_ip(&self) -> Option<IpAddr> {
        match self.state {
            State::Up { exit_ip } => Some(exit_ip),
            State::Down => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn iface(name: &str, desc: &str, up: bool, addrs: &[&str]) -> NetInterface {
        NetInterface {
            name: name.to_string(),
            description: desc.to_string(),
            is_up: up,
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    fn win(desc: &str) -> NetInterface {
        iface("Ethernet 3", desc, true, &["10.8.0.2"])
    }

    #[test]
    fn windows_vpn_descriptions_are_positive() {
        for d in [
            "WireGuard Tunnel",
            "Wintun Userspace Tunnel",
            "TAP-Windows Adapter V9",
            "NordLynx Tunnel",
            "ProtonVPN TUN",
            "Mullvad Tunnel",
            "Cisco AnyConnect Secure Mobility Client Virtual Miniport Adapter for Windows x64",
            "PANGP Virtual Ethernet Adapter",
            "Fortinet SSL VPN Virtual Ethernet Adapter",
        ] {
            assert!(is_vpn_interface(&win(d)), "expected VPN: {d}");
        }
    }

    #[test]
    fn windows_non_vpn_descriptions_are_negative() {
        for d in [
            "Intel(R) Wi-Fi 6 AX201 160MHz",
            "Hyper-V Virtual Ethernet Adapter",
            "vEthernet (WSL)",
            "Teredo Tunneling Pseudo-Interface",
            "Microsoft ISATAP Adapter",
            "Tailscale Tunnel",
            "ZeroTier Virtual Port",
            "VirtualBox Host-Only Ethernet Adapter",
            "Bluetooth Device (Personal Area Network)",
        ] {
            assert!(!is_vpn_interface(&win(d)), "expected non-VPN: {d}");
        }
    }

    #[test]
    fn unix_names() {
        for n in ["wg0", "tun0", "ppp0", "tap1", "ipsec0", "proton0"] {
            assert!(is_vpn_interface(&iface(n, "", true, &["10.0.0.2"])), "{n}");
        }
        for n in ["lo", "docker0", "tailscale0", "eth0", "wlan0", "tunnel", "wg", "proton"] {
            assert!(!is_vpn_interface(&iface(n, "", true, &["10.0.0.2"])), "{n}");
        }
    }

    #[test]
    fn utun_requires_routable_ipv4() {
        assert!(is_vpn_interface(&iface("utun3", "", true, &["10.2.0.2"])));
        assert!(!is_vpn_interface(&iface("utun0", "", true, &["fe80::1"])));
        assert!(!is_vpn_interface(&iface(
            "utun1",
            "",
            true,
            &["169.254.10.1", "fe80::2"]
        )));
    }

    #[test]
    fn macos_tailscale_utun_is_negative() {
        // Tailscale on macOS: utunN, no description, CGNAT IPv4 + its ULA.
        let ts = iface("utun4", "", true, &["100.101.102.103", "fd7a:115c:a1e0::1234", "fe80::1"]);
        assert!(!is_vpn_interface(&ts));
        // Same shape without the Tailscale ULA is still treated as a VPN.
        let vpn = iface("utun4", "", true, &["100.101.102.103", "fe80::1"]);
        assert!(is_vpn_interface(&vpn));
        // The ULA rule applies to every interface name.
        assert!(!is_vpn_interface(&iface("wg0", "", true, &["10.0.0.2", "fd7a:115c:a1e0:ab12::1"])));
        assert!(is_vpn_interface(&iface("wg0", "", true, &["10.0.0.2", "fd7a:115c:a1e1::1"])));
    }

    #[test]
    fn down_or_addressless_is_negative() {
        assert!(!is_vpn_interface(&iface("wg0", "", false, &["10.0.0.2"])));
        assert!(!is_vpn_interface(&iface("wg0", "", true, &[])));
    }

    #[test]
    fn vpn_interfaces_sorted() {
        let ifs = vec![
            iface("wg1", "", true, &["10.0.0.3"]),
            iface("eth0", "", true, &["192.168.1.2"]),
            iface("tun0", "", true, &["10.0.0.2"]),
        ];
        assert_eq!(VpnDetector::vpn_interfaces(&ifs), vec!["tun0", "wg1"]);
    }

    fn vpn_up() -> Vec<NetInterface> {
        vec![
            iface("eth0", "", true, &["192.168.1.2"]),
            iface("wg0", "", true, &["10.0.0.2"]),
        ]
    }

    fn vpn_down() -> Vec<NetInterface> {
        vec![iface("eth0", "", true, &["192.168.1.2"])]
    }

    #[test]
    fn down_to_up() {
        let mut d = VpnDetector::new(DetectorConfig::default());
        let t = Instant::now();
        assert!(!d.is_up());
        assert!(d.needs_ip_check(&vpn_up(), t));
        let ev = d.observe(&vpn_up(), Some(v4("1.2.3.4")), t);
        assert_eq!(
            ev,
            Some(VpnEvent::Up {
                exit_ip: v4("1.2.3.4"),
                interfaces: vec!["wg0".into()]
            })
        );
        assert!(d.is_up());
        assert_eq!(d.exit_ip(), Some(v4("1.2.3.4")));
    }

    #[test]
    fn up_same_ip_no_event_then_changed() {
        let mut d = VpnDetector::new(DetectorConfig::default());
        let t = Instant::now();
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t);
        assert_eq!(d.observe(&vpn_up(), Some(v4("1.2.3.4")), t), None);
        let ev = d.observe(&vpn_up(), Some(v4("5.6.7.8")), t);
        assert_eq!(
            ev,
            Some(VpnEvent::Changed {
                exit_ip: v4("5.6.7.8"),
                interfaces: vec!["wg0".into()]
            })
        );
        assert_eq!(d.exit_ip(), Some(v4("5.6.7.8")));
    }

    #[test]
    fn up_to_down() {
        let mut d = VpnDetector::new(DetectorConfig::default());
        let t = Instant::now();
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t);
        assert!(d.needs_ip_check(&vpn_down(), t));
        assert_eq!(d.observe(&vpn_down(), None, t), Some(VpnEvent::Down));
        assert!(!d.is_up());
        assert_eq!(d.exit_ip(), None);
        assert_eq!(d.observe(&vpn_down(), None, t), None);
        assert!(!d.needs_ip_check(&vpn_down(), t));
    }

    #[test]
    fn unknown_ip_keeps_state() {
        let mut d = VpnDetector::new(DetectorConfig::default());
        let t = Instant::now();
        // Down stays down while IP is unknown, and we keep asking for a check.
        assert_eq!(d.observe(&vpn_up(), None, t), None);
        assert!(!d.is_up());
        assert!(d.needs_ip_check(&vpn_up(), t));
        // Up stays up with the same exit while IP is unknown.
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t);
        assert_eq!(d.observe(&vpn_up(), None, t), None);
        assert_eq!(d.exit_ip(), Some(v4("1.2.3.4")));
    }

    #[test]
    fn recheck_interval() {
        let cfg = DetectorConfig {
            ip_recheck: Duration::from_secs(60),
        };
        let mut d = VpnDetector::new(cfg);
        let t0 = Instant::now();
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t0);
        assert!(!d.needs_ip_check(&vpn_up(), t0 + Duration::from_secs(30)));
        assert!(d.needs_ip_check(&vpn_up(), t0 + Duration::from_secs(60)));
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t0 + Duration::from_secs(61));
        assert!(!d.needs_ip_check(&vpn_up(), t0 + Duration::from_secs(90)));
    }

    #[test]
    fn interface_set_change_triggers_check() {
        let mut d = VpnDetector::new(DetectorConfig::default());
        let t = Instant::now();
        d.observe(&vpn_up(), Some(v4("1.2.3.4")), t);
        let mut more = vpn_up();
        more.push(iface("tun0", "", true, &["10.9.0.2"]));
        assert!(d.needs_ip_check(&more, t));
        // Failed lookup does not record the new set, so we keep asking.
        d.observe(&more, None, t);
        assert!(d.needs_ip_check(&more, t));
        d.observe(&more, Some(v4("1.2.3.4")), t);
        assert!(!d.needs_ip_check(&more, t));
    }
}
