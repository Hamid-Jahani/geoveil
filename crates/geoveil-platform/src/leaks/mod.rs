//! Platform probes for `geoveil leaks`: interface details, DNS resolver
//! discovery and interface-bound STUN probes.
//!
//! The verdicts live in `geoveil_core::leaks`; this module only observes.
//! Nothing here changes system settings.
//!
//! - [`list_leak_interfaces`]: interfaces with index, loopback and gateway
//!   flags (via `netdev`).
//! - [`dns`]: per-interface resolvers through [`CommandRunner`] (Windows
//!   `Get-DnsClientServerAddress`, Linux `resolvectl` with an
//!   `/etc/resolv.conf` fallback, macOS `scutil --dns`). Parsers are pure.
//! - [`stun`]: UDP STUN Binding requests bound to one interface
//!   (`IP_UNICAST_IF` on Windows, `SO_BINDTOIFINDEX` on Linux,
//!   `IP_BOUND_IF` on macOS) or to the default route.
//!
//! [`CommandRunner`]: crate::runner::CommandRunner

pub mod dns;
pub mod stun;

use crate::interfaces::build_interface;
use geoveil_core::leaks::{is_global_v6, LeakInterface};
use std::net::IpAddr;

/// Lists the host's interfaces with what the leak checks need.
pub fn list_leak_interfaces() -> Vec<LeakInterface> {
    netdev::get_interfaces()
        .into_iter()
        .map(|iface| {
            let addrs: Vec<IpAddr> = iface
                .ipv4
                .iter()
                .map(|n| IpAddr::V4(n.addr()))
                .chain(iface.ipv6.iter().map(|n| IpAddr::V6(n.addr())))
                .collect();
            let base = build_interface(
                &iface.name,
                iface.friendly_name.as_deref(),
                iface.description.as_deref(),
                iface.is_up(),
                addrs,
            );
            LeakInterface {
                index: iface.index,
                name: base.name,
                description: base.description,
                is_up: base.is_up,
                is_loopback: iface.is_loopback(),
                has_gateway: iface.gateway.is_some(),
                addrs: base.addrs,
            }
        })
        .collect()
}

/// Where one STUN probe's socket is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    /// `None` for the default route (unbound socket).
    pub interface: Option<BoundInterface>,
    /// Address family of the probe.
    pub ipv6: bool,
}

/// An interface-bound probe source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundInterface {
    /// Display name for the report.
    pub name: String,
    pub index: u32,
    pub local: IpAddr,
}

/// Picks the STUN probe sources: one default-route probe per family that the
/// host can use, plus one probe per family on every up, non-loopback, non-VPN
/// interface with a usable address.
///
/// Interfaces without a default gateway (host-only adapters such as Hyper-V
/// `vEthernet`) are skipped, unless no interface reports a gateway at all
/// (gateway detection failed), in which case every candidate is probed and
/// "no route" answers are reported as such.
pub fn select_targets(interfaces: &[LeakInterface]) -> Vec<ProbeTarget> {
    let usable_v4 = |a: &IpAddr| matches!(a, IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified());
    let usable_v6 = |a: &IpAddr| matches!(a, IpAddr::V6(v6) if is_global_v6(v6));
    let candidates: Vec<&LeakInterface> =
        interfaces.iter().filter(|i| i.is_up && !i.is_loopback && i.index != 0 && !i.is_vpn()).collect();
    let any_gateway = candidates.iter().any(|i| i.has_gateway);

    let mut out = vec![ProbeTarget { interface: None, ipv6: false }];
    if interfaces.iter().any(|i| i.is_up && i.addrs.iter().any(usable_v6)) {
        out.push(ProbeTarget { interface: None, ipv6: true });
    }
    for iface in candidates.into_iter().filter(|i| i.has_gateway || !any_gateway) {
        for (ipv6, pick) in [(false, &usable_v4 as &dyn Fn(&IpAddr) -> bool), (true, &usable_v6)] {
            if let Some(local) = iface.addrs.iter().copied().find(|a| pick(a)) {
                out.push(ProbeTarget {
                    interface: Some(BoundInterface {
                        name: iface.display_name().to_string(),
                        index: iface.index,
                        local,
                    }),
                    ipv6,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(index: u32, name: &str, desc: &str, gw: bool, addrs: &[&str]) -> LeakInterface {
        LeakInterface {
            index,
            name: name.into(),
            description: desc.into(),
            is_up: true,
            is_loopback: false,
            has_gateway: gw,
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    fn names(t: &[ProbeTarget]) -> Vec<String> {
        t.iter()
            .map(|t| match &t.interface {
                Some(b) => format!("{}/{}", b.name, b.local),
                None => format!("default/{}", if t.ipv6 { "v6" } else { "v4" }),
            })
            .collect()
    }

    #[test]
    fn selects_physical_interfaces_with_gateway() {
        let ifs = vec![
            iface(4, "{E}", "Ethernet | Intel", true, &["fe80::1", "192.168.1.118", "2a01:4f8::10"]),
            iface(17, "{W}", "wg | WireGuard Tunnel", false, &["10.64.0.2"]),
            iface(30, "{H}", "vEthernet (WSL)", false, &["172.26.32.1"]),
            LeakInterface { is_loopback: true, ..iface(1, "lo", "Loopback", false, &["127.0.0.1"]) },
            LeakInterface { is_up: false, ..iface(9, "{F}", "Wi-Fi", true, &["192.168.0.5"]) },
        ];
        assert_eq!(
            names(&select_targets(&ifs)),
            vec!["default/v4", "default/v6", "Ethernet/192.168.1.118", "Ethernet/2a01:4f8::10"]
        );
    }

    #[test]
    fn without_gateway_info_probes_every_candidate() {
        let ifs = vec![
            iface(2, "eth0", "", false, &["192.168.1.5"]),
            iface(3, "docker0", "", false, &["172.17.0.1"]),
        ];
        assert_eq!(names(&select_targets(&ifs)), vec!["default/v4", "eth0/192.168.1.5", "docker0/172.17.0.1"]);
    }

    #[test]
    fn link_local_only_interface_is_skipped() {
        let ifs = vec![iface(5, "en0", "", true, &["169.254.3.4", "fe80::2"])];
        assert_eq!(names(&select_targets(&ifs)), vec!["default/v4"]);
    }
}
