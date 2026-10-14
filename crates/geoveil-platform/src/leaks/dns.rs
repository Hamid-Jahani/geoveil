//! Per-interface DNS resolver discovery, read-only.
//!
//! | OS | Command | Fallback |
//! |---|---|---|
//! | Windows | `powershell Get-DnsClientServerAddress` (as JSON) | none |
//! | Linux | `resolvectl status --no-pager` | `/etc/resolv.conf` (no interface info) |
//! | macOS | `scutil --dns` | none |
//!
//! Every discovery function is compiled on every OS and the parsers are pure,
//! so all of them are unit tested everywhere with realistic samples.

use crate::runner::CommandRunner;
use geoveil_core::leaks::{DnsDiscovery, DnsServer};
use serde_json::Value;
use std::io;
use std::net::IpAddr;
use std::path::Path;

/// PowerShell that lists every interface's DNS servers as compact JSON, with
/// non-ASCII characters (localized adapter names) escaped as `\uXXXX`.
pub const WINDOWS_SCRIPT: &str = concat!(
    r"$ErrorActionPreference = 'Stop'; ",
    r"$j = @(Get-DnsClientServerAddress | Where-Object { $_.ServerAddresses } | ForEach-Object { ",
    r"[pscustomobject]@{ alias = $_.InterfaceAlias; index = $_.InterfaceIndex; servers = @($_.ServerAddresses) } }) ",
    r"| ConvertTo-Json -Compress -Depth 3; ",
    r"[regex]::Replace([string]$j, '[^\x00-\x7F]', { param($c) '\u{0:x4}' -f [int][char]$c.Value })"
);

/// Default `resolv.conf` location.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";

/// Discovers resolvers for the host OS.
pub fn discover(runner: &dyn CommandRunner) -> DnsDiscovery {
    #[cfg(target_os = "windows")]
    {
        discover_windows(runner)
    }
    #[cfg(target_os = "macos")]
    {
        discover_macos(runner)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        discover_linux(runner, Path::new(RESOLV_CONF))
    }
}

/// Windows: `Get-DnsClientServerAddress`.
pub fn discover_windows(runner: &dyn CommandRunner) -> DnsDiscovery {
    let source = "Get-DnsClientServerAddress";
    let mut d = DnsDiscovery { source: source.to_string(), ..Default::default() };
    match runner.run("powershell", &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT]) {
        Ok(out) if out.success() => match parse_windows_json(&out.stdout) {
            Ok(servers) => d.servers = servers,
            Err(e) => d.error = Some(e),
        },
        Ok(out) => d.error = Some(format!("{source} failed: {}", detail(&out.stderr, &out.stdout, out.status))),
        Err(e) => d.error = Some(io_detail("powershell", &e)),
    }
    d
}

/// Linux: `resolvectl status`, falling back to `resolv_conf`.
pub fn discover_linux(runner: &dyn CommandRunner, resolv_conf: &Path) -> DnsDiscovery {
    let failure = match runner.run("resolvectl", &["status", "--no-pager"]) {
        Ok(out) if out.success() => {
            let servers = parse_resolvectl(&out.stdout);
            if !servers.is_empty() {
                return DnsDiscovery { source: "resolvectl".to_string(), servers, ..Default::default() };
            }
            "resolvectl listed no DNS servers".to_string()
        }
        Ok(out) => format!("resolvectl failed: {}", detail(&out.stderr, &out.stdout, out.status)),
        Err(e) => io_detail("resolvectl", &e),
    };
    let mut d = DnsDiscovery { source: resolv_conf.display().to_string(), ..Default::default() };
    d.notes.push(format!("{failure}; using {} (no per-interface information)", resolv_conf.display()));
    match std::fs::read_to_string(resolv_conf) {
        Ok(text) => {
            d.servers = parse_resolv_conf(&text);
            if !d.servers.is_empty() && d.servers.iter().all(|s| s.addr.is_loopback()) {
                d.notes.push("only a local stub resolver is listed; its upstream servers are unknown".to_string());
            }
        }
        Err(e) => d.error = Some(format!("{failure}; reading {}: {e}", resolv_conf.display())),
    }
    d
}

