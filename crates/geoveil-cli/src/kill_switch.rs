//! Explicit opt-in entry point for the Windows firewall guard.

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use geoveil_platform::{leaks::list_leak_interfaces, windows::kill_switch};
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Udp,
    Tcp,
}

impl Transport {
    fn protocol(self) -> u8 {
        match self {
            Self::Udp => 17,
            Self::Tcp => 6,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
        }
    }
}

#[derive(Debug, Args)]
pub struct Options {
    /// Absolute path to the actual VPN transport process executable.
    #[arg(long, value_name = "EXE")]
    client: PathBuf,

    /// VPN server as a numeric IPv4 address and port. Use the transport server, not the public exit.
    #[arg(long, value_name = "IP:PORT")]
    server: SocketAddrV4,

    /// Transport used by the VPN client to connect to that server.
    #[arg(long, value_enum)]
    transport: Transport,

    /// Name of the active VPN adapter. Required if multiple VPN adapters are detected.
    #[arg(long)]
    interface: Option<String>,

    /// Install temporary firewall rules until Ctrl+C. Omit to preview the plan.
    #[arg(long)]
    enable: bool,

    /// Automatically remove the rules after this many seconds for a bounded test.
    #[arg(long, requires = "enable", value_parser = clap::value_parser!(u64).range(5..=120))]
    duration_secs: Option<u64>,
}

pub async fn run(options: &Options, json: bool) -> Result<crate::commands::Exit> {
    validate_server(options.server)?;
    let client = executable(&options.client)?;
    let interfaces = list_leak_interfaces();
    let tunnel = select_interface(&interfaces, options.interface.as_deref())?;
    let luid = kill_switch::interface_luid(tunnel.index).context("resolve the selected VPN adapter")?;
    let endpoint = options.server;
    let transport = options.transport.label();
    let plan = serde_json::json!({
        "active": options.enable,
        "interface": tunnel.display_name(),
        "client": client,
        "server": endpoint.to_string(),
        "transport": transport,
        "session_scoped": true,
        "duration_secs": options.duration_secs,
        "experimental": true,
    });

    if !options.enable {
        let mut plan = plan;
        plan["message"] = "Preview only. No firewall rules were installed.".into();
        crate::output::emit(json, &plan, || format!(
            "Preview only (experimental). No firewall rules were installed.\n\
             VPN adapter: {}\n\
             VPN transport: {} → {endpoint} ({transport})\n\
             Allow this transport, the selected VPN adapter, loopback and Windows DHCPv4.\n\
             Block other IPv4 and IPv6 connections until protection stops.\n",
            tunnel.display_name(), client.display(),
        ));
        return Ok(crate::commands::Exit::Ok);
    }

    // Hold the session in this console process. A VPN disconnect does not drop
    // the WFP session or automatically permit another adapter.
    let mut stop = tokio::signal::windows::ctrl_c().context("register Ctrl+C")?;
    let guard = kill_switch::start(luid, &client, endpoint, options.transport.protocol())
        .context("could not start experimental VPN protection")?;
    crate::output::emit(json, &plan, || {
        let stop_message = match options.duration_secs {
            Some(seconds) => format!("Protection ends after {seconds} seconds or when you press Ctrl+C."),
            None => "Keep this terminal open. Press Ctrl+C to stop protection.".to_string(),
        };
        format!("Experimental VPN protection active on {}.\n{stop_message}\n", tunnel.display_name())
    });
    std::io::Write::flush(&mut std::io::stdout()).context("publish protection status")?;
    match options.duration_secs {
        Some(seconds) => {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(seconds), stop.recv()).await;
        }
        None => {
            stop.recv().await;
        }
    }
    drop(guard);
    eprintln!("Geoveil VPN protection stopped.");
    Ok(crate::commands::Exit::Ok)
}

fn select_interface<'a>(
    interfaces: &'a [geoveil_core::leaks::LeakInterface],
    requested: Option<&str>,
) -> Result<&'a geoveil_core::leaks::LeakInterface> {
    let mut matches = interfaces.iter().filter(|interface| {
        interface.index != 0
            && !interface.is_loopback
            && interface.is_vpn()
            && requested.is_none_or(|name| {
                interface.name.eq_ignore_ascii_case(name)
                    || interface.display_name().eq_ignore_ascii_case(name)
            })
    });
    let Some(interface) = matches.next() else {
        bail!("no matching active VPN adapter; connect the VPN and check the adapter name in Geoveil");
    };
    if matches.next().is_some() {
        bail!("multiple VPN adapters match; specify one active VPN adapter with --interface");
    }
    Ok(interface)
}

fn validate_server(server: SocketAddrV4) -> Result<()> {
    let ip = server.ip();
    if server.port() == 0 || ip.is_unspecified() || ip.is_loopback() ||
        ip.is_multicast() || ip.is_broadcast() || ip.is_link_local() {
        bail!("the VPN transport server must be a reachable unicast IPv4 address and nonzero port");
    }
    Ok(())
}

fn executable(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("the VPN transport executable path must be absolute");
    }
    let path = path.canonicalize().context("find the VPN transport executable")?;
    if !path.is_file() || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
        bail!("the VPN transport path must point to an existing .exe file");
    }
    // Convert Rust's verbatim drive prefix back to a conventional drive path
    // for FwpmGetAppIdFromFileName0. Reject CWD- and network-dependent paths.
    let text = path.to_str().context("VPN executable path is not valid Unicode")?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    if !text.as_bytes().get(1..3).is_some_and(|suffix| suffix == b":\\") {
        bail!("the VPN transport executable must be on a local Windows drive");
    }
    Ok(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoveil_core::leaks::LeakInterface;

    fn adapter(index: u32, name: &str, description: &str, is_up: bool) -> LeakInterface {
        LeakInterface {
            index,
            name: name.into(),
            description: description.into(),
            is_up,
            is_loopback: false,
            has_gateway: true,
            addrs: vec!["192.0.2.42".parse().unwrap()],
        }
    }

    #[test]
    fn only_one_active_vpn_is_eligible_and_can_be_selected_by_friendly_name() {
        let mut interfaces = vec![
            adapter(4, "Ethernet", "Realtek PCIe", true),
            adapter(8, "adapter-guid", "Office VPN | TAP-Windows Adapter V9", true),
            adapter(9, "Other VPN", "Wintun", false),
        ];
        assert_eq!(select_interface(&interfaces, None).unwrap().index, 8);
        assert!(select_interface(&interfaces, Some("Ethernet")).is_err());
        assert!(select_interface(&interfaces, Some("Other VPN")).is_err());
        interfaces[2].is_up = true;
        assert!(select_interface(&interfaces, None).is_err());
        assert_eq!(select_interface(&interfaces, Some("office vpn")).unwrap().index, 8);
    }

    #[test]
    fn transport_exception_rejects_broad_and_localhost_endpoints() {
        for server in ["0.0.0.0:1194", "127.0.0.1:443", "224.0.0.1:443", "192.0.2.5:0"] {
            assert!(validate_server(server.parse().unwrap()).is_err(), "{server}");
        }
        assert!(validate_server("192.0.2.5:1194".parse().unwrap()).is_ok());
    }
}
