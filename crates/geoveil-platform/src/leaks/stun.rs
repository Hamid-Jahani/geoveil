//! Interface-bound UDP STUN probes (blocking; run them on a blocking thread).
//!
//! A browser gathers WebRTC "server-reflexive" candidates on every interface,
//! so a STUN Binding request sent from the physical adapter is a faithful
//! stand-in for what a web page could learn.
//!
//! # Binding a socket to one interface
//!
//! Both the source address and the interface are bound, because Linux uses
//! the weak host model (an address bind alone can still leave through a
//! tunnel's policy route) and Windows needs an explicit choice too.
//!
//! | OS | Call |
//! |---|---|
//! | Windows | raw `setsockopt(IPPROTO_IP, IP_UNICAST_IF)` with the index in **network** byte order; `setsockopt(IPPROTO_IPV6, IPV6_UNICAST_IF)` in **host** order (socket2 has no wrapper) |
//! | Linux | `SO_BINDTOIFINDEX` via `socket2::Socket::bind_device_by_index_v4/v6` |
//! | macOS | `IP_BOUND_IF` / `IPV6_BOUND_IF` via the same socket2 calls |
//!
//! Retransmission follows RFC 8489's doubling RTO, shortened for a CLI:
//! three sends with 500, 1000 and 2000 ms waits.
//!
//! An error while creating or binding the socket means the probe never ran,
//! so it is reported as [`StunOutcome::Untested`], never as `Blocked`: on
//! Linux before 5.7 an unprivileged `SO_BINDTOIFINDEX` fails with `EPERM`
//! (and the option does not exist before 5.0), which must not read as "a
//! firewall dropped it". Only errors from sending or receiving map to
//! network outcomes (see [`classify_io_error`]).

use super::ProbeTarget;
use geoveil_core::leaks::stun::{decode_binding_response, encode_binding_request, random_transaction_id, StunError};
use geoveil_core::leaks::{StunOutcome, StunProbe};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Default public STUN servers.
pub const DEFAULT_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

/// Wait after each send (RFC 8489 RTO 500 ms, doubled, three tries).
pub const DEFAULT_SCHEDULE: [Duration; 3] =
    [Duration::from_millis(500), Duration::from_millis(1000), Duration::from_millis(2000)];

const SOFTWARE: &str = concat!("geoveil/", env!("CARGO_PKG_VERSION"));

/// Resolves `host:port` with the system resolver into (first IPv4, first IPv6).
pub fn resolve_server(server: &str) -> io::Result<(Option<SocketAddr>, Option<SocketAddr>)> {
    let addrs: Vec<SocketAddr> = server.to_socket_addrs()?.collect();
    Ok((addrs.iter().copied().find(SocketAddr::is_ipv4), addrs.iter().copied().find(SocketAddr::is_ipv6)))
}

/// Probes every target against every server. Blocking: up to
/// `targets * servers * sum(schedule)` in the worst case.
pub fn probe_all(targets: &[ProbeTarget], servers: &[String], schedule: &[Duration]) -> Vec<StunProbe> {
    let mut out = Vec::new();
    for server in servers {
        let resolved = resolve_server(server);
        for target in targets {
            let server_addr = match &resolved {
                Ok((v4, v6)) => if target.ipv6 { *v6 } else { *v4 },
                Err(_) => None,
            };
            let outcome = match (&resolved, server_addr) {
                (Err(e), _) => StunOutcome::Failed { detail: format!("cannot resolve {server}: {e}") },
                (Ok(_), None) => StunOutcome::Failed {
                    detail: format!("{server} has no {} address", if target.ipv6 { "IPv6" } else { "IPv4" }),
                },
                (Ok(_), Some(addr)) => probe(target, addr, schedule),
            };
            out.push(StunProbe {
                server: server.clone(),
                server_addr,
                interface: target.interface.as_ref().map(|b| b.name.clone()),
                local: target.interface.as_ref().map(|b| b.local),
                outcome,
            });
        }
    }
    out
}

/// One STUN Binding transaction from `target` to `server`.
pub fn probe(target: &ProbeTarget, server: SocketAddr, schedule: &[Duration]) -> StunOutcome {
    let socket = match open_socket(target, server) {
        Ok(socket) => socket,
        Err(e) => return classify_setup_error(&e, target.interface.is_some()),
    };
    match run_probe(&socket, server, schedule) {
        Ok(outcome) => outcome,
        Err(e) => classify_io_error(&e),
    }
}

/// Creates the UDP socket and binds it to the target interface and address.
fn open_socket(target: &ProbeTarget, server: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(server), Type::DGRAM, Some(Protocol::UDP))?;
    let local = match &target.interface {
        Some(b) => {
            bind_to_interface(&socket, b.local, b.index)?;
            SocketAddr::new(b.local, 0)
        }
        None if server.is_ipv4() => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        None => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    socket.bind(&SockAddr::from(local))?;
    Ok(socket.into())
}