/// macOS: `scutil --dns`.
pub fn discover_macos(runner: &dyn CommandRunner) -> DnsDiscovery {
    let mut d = DnsDiscovery { source: "scutil --dns".to_string(), ..Default::default() };
    match runner.run("scutil", &["--dns"]) {
        Ok(out) if out.success() => d.servers = parse_scutil_dns(&out.stdout),
        Ok(out) => d.error = Some(format!("scutil --dns failed: {}", detail(&out.stderr, &out.stdout, out.status))),
        Err(e) => d.error = Some(io_detail("scutil", &e)),
    }
    d
}

fn detail(stderr: &str, stdout: &str, status: i32) -> String {
    let text = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    if text.is_empty() {
        format!("exit status {status}")
    } else {
        format!("exit status {status}: {}", text.lines().next().unwrap_or(text))
    }
}

fn io_detail(program: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => format!("`{program}` not found"),
        _ => format!("cannot run `{program}`: {e}"),
    }
}

/// Parses an address token: strips a `%zone`, a `#tls-name` suffix and
/// brackets. Returns `None` for anything that is not an IP address.
pub fn parse_addr(token: &str) -> Option<IpAddr> {
    let t = token.trim().trim_start_matches('[');
    let t = t.split('#').next().unwrap_or(t);
    let t = t.split('%').next().unwrap_or(t);
    let t = t.trim_end_matches(']');
    t.parse().ok()
}

fn push_unique(out: &mut Vec<DnsServer>, server: DnsServer) {
    if !out.iter().any(|s| s.addr == server.addr && s.if_index == server.if_index && s.interface == server.interface) {
        out.push(server);
    }
}

/// Windows lists these deprecated site-local addresses on every adapter that
/// has no IPv6 DNS server configured. They are placeholders, not resolvers.
const WINDOWS_PLACEHOLDER_DNS: [&str; 3] = ["fec0:0:0:ffff::1", "fec0:0:0:ffff::2", "fec0:0:0:ffff::3"];

fn is_windows_placeholder(addr: &IpAddr) -> bool {
    WINDOWS_PLACEHOLDER_DNS.iter().any(|p| p.parse::<IpAddr>().ok() == Some(*addr))
}

/// Parses the JSON printed by [`WINDOWS_SCRIPT`]: an array (or, from older
/// PowerShell, a single object) of `{alias, index, servers}`. `servers` may
/// be an array, a string, or PowerShell 5.1's `{"value": [...]}` wrapper.
/// Windows' `fec0:0:0:ffff::1..3` placeholders are dropped.
pub fn parse_windows_json(text: &str) -> Result<Vec<DnsServer>, String> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("unexpected Get-DnsClientServerAddress output: {e}"))?;
    let entries: Vec<&Value> = match &value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![&value],
        _ => return Err("unexpected Get-DnsClientServerAddress output: not an object".to_string()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let alias = entry.get("alias").and_then(Value::as_str).map(str::to_string);
        let index = entry.get("index").and_then(Value::as_u64).and_then(|i| u32::try_from(i).ok());
        let addrs = json_strings(entry.get("servers"));
        for addr in addrs.iter().filter_map(|s| parse_addr(s)).filter(|a| !is_windows_placeholder(a)) {
            push_unique(&mut out, DnsServer { interface: alias.clone(), if_index: index, addr });
        }
    }
    Ok(out)
}

fn json_strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::Object(map)) => json_strings(map.get("value")),
        _ => Vec::new(),
    }
}

