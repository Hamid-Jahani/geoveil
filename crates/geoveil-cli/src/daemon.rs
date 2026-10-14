//! `geoveil run`: follow the VPN automatically.
//!
//! The loop polls interfaces, asks the [`VpnDetector`] whether a public-IP
//! check is due, and reconciles the system with the detected exit IP:
//! - VPN up: engage the profile for the exit IP until every backend applied
//!   (failures are retried with exponential backoff; the resolved profile is
//!   cached so retries do not hit the geolocation providers again);
//! - VPN down (and no VPN interface present): restore the original settings,
//!   retrying with backoff until the restore fully succeeds.
//!
//! Orchestrator calls spawn OS processes, so they run on the blocking pool
//! behind an `Arc<std::sync::Mutex<_>>`; the lock is never held across `.await`.
//! The daemon holds the state-directory lock for its whole life, and stops
//! (restoring originals unless `--sticky`) on Ctrl-C, SIGTERM/SIGHUP, or the
//! Windows console close / logoff / shutdown events, even mid-poll.
//!
//! The daemon also serves local IPC ([`crate::ipc`]): it publishes its status
//! after every poll and runs `restore_now` / `pause` / `resume` / `shutdown`
//! between polls. While paused it keeps watching the VPN (for the status)
//! but neither engages nor restores.

use crate::cli::RunArgs;
use crate::commands::{list_interfaces_async, Ctx, Exit};
use crate::ipc::{self, Control, ControlRequest, StatusParts};
use geoveil_ipc::protocol::{DaemonStatus, ErrorBody, ErrorKind};
use anyhow::{anyhow, Context, Result};
use geoveil_core::geo::{self, IpGeoProvider, ResolveOptions};
use geoveil_core::orchestrator::{Orchestrator, OrchestratorError, Report};
use geoveil_core::vpn::{DetectorConfig, VpnDetector};
use geoveil_core::{Profile, VpnEvent};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::MissedTickBehavior;

/// First delay before retrying a failed engage or restore.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Longest delay between retries.
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(600);

type SharedOrch = Arc<Mutex<Orchestrator>>;

/// Run `f` with exclusive access to the orchestrator on the blocking pool.
async fn with_orch<T, F>(orch: &SharedOrch, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Orchestrator) -> T + Send + 'static,
{
    let orch = Arc::clone(orch);
    tokio::task::spawn_blocking(move || {
        // A panic in a previous call poisons the mutex; the state on disk is
        // still authoritative, so keep going with the inner value.
        let mut guard = orch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    })
    .await
    .context("orchestrator task panicked")
}

/// Decide whether to (re)engage now. Returns the exit IP to engage for.
///
/// - nothing to do when the VPN is down or the exit IP is already applied;
/// - after a failure, wait until `retry_at` before trying again.
pub fn engage_target(
    desired: Option<IpAddr>,
    applied: Option<IpAddr>,
    retry_at: Option<Instant>,
    now: Instant,
) -> Option<IpAddr> {
    let ip = desired?;
    if applied == Some(ip) {
        return None;
    }
    match retry_at {
        Some(at) if now < at => None,
        _ => Some(ip),
    }
}

/// Delay before retry number `attempt` (1-based): [`RETRY_BACKOFF`] doubled
/// per attempt, capped at [`MAX_RETRY_BACKOFF`].
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    RETRY_BACKOFF.saturating_mul(1 << doublings).min(MAX_RETRY_BACKOFF)
}

/// Whether to (re)try restoring the original settings now: only when no
/// profile should be applied, the user did not ask to keep it (`sticky`), no
/// VPN interface is present (a VPN that is up but not yet confirmed keeps the
/// current state), and any backoff has expired.
pub fn should_restore(
    vpn_confirmed_up: bool,
    vpn_interface_present: bool,
    sticky: bool,
    retry_at: Option<Instant>,
    now: Instant,
) -> bool {
    !vpn_confirmed_up && !vpn_interface_present && !sticky && retry_at.is_none_or(|at| now >= at)
}

/// How a restore attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreStatus {
    /// Every saved setting was restored and the snapshot cleared.
    Restored,
    /// No saved state.
    NothingToRestore,
    /// Some backends failed; the snapshot is kept for a retry.
    Partial,
    /// The restore could not run (snapshot store error, task panic).
    Error,
}