fn run_probe(socket: &std::net::UdpSocket, server: SocketAddr, schedule: &[Duration]) -> io::Result<StunOutcome> {

    let tid = random_transaction_id();
    let request = encode_binding_request(&tid, Some(SOFTWARE));
    let mut buf = [0u8; 1500];
    let mut last_bad: Option<StunError> = None;
    for wait in schedule {
        socket.send_to(&request, server)?;
        let deadline = Instant::now() + *wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(left))?;
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from.ip() == server.ip() => match decode_binding_response(&buf[..n], &tid) {
                    Ok(addr) => return Ok(StunOutcome::Mapped { addr }),
                    // Stale or foreign datagrams: keep listening.
                    Err(StunError::WrongTransaction | StunError::NotStun) => {}
                    Err(e) => last_bad = Some(e),
                },
                Ok(_) => {} // not from the server we asked
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(match last_bad {
        Some(e) => StunOutcome::Failed { detail: format!("bad response: {e}") },
        None => StunOutcome::Timeout,
    })
}

/// Maps an error from creating or binding the socket: the probe never ran.
pub fn classify_setup_error(err: &io::Error, interface_bound: bool) -> StunOutcome {
    let hint = if interface_bound && err.kind() == io::ErrorKind::PermissionDenied {
        " (on Linux before kernel 5.7, binding to an interface needs root or CAP_NET_RAW)"
    } else {
        ""
    };
    StunOutcome::Untested { detail: format!("cannot bind the socket: {err}{hint}") }
}

/// Maps an error from sending or receiving to a probe outcome.
pub fn classify_io_error(err: &io::Error) -> StunOutcome {
    use io::ErrorKind as K;
    let detail = err.to_string();
    match err.kind() {
        K::NetworkUnreachable | K::HostUnreachable | K::AddrNotAvailable | K::NetworkDown => {
            StunOutcome::NoRoute { detail }
        }
        // A local firewall rejecting the datagram (an OUTPUT rule makes
        // sendto fail with EPERM on Linux).
        K::PermissionDenied => StunOutcome::Blocked { detail: format!("refused locally: {detail}") },
        K::ConnectionReset | K::ConnectionRefused => {
            StunOutcome::Blocked { detail: format!("port unreachable: {detail}") }
        }
        K::WouldBlock | K::TimedOut => StunOutcome::Timeout,
        _ => StunOutcome::Failed { detail },
    }
}

/// The `IP_UNICAST_IF` / `IPV6_UNICAST_IF` option value for `index`: IPv4
/// wants the index in network byte order, IPv6 in host byte order.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn unicast_if_value(ipv6: bool, index: u32) -> u32 {
    if ipv6 {
        index
    } else {
        index.to_be()
    }
}