/// Parses `resolvectl status`. The `Global` section yields servers without an
/// interface; `Link N (name)` sections yield servers on that link.
/// "Fallback DNS Servers" are ignored (used only when nothing else is set).
pub fn parse_resolvectl(text: &str) -> Vec<DnsServer> {
    let mut out = Vec::new();
    let mut link: Option<(Option<u32>, String)> = None;
    let mut in_servers = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_servers = false;
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            in_servers = false;
            if trimmed == "Global" {
                link = None;
            } else if let Some(rest) = trimmed.strip_prefix("Link ") {
                // "Link 2 (enp3s0)"
                let (num, name) = match rest.split_once(' ') {
                    Some((n, name)) => (n.parse().ok(), name.trim().trim_start_matches('(').trim_end_matches(')')),
                    None => (rest.parse().ok(), ""),
                };
                link = Some((num, name.to_string()));
            }
            continue;
        }
        let values = match trimmed.split_once(": ") {
            Some((key, value)) => {
                in_servers = key.trim() == "DNS Servers";
                value
            }
            None if trimmed.ends_with(':') => {
                in_servers = trimmed.trim_end_matches(':').trim() == "DNS Servers";
                ""
            }
            // A continuation line of the previous key.
            None => trimmed,
        };
        if !in_servers {
            continue;
        }
        for addr in values.split_whitespace().filter_map(parse_addr) {
            let (if_index, interface) = match &link {
                Some((n, name)) => (*n, Some(name.clone()).filter(|s| !s.is_empty())),
                None => (None, None),
            };
            push_unique(&mut out, DnsServer { interface, if_index, addr });
        }
    }
    out
}

/// Parses `/etc/resolv.conf` `nameserver` lines.
pub fn parse_resolv_conf(text: &str) -> Vec<DnsServer> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        if parts.next() == Some("nameserver") {
            if let Some(addr) = parts.next().and_then(parse_addr) {
                push_unique(&mut out, DnsServer { interface: None, if_index: None, addr });
            }
        }
    }
    out
}

