//! One-shot commands: detect, apply, restore, status.

use crate::cli::{ApplyArgs, Cli, ProfileRequest};
use crate::lock::StateLock;
use crate::output::{self, emit};
use anyhow::{Context, Result};
use geoveil_core::geo::{self, GeoError, IpGeoProvider, ResolveOptions};
use geoveil_core::orchestrator::{Orchestrator, OrchestratorError, Report};
use geoveil_core::snapshot::SnapshotStore;
use geoveil_core::vpn::{is_vpn_interface, VpnDetector};
use geoveil_core::{NetInterface, Profile};
use geoveil_platform::{default_backends, interfaces, BackendOptions, SystemRunner};
use serde_json::json;
use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// Snapshot file name inside the state directory.
pub const STATE_FILE: &str = "state.json";

/// How a command finished (errors are reported separately via `Err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Everything succeeded.
    Ok,
    /// Some backends failed (exit code 2).
    Partial,
    /// The command failed and has already reported why (exit code 1).
    Failed,
}

impl Exit {
    /// `Ok` when the report is fully successful, `Partial` otherwise.
    pub fn from_report(report: &Report) -> Self {
        if report.all_ok() {
            Exit::Ok
        } else {
            Exit::Partial
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(e: Exit) -> Self {
        match e {
            Exit::Ok => ExitCode::SUCCESS,
            Exit::Partial => ExitCode::from(2),
            Exit::Failed => ExitCode::FAILURE,
        }
    }
}

/// Settings shared by every command, derived from global flags.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Emit JSON instead of human output.
    pub json: bool,
    /// Which optional backends are enabled.
    pub backend_opts: BackendOptions,
    /// Full path of the snapshot file.
    pub state_path: PathBuf,
    /// Options for IP-geolocation consensus.
    pub resolve_opts: ResolveOptions,
}

impl Ctx {
    /// Build the context from parsed CLI flags.
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let state_dir = match &cli.state_dir {
            Some(dir) => dir.clone(),
            None => crate::paths::default_state_dir()?,
        };
        let resolve_opts = ResolveOptions {
            timeout: Duration::from_secs(cli.provider_timeout_secs),
            ..ResolveOptions::default()
        };
        Ok(Self {
            json: cli.json,
            backend_opts: BackendOptions { locale: cli.locale, location: cli.location },
            state_path: state_dir.join(STATE_FILE),
            resolve_opts,
        })
    }