#[cfg(windows)]
fn bind_to_interface(socket: &Socket, local: IpAddr, index: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        getsockopt, setsockopt, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, IP_UNICAST_IF, SOCKET_ERROR,
    };
    let (level, name) = match local {
        IpAddr::V4(_) => (IPPROTO_IP, IP_UNICAST_IF),
        IpAddr::V6(_) => (IPPROTO_IPV6, IPV6_UNICAST_IF),
    };
    let value = unicast_if_value(local.is_ipv6(), index);
    let raw = socket.as_raw_socket() as usize;
    // SAFETY: `raw` is a valid socket owned by `socket` for the whole call,
    // and the option value is a live u32 whose size is passed as the length.
    let rc = unsafe { setsockopt(raw, level, name, (&value as *const u32).cast::<u8>(), 4) };
    if rc == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    // Self-check: getsockopt returns the index in host byte order.
    let mut back: u32 = 0;
    let mut len: i32 = 4;
    // SAFETY: as above; `back` and `len` are live and correctly sized.
    let rc = unsafe { getsockopt(raw, level, name, (&mut back as *mut u32).cast::<u8>(), &mut len) };
    if rc != SOCKET_ERROR && back != index {
        tracing::debug!(index, back, "unicast interface read-back differs from the requested index");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
fn bind_to_interface(socket: &Socket, local: IpAddr, index: u32) -> io::Result<()> {
    let index = std::num::NonZeroU32::new(index);
    match local {
        IpAddr::V4(_) => socket.bind_device_by_index_v4(index),
        IpAddr::V6(_) => socket.bind_device_by_index_v6(index),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
fn bind_to_interface(_socket: &Socket, _local: IpAddr, _index: u32) -> io::Result<()> {
    // Only the source address is bound on other systems.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaks::BoundInterface;
    use geoveil_core::leaks::stun::{BINDING_SUCCESS, MAGIC_COOKIE};

    #[test]
    fn unicast_if_byte_order() {
        assert_eq!(unicast_if_value(false, 7), 7u32.to_be());
        assert_eq!(unicast_if_value(false, 7).to_ne_bytes(), [0, 0, 0, 7]);
        assert_eq!(unicast_if_value(true, 7), 7);
    }

    #[test]
    fn io_errors_map_to_outcomes() {
        let o = |k| classify_io_error(&io::Error::from(k));
        assert!(matches!(o(io::ErrorKind::NetworkUnreachable), StunOutcome::NoRoute { .. }));
        assert!(matches!(o(io::ErrorKind::HostUnreachable), StunOutcome::NoRoute { .. }));
        assert!(matches!(o(io::ErrorKind::AddrNotAvailable), StunOutcome::NoRoute { .. }));
        assert!(matches!(o(io::ErrorKind::PermissionDenied), StunOutcome::Blocked { .. }));
        assert!(matches!(o(io::ErrorKind::ConnectionReset), StunOutcome::Blocked { .. }));
        assert_eq!(o(io::ErrorKind::TimedOut), StunOutcome::Timeout);
        assert!(matches!(o(io::ErrorKind::InvalidData), StunOutcome::Failed { .. }));
    }

    #[test]
    fn setup_errors_are_untested_not_blocked() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        match classify_setup_error(&denied, true) {
            StunOutcome::Untested { detail } => assert!(detail.contains("CAP_NET_RAW"), "{detail}"),
            other => panic!("expected untested, got {other:?}"),
        }
        let unsupported = io::Error::from(io::ErrorKind::Unsupported);
        assert!(matches!(classify_setup_error(&unsupported, true), StunOutcome::Untested { .. }));
        assert!(matches!(
            classify_setup_error(&denied, false),
            StunOutcome::Untested { detail } if !detail.contains("CAP_NET_RAW")
        ));
    }

    #[test]
    fn unbindable_local_address_is_untested() {
        // 192.0.2.10 (TEST-NET-1) is not an address of this machine, so the
        // bind fails before anything is sent. Index 0 skips the interface
        // option, so only the address bind is exercised.
        let target = ProbeTarget {
            interface: Some(BoundInterface { name: "eth0".into(), index: 0, local: "192.0.2.10".parse().unwrap() }),
            ipv6: false,
        };
        let outcome = probe(&target, "127.0.0.1:9".parse().unwrap(), &[Duration::from_millis(10)]);
        assert!(matches!(outcome, StunOutcome::Untested { .. }), "{outcome:?}");
    }

    /// A tiny STUN server on loopback, so the socket path is exercised
    /// without any network access.
    fn loopback_server(reply: bool) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            if !reply {
                // Swallow requests and keep the port open (a closed port
                // would answer with ICMP port unreachable) until idle.
                sock.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                while sock.recv_from(&mut buf).is_ok() {}
                return;
            }
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let Ok((n, from)) = sock.recv_from(&mut buf) else { return };
            if n < 20 {
                return;
            }
            let IpAddr::V4(v4) = from.ip() else { return };
            let mut msg = Vec::new();
            msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
            msg.extend_from_slice(&12u16.to_be_bytes());
            msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
            msg.extend_from_slice(&buf[8..20]);
            msg.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            msg.extend_from_slice(&(from.port() ^ 0x2112).to_be_bytes());
            msg.extend_from_slice(&(u32::from(v4) ^ MAGIC_COOKIE).to_be_bytes());
            sock.send_to(&msg, from).unwrap();
        });
        (addr, handle)
    }

    #[test]
    fn loopback_round_trip() {
        let (server, handle) = loopback_server(true);
        let target = ProbeTarget { interface: None, ipv6: false };
        let outcome = probe(&target, server, &[Duration::from_secs(2)]);
        handle.join().unwrap();
        match outcome {
            StunOutcome::Mapped { addr } => assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST)),
            other => panic!("expected a mapped address, got {other:?}"),
        }
    }

    #[test]
    fn silent_server_times_out() {
        let (server, handle) = loopback_server(false);
        let target = ProbeTarget { interface: None, ipv6: false };
        let outcome = probe(&target, server, &[Duration::from_millis(100), Duration::from_millis(100)]);
        handle.join().unwrap();
        assert_eq!(outcome, StunOutcome::Timeout);
    }

    #[test]
    fn unresolvable_server_is_reported_per_target() {
        // A server without a port fails to parse before any DNS lookup, so
        // no packet leaves the machine.
        let targets = vec![
            ProbeTarget { interface: None, ipv6: false },
            ProbeTarget {
                interface: Some(BoundInterface { name: "eth0".into(), index: 2, local: "192.0.2.10".parse().unwrap() }),
                ipv6: false,
            },
        ];
        let probes = probe_all(&targets, &["stun.invalid".to_string()], &[Duration::from_millis(10)]);
        assert_eq!(probes.len(), 2);
        assert!(probes.iter().all(|p| matches!(p.outcome, StunOutcome::Failed { .. })));
        assert_eq!(probes[1].interface.as_deref(), Some("eth0"));
    }
}