/// Parses `scutil --dns`: only the default section. The "for scoped
/// queries" section is skipped, because macOS uses those resolvers only for
/// lookups an app explicitly ties to that interface, not for normal traffic;
/// with a VPN up, `en0`'s router resolver always sits there and would
/// otherwise look like a leak. Resolvers without an `if_index` are reported
/// without interface.
pub fn parse_scutil_dns(text: &str) -> Vec<DnsServer> {
    #[derive(Default)]
    struct Block {
        servers: Vec<IpAddr>,
        index: Option<u32>,
        name: Option<String>,
    }
    fn flush(block: &mut Block, out: &mut Vec<DnsServer>) {
        for addr in block.servers.drain(..) {
            push_unique(out, DnsServer { interface: block.name.clone(), if_index: block.index, addr });
        }
        *block = Block::default();
    }

    let mut out = Vec::new();
    let mut block = Block::default();
    let mut scoped = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("resolver #") || trimmed.starts_with("DNS configuration") {
            flush(&mut block, &mut out);
            if trimmed.starts_with("DNS configuration") {
                scoped = trimmed.contains("scoped");
            }
            continue;
        }
        if scoped {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(" : ").or_else(|| trimmed.split_once(':')) else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.starts_with("nameserver[") {
            if let Some(addr) = parse_addr(value) {
                block.servers.push(addr);
            }
        } else if key == "if_index" {
            // "6 (en0)"
            let mut parts = value.split_whitespace();
            block.index = parts.next().and_then(|n| n.parse().ok());
            block.name = parts.next().map(|n| n.trim_start_matches('(').trim_end_matches(')').to_string());
        }
    }
    flush(&mut block, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn srv(name: Option<&str>, idx: Option<u32>, addr: &str) -> DnsServer {
        DnsServer { interface: name.map(String::from), if_index: idx, addr: ip(addr) }
    }

    const WINDOWS_SAMPLE: &str = r#"[{"alias":"Ethernet","index":4,"servers":["192.168.1.1"]},{"alias":"Wi-Fi","index":12,"servers":["8.8.8.8","1.1.1.1"]},{"alias":"WireGuard من","index":17,"servers":["10.64.0.1"]},{"alias":"Ethernet","index":4,"servers":["fe80::1%4","fec0:0:0:ffff::1%1"]}]"#;

    #[test]
    fn windows_json_array() {
        let s = parse_windows_json(WINDOWS_SAMPLE).unwrap();
        assert_eq!(
            s,
            vec![
                srv(Some("Ethernet"), Some(4), "192.168.1.1"),
                srv(Some("Wi-Fi"), Some(12), "8.8.8.8"),
                srv(Some("Wi-Fi"), Some(12), "1.1.1.1"),
                srv(Some("WireGuard من"), Some(17), "10.64.0.1"),
                srv(Some("Ethernet"), Some(4), "fe80::1"),
            ]
        );
    }

    #[test]
    fn windows_placeholders_dropped() {
        let s = parse_windows_json(
            r#"[{"alias":"vEthernet (Default Switch)","index":30,"servers":["fec0:0:0:ffff::1","fec0:0:0:ffff::2","fec0:0:0:ffff::3"]}]"#,
        )
        .unwrap();
        assert_eq!(s, vec![]);
    }

    #[test]
    fn windows_json_single_object_and_wrappers() {
        let s = parse_windows_json(r#"{"alias":"Ethernet","index":4,"servers":"192.168.1.1"}"#).unwrap();
        assert_eq!(s, vec![srv(Some("Ethernet"), Some(4), "192.168.1.1")]);
        let s = parse_windows_json(r#"{"alias":"Ethernet","index":4,"servers":{"value":["9.9.9.9"],"Count":1}}"#).unwrap();
        assert_eq!(s, vec![srv(Some("Ethernet"), Some(4), "9.9.9.9")]);
        assert_eq!(parse_windows_json("\n").unwrap(), vec![]);
        assert!(parse_windows_json("Get-DnsClientServerAddress : not recognized").is_err());
    }

    #[test]
    fn windows_discovery_runs_exact_command() {
        let fake = FakeRunner::new();
        fake.push_stdout(WINDOWS_SAMPLE);
        let d = discover_windows(&fake);
        assert_eq!(d.servers.len(), 5);
        assert!(d.error.is_none());
        let calls = fake.raw_calls();
        assert_eq!(calls[0].0, "powershell");
        assert_eq!(calls[0].1, vec!["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT]);
        assert!(WINDOWS_SCRIPT.contains("Get-DnsClientServerAddress"));
    }

    #[test]
    fn windows_discovery_errors() {
        let fake = FakeRunner::new();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "boom\nmore".into() });
        let d = discover_windows(&fake);
        assert_eq!(d.error.as_deref(), Some("Get-DnsClientServerAddress failed: exit status 1: boom"));
        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        assert_eq!(discover_windows(&fake).error.as_deref(), Some("`powershell` not found"));
    }

    const RESOLVECTL_SAMPLE: &str = "\
Global
           Protocols: -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
    resolv.conf mode: stub
  Current DNS Server: 1.1.1.1#cloudflare-dns.com
         DNS Servers: 1.1.1.1#cloudflare-dns.com 9.9.9.9
Fallback DNS Servers: 8.8.8.8 2001:4860:4860::8888

Link 2 (enp3s0)
    Current Scopes: DNS
         Protocols: +DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
Current DNS Server: 192.168.1.1
       DNS Servers: 192.168.1.1
                    fe80::1%enp3s0
        DNS Domain: lan

Link 3 (wlp2s0)
    Current Scopes: none
         Protocols: -DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported

Link 7 (wg0)
    Current Scopes: DNS
         Protocols: +DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
Current DNS Server: 10.64.0.1
       DNS Servers: 10.64.0.1
        DNS Domain: ~.
";

    #[test]
    fn resolvectl_sections() {
        assert_eq!(
            parse_resolvectl(RESOLVECTL_SAMPLE),
            vec![
                srv(None, None, "1.1.1.1"),
                srv(None, None, "9.9.9.9"),
                srv(Some("enp3s0"), Some(2), "192.168.1.1"),
                srv(Some("enp3s0"), Some(2), "fe80::1"),
                srv(Some("wg0"), Some(7), "10.64.0.1"),
            ]
        );
    }

    #[test]
    fn linux_uses_resolvectl_when_available() {
        let fake = FakeRunner::new();
        fake.push_stdout(RESOLVECTL_SAMPLE);
        let d = discover_linux(&fake, Path::new("/nonexistent/resolv.conf"));
        assert_eq!(d.source, "resolvectl");
        assert_eq!(d.servers.len(), 5);
        assert_eq!(fake.calls(), vec!["resolvectl status --no-pager"]);
    }

    #[test]
    fn linux_falls_back_to_resolv_conf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, "# generated by NetworkManager\nsearch lan\nnameserver 192.168.1.1 # router\nnameserver 2a01:4f8::53\noptions edns0\n").unwrap();
        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        let d = discover_linux(&fake, &path);
        assert_eq!(d.servers, vec![srv(None, None, "192.168.1.1"), srv(None, None, "2a01:4f8::53")]);
        assert!(d.notes[0].starts_with("`resolvectl` not found"));
        assert!(d.error.is_none());
    }

    #[test]
    fn linux_stub_only_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, "nameserver 127.0.0.53\noptions edns0 trust-ad\n").unwrap();
        let fake = FakeRunner::new();
        fake.push(CommandOutput { status: 1, stdout: String::new(), stderr: "Failed to get global data".into() });
        let d = discover_linux(&fake, &path);
        assert_eq!(d.servers, vec![srv(None, None, "127.0.0.53")]);
        assert!(d.notes.iter().any(|n| n.contains("stub")));

        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        let d = discover_linux(&fake, &dir.path().join("missing"));
        assert!(d.error.is_some());
    }

    const SCUTIL_SAMPLE: &str = "\
DNS configuration

