//! Network interface enumeration via the `netdev` crate.
//!
//! Conversion from `netdev::Interface` to [`NetInterface`] is split into a tiny
//! pure helper ([`build_interface`]) so the logic is unit tested without
//! touching the host's real adapters.
//!
//! # `netdev` API assumptions (verify when bumping the crate)
//! - `netdev::get_interfaces() -> Vec<netdev::Interface>`
//! - `Interface::name: String` (on Windows this is the adapter GUID, e.g. `{6B29...}`)
//! - `Interface::friendly_name: Option<String>` (Windows: "Ethernet", "WireGuard Tunnel")
//! - `Interface::description: Option<String>` (Windows: "Wintun Userspace Tunnel")
//! - `Interface::ipv4: Vec<Ipv4Net>` / `Interface::ipv6: Vec<Ipv6Net>` where the
//!   net types expose `.addr()` (true for the `ipnet` types netdev re-exports;
//!   very old netdev versions had a public `addr` field instead).
//! - `Interface::is_up() -> bool` (derived from `IFF_UP` flags; on Windows netdev
//!   sets the flag from the adapter's operational status).

use geoveil_core::NetInterface;
use std::net::IpAddr;

/// Lists all network interfaces on the host.
///
/// `description` combines the friendly name and the adapter description (when
/// present and distinct) so VPN detection can match either one.
pub fn list_interfaces() -> Vec<NetInterface> {
    netdev::get_interfaces()
        .into_iter()
        .map(|iface| {
            let addrs: Vec<IpAddr> = iface
                .ipv4
                .iter()
                .map(|net| IpAddr::V4(net.addr()))
                .chain(iface.ipv6.iter().map(|net| IpAddr::V6(net.addr())))
                .collect();
            build_interface(
                &iface.name,
                iface.friendly_name.as_deref(),
                iface.description.as_deref(),
                iface.is_up(),
                addrs,
            )
        })
        .collect()
}

/// Pure conversion from raw interface fields into a [`NetInterface`].
///
/// Non-empty, trimmed `friendly_name` and `description` are joined with `" | "`;
/// a description equal (case-insensitively) to the friendly name is dropped.
/// Duplicate addresses are removed while preserving order.
pub(crate) fn build_interface(
    name: &str,
    friendly_name: Option<&str>,
    description: Option<&str>,
    is_up: bool,
    addrs: Vec<IpAddr>,
) -> NetInterface {
    NetInterface {
        name: name.trim().to_string(),
        description: join_description(friendly_name, description),
        is_up,
        addrs: dedup_preserving_order(addrs),
    }
}

/// Joins the friendly name and description, skipping empty and duplicate parts.
fn join_description(friendly_name: Option<&str>, description: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(2);
    for part in [friendly_name, description].into_iter().flatten() {
        let part = part.trim();
        if part.is_empty() || parts.iter().any(|p| p.eq_ignore_ascii_case(part)) {
            continue;
        }
        parts.push(part);
    }
    parts.join(" | ")
}

/// Removes duplicate addresses, keeping the first occurrence.
fn dedup_preserving_order(addrs: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::with_capacity(addrs.len());
    for addr in addrs {
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn joins_friendly_name_and_description() {
        let i = build_interface(
            "{6B29FC40-CA47-1067-B31D-00DD010662DA}",
            Some("WireGuard Tunnel"),
            Some("Wintun Userspace Tunnel"),
            true,
            vec![IpAddr::V4(Ipv4Addr::new(10, 2, 0, 2))],
        );
        assert_eq!(i.name, "{6B29FC40-CA47-1067-B31D-00DD010662DA}");
        assert_eq!(i.description, "WireGuard Tunnel | Wintun Userspace Tunnel");
        assert!(i.is_up);
        assert_eq!(i.addrs, vec![IpAddr::V4(Ipv4Addr::new(10, 2, 0, 2))]);
    }

    #[test]
    fn drops_duplicate_and_empty_description_parts() {
        let i = build_interface("eth0", Some("Ethernet"), Some("ethernet"), false, vec![]);
        assert_eq!(i.description, "Ethernet");
        let i = build_interface("wg0", None, Some("   "), true, vec![]);
        assert_eq!(i.description, "");
        let i = build_interface("utun3", None, None, true, vec![]);
        assert_eq!(i.description, "");
        let i = build_interface("x", Some(""), Some("Desc"), true, vec![]);
        assert_eq!(i.description, "Desc");
    }

    #[test]
    fn keeps_v4_and_v6_and_dedups() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let i = build_interface(" tun0 ", None, None, true, vec![v4, v6, v4]);
        assert_eq!(i.name, "tun0");
        assert_eq!(i.addrs, vec![v4, v6]);
    }
}