/// A pending retry: which attempt failed last and when to try again.
#[derive(Debug, Clone, Copy)]
struct Retry {
    ip: IpAddr,
    at: Instant,
    attempt: u32,
}

/// Mutable daemon state.
struct Daemon {
    orch: SharedOrch,
    detector: VpnDetector,
    providers: Vec<Box<dyn IpGeoProvider>>,
    resolve_opts: ResolveOptions,
    sticky: bool,
    /// Exit IP whose profile is fully applied (every backend succeeded).
    applied: Option<IpAddr>,
    /// Last failed engage attempt and when to retry it.
    retry: Option<Retry>,
    /// Profile resolved for an exit IP, reused by engage retries.
    resolved: Option<(IpAddr, Profile)>,
    /// When to retry a failed restore, and how many attempts failed so far.
    restore_retry: Option<(Instant, u32)>,
    /// Following the VPN is paused (IPC `pause` / `restore_now`).
    paused: bool,
    /// Last engage / restore error, for the IPC status.
    last_error: Option<String>,
    /// VPN-like interfaces seen at the last poll.
    vpn_interfaces: Vec<String>,
    /// State directory, for the IPC status.
    state_dir: String,
    /// Where the IPC server reads the status from.
    status_tx: tokio::sync::watch::Sender<DaemonStatus>,
}

/// What the loop does after an IPC command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterControl {
    /// Keep the normal poll schedule.
    Continue,
    /// Poll right away (after `resume`).
    PollNow,
    /// Stop the daemon.
    Stop(&'static str),
}

impl Daemon {
    /// Publishes the current status for IPC clients and returns it.
    async fn publish(&self) -> DaemonStatus {
        let (engaged, profile) = with_orch(&self.orch, |o| (o.is_engaged(), o.current_profile().cloned()))
            .await
            .unwrap_or((false, None));
        let status = ipc::build_status(StatusParts {
            engaged,
            paused: self.paused,
            sticky: self.sticky,
            profile,
            vpn_up: self.detector.is_up(),
            exit_ip: if self.detector.is_up() { self.detector.exit_ip() } else { None },
            vpn_interfaces: self.vpn_interfaces.clone(),
            last_error: self.last_error.clone(),
            state_dir: self.state_dir.clone(),
        });
        self.status_tx.send_replace(status.clone());
        status
    }

    /// Runs one IPC command and answers it with the resulting status.
    async fn control(&mut self, req: ControlRequest) -> AfterControl {
        let mut after = AfterControl::Continue;
        let result: Result<(), String> = match req.control {
            Control::Pause => {
                tracing::info!("IPC: pausing; the VPN is still watched but settings are left as they are");
                self.paused = true;
                Ok(())
            }
            Control::Resume => {
                tracing::info!("IPC: resuming");
                self.paused = false;
                self.retry = None;
                self.restore_retry = None;
                after = AfterControl::PollNow;
                Ok(())
            }
            Control::RestoreNow => {
                tracing::info!("IPC: restoring original settings now and pausing");
                self.paused = true;
                self.applied = None;
                self.retry = None;
                self.restore_retry = None;
                match self.disengage().await {
                    RestoreStatus::Restored | RestoreStatus::NothingToRestore => {
                        self.last_error = None;
                        Ok(())
                    }
                    RestoreStatus::Partial | RestoreStatus::Error => {
                        let msg = "some original settings were not restored; see the daemon log".to_string();
                        self.last_error = Some(msg.clone());
                        Err(msg)
                    }
                }
            }
            Control::Shutdown => {
                after = AfterControl::Stop("IPC shutdown request");
                Ok(())
            }
        };
        let status = self.publish().await;
        let _ = req.reply.send(result.map(|()| status).map_err(|m| ErrorBody::new(ErrorKind::Failed, m)));
        after
    }

