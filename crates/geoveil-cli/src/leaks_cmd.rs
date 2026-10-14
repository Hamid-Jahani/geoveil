//! `geoveil leaks`: tell the user whether their VPN leaks. Changes nothing.
//!
//! Gathers observations concurrently (interfaces, public exit IP, an IPv6-only
//! endpoint, interface-bound STUN probes, configured DNS resolvers, the
//! optional active DNS test, and the orchestrator's verify results while a
//! profile is engaged), then hands them to `geoveil_core::leaks::evaluate`.
//!
//! Exit codes: 0 no leak found, 3 leak(s) found, 1 a check could not run.

use crate::commands::{Ctx, Exit};
use crate::output::emit;
use anyhow::{Context, Result};
use clap::Args;
use geoveil_core::geo;
use geoveil_core::leaks::{
    dnstest, evaluate, net, CheckResult, ExitProbe, LeakInputs, LeakOutcome, LeakReport, Verdict,
};
use geoveil_core::VerifyReport;
use geoveil_platform::leaks::{dns, list_leak_interfaces, select_targets, stun};
use geoveil_platform::SystemRunner;

/// Hosts contacted by `geo::public_ip` (kept in sync with its endpoint list).
const PUBLIC_IP_HOSTS: [&str; 3] = ["api.ipify.org", "icanhazip.com", "ifconfig.me"];

/// Arguments for `leaks`.
#[derive(Debug, Clone, Args)]
pub struct LeaksArgs {
    /// Also run a unique-subdomain DNS leak test. Contacts am.i.mullvad.net
    /// and <random>.dnsleak.am.i.mullvad.net, which learn your exit IP and
    /// your resolvers.
    #[arg(long)]
    pub active_dns: bool,

    /// STUN server to probe, HOST:PORT (repeatable). Default:
    /// stun.l.google.com:19302 and stun.cloudflare.com:3478.
    #[arg(long = "stun", value_name = "HOST:PORT", value_parser = parse_stun_server)]
    pub stun: Vec<String>,

    /// Skip the STUN (WebRTC) probes.
    #[arg(long, conflicts_with = "stun")]
    pub no_stun: bool,

    /// Skip the IPv6 probe.
    #[arg(long)]
    pub no_ipv6: bool,
}

impl LeaksArgs {
    /// STUN servers to use: the user's, or the defaults.
    pub fn stun_servers(&self) -> Vec<String> {
        if self.stun.is_empty() {
            stun::DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect()
        } else {
            self.stun.clone()
        }
    }
}

/// Validates `HOST:PORT` (port 1..=65535, non-empty host, IPv6 in brackets).
fn parse_stun_server(s: &str) -> Result<String, String> {
    let s = s.trim();
    let (host, port) = s.rsplit_once(':').ok_or_else(|| format!("`{s}` is not HOST:PORT"))?;
    let port: u16 = port.parse().map_err(|_| format!("`{port}` is not a port number"))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if port == 0 || bare.is_empty() || (host.contains(':') && !host.starts_with('[')) {
        return Err(format!("`{s}` is not HOST:PORT (write IPv6 addresses as [addr]:port)"));
    }
    Ok(s.to_string())
}

/// Hosts the run contacts, for the report.
pub fn third_parties(args: &LeaksArgs) -> Vec<String> {
    let mut out: Vec<String> = PUBLIC_IP_HOSTS.iter().map(|h| h.to_string()).collect();
    if !args.no_ipv6 {
        out.extend(net::IPV6_ENDPOINTS.iter().map(|u| u.trim_start_matches("https://").to_string()));
    }
    if !args.no_stun {
        out.extend(args.stun_servers());
    }
    if args.active_dns {
        out.extend(dnstest::contacted_hosts());
    }
    out
}

/// Maps the report outcome to the process exit.
pub fn exit_for(outcome: LeakOutcome) -> Exit {
    match outcome {
        LeakOutcome::Clean => Exit::Ok,
        LeakOutcome::LeaksFound => Exit::Leaks,
        LeakOutcome::Incomplete => Exit::Failed,
    }
}

