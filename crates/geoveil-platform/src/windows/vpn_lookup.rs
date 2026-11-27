use crate::leaks::list_leak_interfaces;
use crate::runner::{CommandRunner, SystemRunner};
use geoveil_core::geo::{BoundLookup, LOOKUP_HOSTS};
use geoveil_core::leaks::LeakInterface;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use windows_sys::Win32::NetworkManagement::IpHelper::GetBestInterface;

fn lookup_adapter(interfaces: &[LeakInterface]) -> Result<Option<&LeakInterface>, String> {
    let mut active = interfaces.iter().filter(|iface| iface.is_vpn());
    let Some(adapter) = active.find(|iface| {
        let description = iface.description.to_ascii_lowercase();
        ["openvpn", "sing-tun", "singbox_tun"].iter().any(|name| description.contains(name))
            || iface.name.eq_ignore_ascii_case("singbox_tun")
    }) else {
        return Ok(None);
    };
    if interfaces.iter().filter(|iface| iface.is_vpn()).count() != 1 {
        return Err("Multiple VPN adapters are active. Leave only the VPN you want to check connected.".into());
    }
    Ok(Some(adapter))
}

/// Use the active OpenVPN or sing-box adapter's IPv4 resolver and source address.
/// Never switch to a general resolver or unrelated proxy on failure.
pub fn prepare() -> Result<Option<BoundLookup>, String> {
    let interfaces = list_leak_interfaces();
    let Some(adapter) = lookup_adapter(&interfaces)? else {
        return Ok(None);
    };
    let label = adapter.display_name();
    if adapter.index == 0 {
        return Err(format!("Windows did not report an interface index for {label}."));
    }
    let Some(local) = adapter.addrs.iter().find_map(|ip| match ip {
        IpAddr::V4(ip) if !ip.is_link_local() && !ip.is_loopback() && !ip.is_unspecified() => Some(*ip),
        _ => None,
    }) else {
        return Err(format!("The VPN adapter {label} has no usable IPv4 address."));
    };
    let names = LOOKUP_HOSTS.map(|host| format!("'{host}'")).join(",");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop';
        $index = {index};
        $otherInterfaces = Get-NetIPInterface -AddressFamily IPv4 | Where-Object {{ $_.InterfaceIndex -ne $index -and $_.ConnectionState -eq 'Connected' }};
        if ($otherInterfaces | Where-Object WeakHostSend -ne 'Disabled') {{ throw 'IPv4 weak host send is enabled on another active adapter.' }}
        $resolvers = @((Get-DnsClientServerAddress -InterfaceIndex $index -AddressFamily IPv4).ServerAddresses);
        $resolver = $resolvers | Where-Object {{
            $routes = @(Find-NetRoute -RemoteIPAddress $_ -ErrorAction SilentlyContinue);
            $routes.Count -gt 0 -and !($routes | Where-Object InterfaceIndex -ne $index)
        }} | Select-Object -First 1;
        if (!$resolver) {{ throw 'No IPv4 DNS server is routed through the VPN adapter.' }}
        $addresses = @{{}};
        $pool = [RunspaceFactory]::CreateRunspacePool(1,{count});
        $workers = @();
        try {{
            $pool.Open();
            foreach ($hostname in @({names})) {{
                $query = [PowerShell]::Create();
                $query.RunspacePool = $pool;
                [void]$query.AddScript({{
                    param($hostname, $resolver)
                    Resolve-DnsName -Name $hostname -Type A -DnsOnly -NoHostsFile -QuickTimeout -Server $resolver -ErrorAction SilentlyContinue |
                        Where-Object Type -eq A | Select-Object -ExpandProperty IPAddress
                }}).AddArgument($hostname).AddArgument($resolver);
                $workers += @{{ host = $hostname; process = $query; task = $query.BeginInvoke() }};
            }}
            foreach ($worker in $workers) {{
                $addresses[$worker.host] = @($worker.process.EndInvoke($worker.task) | ForEach-Object {{ [string]$_ }});
            }}
        }} finally {{
            foreach ($worker in $workers) {{ $worker.process.Dispose() }}
            $pool.Dispose();
        }}
        ConvertTo-Json -InputObject $addresses -Compress "#,
        index = adapter.index,
        count = LOOKUP_HOSTS.len(),
    );
    let output = SystemRunner.run("powershell", &["-NoProfile", "-NonInteractive", "-Command", &script])
        .map_err(|error| format!("Could not query DNS on {label}: {error}"))?;
    if !output.success() {
        let detail = output.stderr.trim();
        return Err(format!("Could not query DNS on {label}: {detail}"));
    }
    let mut addresses: BTreeMap<String, Vec<Ipv4Addr>> = serde_json::from_str(&output.stdout)
        .map_err(|error| format!("Could not read DNS answers from {label}: {error}"))?;
    for list in addresses.values_mut() {
        list.retain(|ip| best_interface(*ip) == Some(adapter.index));
    }
    if addresses.values().all(Vec::is_empty) {
        return Err(format!("DNS on {label} returned no addresses routed through the adapter."));
    }
    BoundLookup::new(local, &addresses).map(Some).map_err(|error| error.to_string())
}

fn best_interface(ip: Ipv4Addr) -> Option<u32> {
    let mut index = 0;
    // GetBestInterface expects the destination in network byte order.
    let status = unsafe { GetBestInterface(u32::from_ne_bytes(ip.octets()), &mut index) };
    (status == 0).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(description: &str, is_up: bool) -> LeakInterface {
        LeakInterface {
            index: 1,
            name: "adapter".into(),
            description: description.into(),
            is_up,
            is_loopback: false,
            has_gateway: false,
            addrs: vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))],
        }
    }

    #[test]
    fn selects_active_supported_tunnel() {
        let openvpn = adapter("TAP-Windows Adapter V9 for OpenVPN Connect", true);
        let singbox = adapter("singbox_tun | sing-tun Tunnel", true);
        let ethernet = adapter("Ethernet", true);
        let disconnected = adapter("OpenVPN Data Channel Offload", false);
        assert_eq!(lookup_adapter(&[ethernet, disconnected, singbox.clone()]).unwrap(), Some(&singbox));
        assert_eq!(lookup_adapter(std::slice::from_ref(&openvpn)).unwrap(), Some(&openvpn));
        assert!(lookup_adapter(&[openvpn, singbox]).is_err());
        assert_eq!(lookup_adapter(&[adapter("WireGuard Tunnel", true)]).unwrap(), None);
    }
}