    /// One poll: interfaces, optional public-IP check, detector, reconcile.
    async fn tick(&mut self) {
        let ifaces = match list_interfaces_async().await {
            Ok(i) => i,
            Err(e) => {
                tracing::error!("listing interfaces: {e:#}");
                return;
            }
        };
        let public_ip = if self.detector.needs_ip_check(&ifaces, Instant::now()) {
            match geo::public_ip(self.resolve_opts.timeout).await {
                Ok(ip) => Some(ip),
                Err(e) => {
                    tracing::warn!("public IP check failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        if let Some(event) = self.detector.observe(&ifaces, public_ip, Instant::now()) {
            self.on_event(event);
        }
        self.vpn_interfaces = VpnDetector::vpn_interfaces(&ifaces);
        let vpn_present = !self.vpn_interfaces.is_empty();
        if !self.paused {
            self.reconcile(vpn_present).await;
        }
        self.publish().await;
    }

    fn on_event(&mut self, event: VpnEvent) {
        match event {
            VpnEvent::Up { exit_ip, interfaces } => {
                tracing::info!("VPN up via {} (exit {exit_ip})", interfaces.join(", "));
            }
            VpnEvent::Changed { exit_ip, interfaces } => {
                tracing::info!("VPN exit changed to {exit_ip} via {}", interfaces.join(", "));
            }
            VpnEvent::Down => {
                if self.sticky {
                    tracing::warn!("VPN down; --sticky set, keeping spoofed settings (your real location may leak)");
                } else {
                    tracing::info!("VPN down; restoring original settings");
                    self.applied = None;
                }
                self.retry = None;
                self.restore_retry = None;
            }
        }
    }

    /// Bring the system in line with the detector: engage for the exit IP,
    /// or restore originals when the VPN is gone.
    async fn reconcile(&mut self, vpn_present: bool) {
        let now = Instant::now();
        let desired = if self.detector.is_up() { self.detector.exit_ip() } else { None };
        if desired.is_none() {
            let retry_at = self.restore_retry.map(|(at, _)| at);
            if should_restore(self.detector.is_up(), vpn_present, self.sticky, retry_at, now) {
                self.restore_if_engaged().await;
            }
            return;
        }
        let retry_at = self.retry.filter(|r| Some(r.ip) == desired).map(|r| r.at);
        let Some(ip) = engage_target(desired, self.applied, retry_at, now) else {
            return;
        };
        match self.engage_for(ip).await {
            Ok(()) => {
                self.applied = Some(ip);
                self.retry = None;
                self.restore_retry = None;
                self.last_error = None;
            }
            Err(e) => {
                let attempt = self.retry.filter(|r| r.ip == ip).map_or(1, |r| r.attempt + 1);
                let delay = retry_delay(attempt);
                tracing::error!("engaging for exit {ip} incomplete: {e:#}; retrying in {}s", delay.as_secs());
                self.last_error = Some(format!("engaging for exit {ip}: {e:#}"));
                self.retry = Some(Retry { ip, at: Instant::now() + delay, attempt });
            }
        }
    }

    /// Restore originals if a snapshot is saved; schedule a retry on failure.
    async fn restore_if_engaged(&mut self) {
        let engaged = match with_orch(&self.orch, |o| o.is_engaged()).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("{e:#}");
                false
            }
        };
        if !engaged {
            self.restore_retry = None;
            return;
        }
        match self.disengage().await {
            RestoreStatus::Restored | RestoreStatus::NothingToRestore => {
                self.restore_retry = None;
                self.last_error = None;
            }
            RestoreStatus::Partial | RestoreStatus::Error => {
                let attempt = self.restore_retry.map_or(1, |(_, n)| n + 1);
                let delay = retry_delay(attempt);
                tracing::warn!("original settings not fully restored; retrying in {}s", delay.as_secs());
                self.last_error = Some("original settings not fully restored; retrying".to_string());
                self.restore_retry = Some((Instant::now() + delay, attempt));
            }
        }
    }

    /// Resolve `ip` with provider consensus (cached per exit IP), then engage.
    /// Never engages without consensus. Fails unless every backend applied.
    async fn engage_for(&mut self, ip: IpAddr) -> Result<()> {
        let profile = match &self.resolved {
            Some((cached_ip, profile)) if *cached_ip == ip => profile.clone(),
            _ => {
                let profile = geo::resolve(&self.providers, Some(ip), &self.resolve_opts)
                    .await
                    .with_context(|| format!("resolving location of {ip}"))?;
                self.resolved = Some((ip, profile.clone()));
                profile
            }
        };
        tracing::info!(
            "engaging {} / {} / {} ({:.4}, {:.4})",
            profile.country,
            profile.timezone,
            profile.locale,
            profile.lat,
            profile.lon
        );
        let report = with_orch(&self.orch, move |o| o.engage(&profile)).await?.context("engaging profile")?;
        log_report("engage", &report);
        let failed = report.outcomes.iter().filter(|o| !o.ok).count();
        if failed == 0 {
            Ok(())
        } else {
            Err(anyhow!("{failed} of {} backend(s) failed", report.outcomes.len()))
        }
    }

    /// Restore originals and say how it went.
    async fn disengage(&self) -> RestoreStatus {
        match with_orch(&self.orch, |o| o.disengage()).await {
            Ok(Ok(report)) => {
                log_report("restore", &report);
                if report.all_ok() {
                    RestoreStatus::Restored
                } else {
                    RestoreStatus::Partial
                }
            }
            Ok(Err(OrchestratorError::NotEngaged)) => {
                tracing::debug!("nothing to restore");
                RestoreStatus::NothingToRestore
            }
            Ok(Err(e)) => {
                tracing::error!("restore failed: {e}");
                RestoreStatus::Error
            }
            Err(e) => {
                tracing::error!("restore failed: {e:#}");
                RestoreStatus::Error
            }
        }
    }
}

fn log_report(action: &str, report: &Report) {
    for o in &report.outcomes {
        if o.ok {
            tracing::info!("{action}: {} ok", o.backend);
        } else {
            tracing::warn!("{action}: {} failed: {}", o.backend, o.error.as_deref().unwrap_or("unknown error"));
        }
    }
}

/// Startup crash recovery: if a saved state exists but no VPN is up, restore it.
async fn recover(daemon: &mut Daemon) -> Result<()> {
    let engaged = with_orch(&daemon.orch, |o| o.is_engaged()).await?;
    if !engaged {
        return Ok(());
    }
    let ifaces = list_interfaces_async().await?;
    if VpnDetector::vpn_interfaces(&ifaces).is_empty() {
        if daemon.sticky {
            tracing::warn!("found saved state from a previous run and no VPN is up; --sticky set, leaving it");
        } else {
            tracing::warn!("found saved state from a previous run and no VPN is up; restoring originals");
            daemon.restore_if_engaged().await;
        }
    } else {
        tracing::info!("found saved state from a previous run; VPN is up, will re-resolve and keep original snapshots");
    }
    Ok(())
}

/// Exit code for the final restore when the daemon stops.
fn exit_for(status: RestoreStatus) -> Exit {
    match status {
        RestoreStatus::Restored | RestoreStatus::NothingToRestore => Exit::Ok,
        RestoreStatus::Partial => Exit::Partial,
        RestoreStatus::Error => Exit::Failed,
    }
}

/// `geoveil run`.
pub async fn run(ctx: &Ctx, args: &RunArgs) -> Result<Exit> {
    // Listen for termination before touching anything, so a stop request
    // that arrives during recovery still leads to a restore.
    let mut shutdown = Shutdown::register();
    let lock = ctx.lock()?;
    tracing::debug!(lock = %lock.path().display(), "holding the state lock");
    let orch: SharedOrch = Arc::new(Mutex::new(ctx.orchestrator()?));
    let (status_tx, status_rx) = tokio::sync::watch::channel(DaemonStatus::default());
    let (control_tx, mut controls) = tokio::sync::mpsc::channel::<ControlRequest>(8);
    let mut daemon = Daemon {
        orch,
        detector: VpnDetector::new(DetectorConfig::default()),
        providers: geo::default_providers(),
        resolve_opts: ctx.resolve_opts.clone(),
        sticky: args.sticky,
        applied: None,
        retry: None,
        resolved: None,
        restore_retry: None,
        paused: false,
        last_error: None,
        vpn_interfaces: Vec::new(),
        state_dir: ctx.state_dir().display().to_string(),
        status_tx,
    };
    daemon.publish().await;
    let ipc_server = ipc::start(ipc::DaemonHandle::new(status_rx, control_tx)).await;

    let mut stop_reason = None;
    tokio::select! {
        r = recover(&mut daemon) => {
            if let Err(e) = r {
                tracing::error!("crash recovery: {e:#}");
            }
        }
        why = shutdown.recv() => stop_reason = Some(why),
    }

    if stop_reason.is_none() {
        tracing::info!("watching for VPN changes every {}s (Ctrl-C to stop)", args.interval_secs);
        let mut interval = tokio::time::interval(Duration::from_secs(args.interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The poll itself is raced against shutdown: a slow provider or a hung
        // child process must never delay the restore.
        let why = loop {
            tokio::select! {
                _ = interval.tick() => {}
                why = shutdown.recv() => break why,
                Some(req) = controls.recv() => {
                    match daemon.control(req).await {
                        AfterControl::Continue => {}
                        AfterControl::PollNow => interval.reset_immediately(),
                        AfterControl::Stop(why) => break why,
                    }
                    continue;
                }
            }
            tokio::select! {
                _ = daemon.tick() => {}
                why = shutdown.recv() => break why,
            }
        };
        stop_reason = Some(why);
    }
    let why = stop_reason.unwrap_or("stop request");
    // Stop taking IPC commands before the final restore.
    if let Some(server) = ipc_server {
        server.stop().await;
    }
    drop(controls);

    if daemon.sticky {
        tracing::warn!("{why}: stopping; --sticky set, leaving spoofed settings in place (run `geoveil restore` to undo)");
        drop(lock);
        return Ok(Exit::Ok);
    }
    tracing::info!("{why}: stopping; restoring original settings");
    let status = daemon.disengage().await;
    drop(lock);
    let exit = exit_for(status);
    if exit != Exit::Ok {
        eprintln!("error: some original settings were not restored; the snapshot is kept, run `geoveil restore` again");
    }
    Ok(exit)
}

/// Termination requests the daemon reacts to. Handlers are registered when
/// this is created; a handler that cannot be registered is logged and skipped.
struct Shutdown {
    #[cfg(unix)]
    signals: Vec<(&'static str, tokio::signal::unix::Signal)>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
    #[cfg(windows)]
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
    #[cfg(windows)]
    ctrl_close: Option<tokio::signal::windows::CtrlClose>,
    #[cfg(windows)]
    ctrl_logoff: Option<tokio::signal::windows::CtrlLogoff>,
    #[cfg(windows)]
    ctrl_shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

/// Keeps a registered handler, logging one that failed to register.
fn registered<T>(name: &str, result: std::io::Result<T>) -> Option<T> {
    result
        .map_err(|e| tracing::warn!("cannot listen for {name}: {e}; stop the process another way"))
        .ok()
}

/// Waits on an optional handler; never resolves when it is absent or closed.
async fn recv_or_pending<F: std::future::Future<Output = Option<()>>>(fut: Option<F>) {
    match fut {
        Some(f) => {
            if f.await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

impl Shutdown {
    #[cfg(unix)]
    fn register() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        let wanted = [
            ("Ctrl-C (SIGINT)", SignalKind::interrupt()),
            ("SIGTERM", SignalKind::terminate()),
            ("SIGHUP", SignalKind::hangup()),
        ];
        let signals = wanted
            .into_iter()
            .filter_map(|(name, kind)| registered(name, signal(kind)).map(|s| (name, s)))
            .collect();
        Self { signals }
    }

    #[cfg(windows)]
    fn register() -> Self {
        use tokio::signal::windows;
        Self {
            ctrl_c: registered("Ctrl-C", windows::ctrl_c()),
            ctrl_break: registered("Ctrl-Break", windows::ctrl_break()),
            ctrl_close: registered("console close", windows::ctrl_close()),
            ctrl_logoff: registered("logoff", windows::ctrl_logoff()),
            ctrl_shutdown: registered("shutdown", windows::ctrl_shutdown()),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn register() -> Self {
        Self {}
    }

    /// Resolves with the name of the first termination request.
    #[cfg(unix)]
    async fn recv(&mut self) -> &'static str {
        if self.signals.is_empty() {
            std::future::pending::<()>().await;
        }
        let futs: Vec<_> = self
            .signals
            .iter_mut()
            .map(|entry| {
                let (name, sig) = (entry.0, &mut entry.1);
                Box::pin(async move {
                    recv_or_pending(Some(sig.recv())).await;
                    name
                })
            })
            .collect();
        first_of(futs).await
    }

    #[cfg(windows)]
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = recv_or_pending(self.ctrl_c.as_mut().map(|s| s.recv())) => "Ctrl-C",
            _ = recv_or_pending(self.ctrl_break.as_mut().map(|s| s.recv())) => "Ctrl-Break",
            _ = recv_or_pending(self.ctrl_close.as_mut().map(|s| s.recv())) => "console window closed",
            _ = recv_or_pending(self.ctrl_logoff.as_mut().map(|s| s.recv())) => "user logoff",
            _ = recv_or_pending(self.ctrl_shutdown.as_mut().map(|s| s.recv())) => "system shutdown",
        }
    }

    #[cfg(not(any(unix, windows)))]
    async fn recv(&mut self) -> &'static str {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// The output of whichever future finishes first.
#[cfg(unix)]
async fn first_of<T, F>(mut futs: Vec<std::pin::Pin<Box<F>>>) -> T
where
    F: std::future::Future<Output = T>,
{
    std::future::poll_fn(move |cx| {
        for f in futs.iter_mut() {
            if let std::task::Poll::Ready(v) = f.as_mut().poll(cx) {
                return std::task::Poll::Ready(v);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn no_target_when_vpn_down() {
        assert_eq!(engage_target(None, Some(ip("1.1.1.1")), None, Instant::now()), None);
    }

    #[test]
    fn no_target_when_already_applied() {
        assert_eq!(engage_target(Some(ip("1.1.1.1")), Some(ip("1.1.1.1")), None, Instant::now()), None);
    }

    #[test]
    fn target_when_new_or_changed() {
        let now = Instant::now();
        assert_eq!(engage_target(Some(ip("1.1.1.1")), None, None, now), Some(ip("1.1.1.1")));
        assert_eq!(engage_target(Some(ip("2.2.2.2")), Some(ip("1.1.1.1")), None, now), Some(ip("2.2.2.2")));
    }

    #[test]
    fn backoff_respected() {
        let now = Instant::now();
        let later = now + Duration::from_secs(10);
        assert_eq!(engage_target(Some(ip("1.1.1.1")), None, Some(later), now), None);
        assert_eq!(engage_target(Some(ip("1.1.1.1")), None, Some(now), now), Some(ip("1.1.1.1")));
    }

    #[test]
    fn retry_delay_doubles_and_caps() {
        assert_eq!(retry_delay(0), RETRY_BACKOFF);
        assert_eq!(retry_delay(1), Duration::from_secs(30));
        assert_eq!(retry_delay(2), Duration::from_secs(60));
        assert_eq!(retry_delay(3), Duration::from_secs(120));
        assert_eq!(retry_delay(6), MAX_RETRY_BACKOFF);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_BACKOFF);
    }

    #[test]
    fn restore_is_retried_while_vpn_is_gone() {
        let now = Instant::now();
        // VPN gone, not sticky, no backoff: restore (also after a failed one).
        assert!(should_restore(false, false, false, None, now));
        assert!(should_restore(false, false, false, Some(now), now));
        // Backoff not expired yet.
        assert!(!should_restore(false, false, false, Some(now + Duration::from_secs(5)), now));
        // Sticky keeps the spoofed settings.
        assert!(!should_restore(false, false, true, None, now));
        // A VPN interface is present but not confirmed yet: keep state.
        assert!(!should_restore(false, true, false, None, now));
        // VPN confirmed up: never restore.
        assert!(!should_restore(true, false, false, None, now));
    }

    #[test]
    fn final_restore_status_maps_to_exit_code() {
        assert_eq!(exit_for(RestoreStatus::Restored), Exit::Ok);
        assert_eq!(exit_for(RestoreStatus::NothingToRestore), Exit::Ok);
        assert_eq!(exit_for(RestoreStatus::Partial), Exit::Partial);
        assert_eq!(exit_for(RestoreStatus::Error), Exit::Failed);
    }

    #[tokio::test]
    async fn shutdown_handlers_register() {
        // Registration must not panic and must not resolve on its own.
        let mut s = Shutdown::register();
        let r = tokio::time::timeout(Duration::from_millis(50), s.recv()).await;
        assert!(r.is_err(), "no termination request was sent");
    }
}