resolver #1
  search domain[0] : lan
  nameserver[0] : 10.64.0.1
  if_index : 21 (utun4)
  flags    : Request A records
  reach    : 0x00000003 (Reachable,Transient Connection)

resolver #2
  domain   : local
  options  : mdns
  timeout  : 5
  flags    : Request A records, Request AAAA records
  reach    : 0x00000000 (Not Reachable)
  order    : 300000

DNS configuration (for scoped queries)

resolver #1
  search domain[0] : lan
  nameserver[0] : 192.168.1.1
  nameserver[1] : fe80::1%en0
  if_index : 6 (en0)
  flags    : Scoped, Request A records
  reach    : 0x00020002 (Reachable,Directly Reachable Address)

resolver #2
  nameserver[0] : 10.64.0.1
  if_index : 21 (utun4)
  flags    : Scoped, Request A records
  reach    : 0x00000003 (Reachable,Transient Connection)
";

    #[test]
    fn scutil_skips_scoped_section() {
        // en0's router resolver appears only "for scoped queries" while the
        // VPN owns the default resolver: it must not be reported.
        assert_eq!(parse_scutil_dns(SCUTIL_SAMPLE), vec![srv(Some("utun4"), Some(21), "10.64.0.1")]);
        // Without a VPN the default section carries en0's resolver.
        let no_vpn = "\
DNS configuration

resolver #1
  nameserver[0] : 192.168.1.1
  if_index : 6 (en0)

DNS configuration (for scoped queries)

resolver #1
  nameserver[0] : 192.168.1.1
  if_index : 6 (en0)
  flags    : Scoped, Request A records
";
        assert_eq!(parse_scutil_dns(no_vpn), vec![srv(Some("en0"), Some(6), "192.168.1.1")]);
    }

    #[test]
    fn macos_discovery() {
        let fake = FakeRunner::new();
        fake.push_stdout(SCUTIL_SAMPLE);
        let d = discover_macos(&fake);
        assert_eq!(d.servers.len(), 1);
        assert_eq!(fake.calls(), vec!["scutil --dns"]);
        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        assert_eq!(discover_macos(&fake).error.as_deref(), Some("`scutil` not found"));
    }

    #[test]
    fn address_tokens() {
        assert_eq!(parse_addr("1.1.1.1#cloudflare-dns.com"), Some(ip("1.1.1.1")));
        assert_eq!(parse_addr("fe80::1%en0"), Some(ip("fe80::1")));
        assert_eq!(parse_addr("[2001:db8::1]"), Some(ip("2001:db8::1")));
        assert_eq!(parse_addr("lan"), None);
    }
}