/// `geoveil leaks`.
pub async fn leaks(ctx: &Ctx, args: &LeaksArgs) -> Result<Exit> {
    let timeout = ctx.resolve_opts.timeout;
    let interfaces = tokio::task::spawn_blocking(list_leak_interfaces)
        .await
        .context("interface enumeration task failed")?;

    let targets = select_targets(&interfaces);
    let servers = args.stun_servers();
    let no_stun = args.no_stun;
    let stun_task = tokio::task::spawn_blocking(move || {
        (!no_stun).then(|| stun::probe_all(&targets, &servers, &stun::DEFAULT_SCHEDULE))
    });
    let dns_task = tokio::task::spawn_blocking(|| dns::discover(&SystemRunner));
    let verify_task = {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || engaged_verify(&ctx))
    };
    let exit_fut = async {
        match geo::public_ip(timeout).await {
            Ok(ip) => ExitProbe::Found { ip },
            Err(e) => ExitProbe::Failed { error: e.to_string() },
        }
    };
    let v6_fut = async {
        if args.no_ipv6 {
            ExitProbe::NotRun
        } else {
            net::ipv6_exit(timeout).await
        }
    };
    let active_fut = async {
        if args.active_dns {
            Some(dnstest::run(timeout, dnstest::DEFAULT_ROUNDS).await)
        } else {
            None
        }
    };
    let (exit, exit_v6, active_dns, stun, dns, verify) =
        tokio::join!(exit_fut, v6_fut, active_fut, stun_task, dns_task, verify_task);

    let inputs = LeakInputs {
        interfaces,
        exit,
        exit_v6,
        stun: stun.context("STUN probe task failed")?,
        dns: dns.context("DNS discovery task failed")?,
        active_dns,
        verify: verify.context("verify task failed")?,
        third_parties: third_parties(args),
    };
    let report = evaluate(inputs);
    emit(ctx.json, &report, || format_report(&report));
    Ok(exit_for(report.outcome))
}

/// Verify results when a profile is engaged; `None` otherwise or when the
/// saved state cannot be read (logged).
fn engaged_verify(ctx: &Ctx) -> Option<Vec<VerifyReport>> {
    match ctx.orchestrator() {
        Ok(orch) if orch.is_engaged() => Some(orch.verify()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("cannot read saved state, skipping verify: {e:#}");
            None
        }
    }
}

/// Column where summaries start in the table.
const SUMMARY_COL: usize = 19;

/// Human-readable report.
pub fn format_report(report: &LeakReport) -> String {
    let mut s = String::new();
    let vpn = if report.vpn_interfaces.is_empty() { "none".to_string() } else { report.vpn_interfaces.join(", ") };
    let ip = |ip: Option<std::net::IpAddr>| ip.map(|i| i.to_string()).unwrap_or_else(|| "unknown".to_string());
    s.push_str(&format!("VPN interfaces: {vpn}\nexit IP: {}\n", ip(report.exit_ip)));
    if let Some(v6) = report.exit_ipv6 {
        s.push_str(&format!("IPv6 exit: {v6}\n"));
    }
    s.push('\n');
    s.push_str(&format!("{:<12}{:<7}SUMMARY\n", "CHECK", "RESULT"));
    for check in &report.checks {
        s.push_str(&format_check(check));
    }
    if !report.inputs.third_parties.is_empty() {
        s.push_str(&format!("\nthird parties contacted: {}\n", report.inputs.third_parties.join(", ")));
    }
    s.push_str(match report.outcome {
        LeakOutcome::Clean => "result: no leaks found\n",
        LeakOutcome::LeaksFound => "result: LEAKS FOUND\n",
        LeakOutcome::Incomplete => "result: no leaks found, but some checks could not run\n",
    });
    s
}