    /// Directory holding the snapshot (and lock) file.
    pub fn state_dir(&self) -> &Path {
        self.state_path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Exclusive lock on the state directory; hold it while changing settings.
    pub fn lock(&self) -> Result<StateLock> {
        StateLock::acquire(self.state_dir())
    }

    /// Orchestrator over the host OS backends, loading any saved state.
    pub fn orchestrator(&self) -> Result<Orchestrator> {
        let backends = default_backends(Arc::new(SystemRunner), &self.backend_opts);
        let store = SnapshotStore::new(self.state_path.clone());
        Orchestrator::new(backends, store)
            .with_context(|| format!("loading saved state from {}", self.state_path.display()))
    }
}

/// Build a profile for the given request (manual coords, explicit IP, or public IP).
pub async fn build_profile(
    request: &ProfileRequest,
    providers: &[Box<dyn IpGeoProvider>],
    opts: &ResolveOptions,
) -> Result<Profile> {
    match request {
        ProfileRequest::Manual { lat, lon, accuracy_m } => Ok(geo::manual_profile(*lat, *lon, *accuracy_m)),
        ProfileRequest::Ip(ip) => geo::resolve(providers, Some(*ip), opts)
            .await
            .with_context(|| format!("resolving location of {ip}")),
        ProfileRequest::PublicIp => profile_for_public_ip(
            geo::public_ip(opts.timeout),
            |ip| geo::resolve(providers, ip, opts),
        )
        .await
        .context("resolving location of the public (VPN exit) IP"),
    }
}

/// Resolve the public IP's location: find the IP first (IPv4 preferred,
/// deterministic), then have every provider geolocate that exact IP.
///
/// Letting each provider look up "my IP" instead could mix answers about
/// different addresses (a provider reached over IPv6 may see the user's real
/// address when the VPN only tunnels IPv4). If the IP cannot be found, fall
/// back to "my IP" lookups; [`geo::vote`] never pools answers about different
/// IPs, so that stays safe.
pub(crate) async fn profile_for_public_ip<R, Fut>(
    public_ip: impl Future<Output = Result<IpAddr, GeoError>>,
    resolve: R,
) -> Result<Profile, GeoError>
where
    R: Fn(Option<IpAddr>) -> Fut,
    Fut: Future<Output = Result<Profile, GeoError>>,
{
    match public_ip.await {
        Ok(ip) => resolve(Some(ip)).await,
        Err(e) => {
            tracing::warn!("could not determine the public IP ({e}); asking providers directly");
            resolve(None).await
        }
    }
}

/// Interfaces paired with their VPN-likeness.
fn classify(ifaces: Vec<NetInterface>) -> Vec<(NetInterface, bool)> {
    ifaces
        .into_iter()
        .map(|i| {
            let vpn = is_vpn_interface(&i);
            (i, vpn)
        })
        .collect()
}

/// List interfaces without blocking the async runtime.
pub async fn list_interfaces_async() -> Result<Vec<NetInterface>> {
    tokio::task::spawn_blocking(interfaces::list_interfaces)
        .await
        .context("interface enumeration task failed")
}

/// `geoveil detect`.
pub async fn detect(ctx: &Ctx) -> Result<Exit> {
    let ifaces = list_interfaces_async().await?;
    let vpn_names = VpnDetector::vpn_interfaces(&ifaces);
    if vpn_names.is_empty() {
        tracing::warn!("no VPN interface detected; the resolved location is probably your real one");
    }
    let classified = classify(ifaces);
    let providers = geo::default_providers();
    let resolved = build_profile(&ProfileRequest::PublicIp, &providers, &ctx.resolve_opts).await;

    let (profile, error) = match &resolved {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let interfaces_json: Vec<_> = classified
        .iter()
        .map(|(i, vpn)| json!({ "interface": i, "vpn": vpn }))
        .collect();
    let value = json!({
        "interfaces": interfaces_json,
        "vpn_interfaces": vpn_names,
        "profile": profile,
        "error": error,
    });
    emit(ctx.json, &value, || {
        let mut s = format!("interfaces:\n{}", output::format_interfaces(&classified));
        s.push_str(&format!("VPN interfaces: {}\n", output::format_names(&vpn_names)));
        if vpn_names.is_empty() {
            s.push_str("warning: no VPN interface detected\n");
        }
        if let Some(p) = profile {
            s.push_str("profile:\n");
            s.push_str(&output::format_profile(p));
        }
        s
    });
    match error {
        None => Ok(Exit::Ok),
        Some(e) => {
            // Already in the JSON output ("error"); humans also get it on stderr.
            eprintln!("error: {e}");
            Ok(Exit::Failed)
        }
    }
}

/// `geoveil apply`.
pub async fn apply(ctx: &Ctx, args: &ApplyArgs) -> Result<Exit> {
    let request = args.profile_request();
    let providers = geo::default_providers();
    let profile = build_profile(&request, &providers, &ctx.resolve_opts).await?;

    let _lock = ctx.lock()?;
    let mut orch = ctx.orchestrator()?;
    let report = orch.engage(&profile).context("engaging profile")?;
    let value = json!({ "profile": &profile, "report": &report });
    emit(ctx.json, &value, || {
        format!("applied profile:\n{}backends:\n{}", output::format_profile(&profile), output::format_report(&report))
    });
    Ok(Exit::from_report(&report))
}

/// `geoveil restore`.
pub async fn restore(ctx: &Ctx) -> Result<Exit> {
    let _lock = ctx.lock()?;
    let mut orch = ctx.orchestrator()?;
    match orch.disengage() {
        Ok(report) => {
            let exit = Exit::from_report(&report);
            emit(ctx.json, &json!({ "restored": exit == Exit::Ok, "report": &report }), || {
                let mut s = format!("restore:\n{}", output::format_report(&report));
                if exit != Exit::Ok {
                    s.push_str("some settings were not restored; the snapshot is kept, run `geoveil restore` again\n");
                }
                s
            });
            Ok(exit)
        }
        Err(OrchestratorError::NotEngaged) => {
            let message = format!("nothing to restore (no saved state in {})", ctx.state_dir().display());
            emit(ctx.json, &json!({ "restored": false, "report": null, "message": &message }), || message.clone());
            Ok(Exit::Ok)
        }
        Err(e) => Err(e).context("restoring original settings"),
    }
}

/// `geoveil status`.
pub async fn status(ctx: &Ctx) -> Result<Exit> {
    let orch = ctx.orchestrator()?;
    let engaged = orch.is_engaged();
    let profile = orch.current_profile().cloned();
    let verify = orch.verify();
    let ifaces = list_interfaces_async().await?;
    let vpn_names = VpnDetector::vpn_interfaces(&ifaces);

    let value = json!({
        "engaged": engaged,
        "profile": &profile,
        "verify": &verify,
        "vpn_interfaces": &vpn_names,
        "state_file": ctx.state_path.display().to_string(),
    });
    emit(ctx.json, &value, || {
        let mut s = format!("engaged: {}\n", if engaged { "yes" } else { "no" });
        if let Some(p) = &profile {
            s.push_str(&output::format_profile(p));
        }
        if !verify.is_empty() {
            s.push_str("verify:\n");
            s.push_str(&output::format_verify(&verify));
        }
        s.push_str(&format!("VPN interfaces: {}\n", output::format_names(&vpn_names)));
        s
    });
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use geoveil_core::orchestrator::BackendOutcome;
    use geoveil_core::ProfileSource;

    #[test]
    fn exit_from_report() {
        let ok = Report { outcomes: vec![BackendOutcome { backend: "timezone".into(), ok: true, error: None }] };
        assert_eq!(Exit::from_report(&ok), Exit::Ok);
        let bad = Report {
            outcomes: vec![BackendOutcome { backend: "locale".into(), ok: false, error: Some("x".into()) }],
        };
        assert_eq!(Exit::from_report(&bad), Exit::Partial);
    }

    #[test]
    fn ctx_uses_state_dir_and_timeout() {
        let cli = Cli::try_parse_from(["geoveil", "--state-dir", "st", "--provider-timeout-secs", "9", "--locale", "status"])
            .unwrap();
        let ctx = Ctx::from_cli(&cli).unwrap();
        assert_eq!(ctx.state_path, PathBuf::from("st").join(STATE_FILE));
        assert_eq!(ctx.resolve_opts.timeout, Duration::from_secs(9));
        assert!(ctx.backend_opts.locale);
        assert!(!ctx.backend_opts.location);
    }

    #[tokio::test]
    async fn manual_profile_needs_no_providers() {
        let req = ProfileRequest::Manual { lat: 48.8566, lon: 2.3522, accuracy_m: 30.0 };
        let p = build_profile(&req, &[], &ResolveOptions::default()).await.unwrap();
        assert_eq!(p.source, ProfileSource::Manual);
        assert_eq!(p.accuracy_m, 30.0);
        assert!((p.lat - 48.8566).abs() < 1e-9);
        assert!((p.lon - 2.3522).abs() < 1e-9);
    }

    fn manual() -> Profile {
        geo::manual_profile(52.52, 13.405, 100.0)
    }

    #[tokio::test]
    async fn public_ip_profile_resolves_that_exact_ip() {
        let seen = std::sync::Mutex::new(Vec::new());
        let ip: IpAddr = "185.65.135.230".parse().unwrap();
        let p = profile_for_public_ip(async move { Ok(ip) }, |asked| {
            seen.lock().unwrap().push(asked);
            async { Ok(manual()) }
        })
        .await
        .unwrap();
        assert_eq!(p, manual());
        assert_eq!(*seen.lock().unwrap(), vec![Some(ip)]);
    }

    #[tokio::test]
    async fn public_ip_failure_falls_back_to_provider_lookup() {
        let seen = std::sync::Mutex::new(Vec::new());
        let failed = async { Err(GeoError::Http { provider: "public-ip".into(), detail: "offline".into() }) };
        profile_for_public_ip(failed, |asked| {
            seen.lock().unwrap().push(asked);
            async { Ok(manual()) }
        })
        .await
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![None]);
    }

    #[test]
    fn state_dir_is_parent_of_state_file() {
        let cli = Cli::try_parse_from(["geoveil", "--state-dir", "st", "status"]).unwrap();
        let ctx = Ctx::from_cli(&cli).unwrap();
        assert_eq!(ctx.state_dir(), Path::new("st"));
    }

    #[test]
    fn lock_is_exclusive_per_state_dir() {
        let dir = crate::testutil::TempDir::new("ctxlock");
        let arg = dir.path().to_string_lossy().into_owned();
        let cli = Cli::try_parse_from(["geoveil", "--state-dir", arg.as_str(), "status"]).unwrap();
        let ctx = Ctx::from_cli(&cli).unwrap();
        let held = ctx.lock().unwrap();
        assert!(ctx.lock().is_err(), "a second apply/restore/run must be refused");
        drop(held);
        assert!(ctx.lock().is_ok());
    }

    #[tokio::test]
    async fn ip_request_without_providers_fails() {
        let req = ProfileRequest::Ip("203.0.113.9".parse().unwrap());
        assert!(build_profile(&req, &[], &ResolveOptions::default()).await.is_err());
    }
}