fn format_check(c: &CheckResult) -> String {
    let pad = " ".repeat(SUMMARY_COL);
    let mut s = format!("{:<12}{:<7}{}\n", c.id.name(), c.verdict.label(), c.summary);
    // Details matter most when something is wrong; keep clean checks short.
    let show_details = c.verdict >= Verdict::Warn || c.verdict == Verdict::Info;
    if show_details {
        for d in &c.details {
            s.push_str(&format!("{pad}- {d}\n"));
        }
    }
    if let Some(a) = &c.advice {
        s.push_str(&format!("{pad}advice: {a}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use geoveil_core::leaks::{CheckId, DnsDiscovery, LeakInterface, StunOutcome, StunProbe};

    fn parse(args: &[&str]) -> Result<LeaksArgs, clap::Error> {
        let cli = Cli::try_parse_from(std::iter::once("geoveil").chain(args.iter().copied()))?;
        match cli.command {
            Command::Leaks(a) => Ok(a),
            other => panic!("expected leaks, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let a = parse(&["leaks"]).unwrap();
        assert!(!a.active_dns && !a.no_stun && !a.no_ipv6);
        assert_eq!(a.stun_servers(), vec!["stun.l.google.com:19302", "stun.cloudflare.com:3478"]);
        let tp = third_parties(&a);
        assert!(tp.contains(&"api6.ipify.org".to_string()));
        assert!(tp.contains(&"stun.cloudflare.com:3478".to_string()));
        assert!(!tp.iter().any(|h| h.contains("mullvad")), "active test is opt-in");
    }

    #[test]
    fn flags() {
        let a = parse(&["leaks", "--active-dns", "--json", "--stun", "stun.example.org:3478", "--stun", "[2001:db8::1]:3478"])
            .unwrap();
        assert!(a.active_dns);
        assert_eq!(a.stun_servers(), vec!["stun.example.org:3478", "[2001:db8::1]:3478"]);
        assert!(third_parties(&a).iter().any(|h| h.contains("mullvad")));
        let a = parse(&["leaks", "--no-stun", "--no-ipv6"]).unwrap();
        assert!(!third_parties(&a).iter().any(|h| h.contains("stun") || h.contains("api6")));
    }

    #[test]
    fn bad_stun_values_rejected() {
        for bad in ["stun.example.org", "host:0", "host:99999", ":3478", "2001:db8::1:3478"] {
            assert!(parse(&["leaks", "--stun", bad]).is_err(), "{bad}");
        }
        assert!(parse(&["leaks", "--no-stun", "--stun", "a:1"]).is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_for(LeakOutcome::Clean), Exit::Ok);
        assert_eq!(exit_for(LeakOutcome::LeaksFound), Exit::Leaks);
        assert_eq!(exit_for(LeakOutcome::Incomplete), Exit::Failed);
        assert_eq!(std::process::ExitCode::from(Exit::Leaks), std::process::ExitCode::from(3));
    }

    fn sample_report() -> LeakReport {
        let eth = LeakInterface {
            index: 4,
            name: "{E}".into(),
            description: "Ethernet".into(),
            is_up: true,
            is_loopback: false,
            has_gateway: true,
            addrs: vec!["192.168.1.118".parse().unwrap()],
        };
        let wg = LeakInterface {
            index: 17,
            name: "wg0".into(),
            description: String::new(),
            is_up: true,
            is_loopback: false,
            has_gateway: false,
            addrs: vec!["10.64.0.2".parse().unwrap()],
        };
        evaluate(LeakInputs {
            interfaces: vec![eth, wg],
            exit: ExitProbe::Found { ip: "185.65.135.230".parse().unwrap() },
            exit_v6: ExitProbe::Failed { error: "no route".into() },
            stun: Some(vec![StunProbe {
                server: "stun.l.google.com:19302".into(),
                server_addr: None,
                interface: Some("Ethernet".into()),
                local: Some("192.168.1.118".parse().unwrap()),
                outcome: StunOutcome::Mapped { addr: "82.152.98.44:51000".parse().unwrap() },
            }]),
            dns: DnsDiscovery { source: "test".into(), ..Default::default() },
            active_dns: None,
            verify: None,
            third_parties: vec!["api.ipify.org".into()],
        })
    }

    #[test]
    fn human_table() {
        let r = sample_report();
        assert_eq!(r.checks.iter().find(|c| c.id == CheckId::Webrtc).unwrap().verdict, Verdict::Leak);
        let s = format_report(&r);
        assert!(s.starts_with("VPN interfaces: wg0\nexit IP: 185.65.135.230\n"), "{s}");
        assert!(s.contains("CHECK       RESULT SUMMARY\n"));
        assert!(s.contains("vpn         PASS   VPN interface(s) wg0 up"));
        assert!(s.contains("webrtc      LEAK   STUN outside the tunnel reveals 82.152.98.44"));
        assert!(s.contains("                   - Ethernet (192.168.1.118) -> stun.l.google.com:19302: sees 82.152.98.44"));
        assert!(s.contains("                   advice: Browsers using WebRTC"));
        assert!(s.contains("third parties contacted: api.ipify.org\n"));
        assert!(s.ends_with("result: LEAKS FOUND\n"));
    }

    #[test]
    fn json_has_outcome_and_checks() {
        let v = serde_json::to_value(sample_report()).unwrap();
        assert_eq!(v["outcome"], "leaks_found");
        assert!(v["checks"].as_array().unwrap().iter().any(|c| c["id"] == "webrtc" && c["verdict"] == "leak"));
    }
}
