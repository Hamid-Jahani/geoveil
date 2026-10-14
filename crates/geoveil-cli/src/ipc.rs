//! The IPC server of `geoveil run`: lets the tray (or any client running as
//! the same user) read the daemon status and send `restore_now`, `pause`,
//! `resume` and `shutdown`.
//!
//! Protocol, endpoint and access control live in the `geoveil-ipc` crate.
//! This module connects them to the daemon loop:
//!
//! - the daemon publishes a [`DaemonStatus`] into a `watch` channel after
//!   every poll and every command; `status` requests read the latest value
//!   without waiting for the loop;
//! - other commands are sent to the daemon loop as [`ControlRequest`]s and
//!   run between polls, so they never race a running engage or restore.
//!
//! The server is optional: when the endpoint cannot be bound (another daemon
//! of the same user owns it, or the platform refuses), the daemon logs a
//! warning and keeps running headless.

use geoveil_core::Profile;
use geoveil_ipc::protocol::{Command, CommandError, DaemonStatus, ErrorBody, ErrorKind, Handler, ProfileSummary};
use geoveil_ipc::transport::{self, Endpoint};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// Longest a command may wait for the daemon loop (a restore can run several
/// external tools, each with its own 120 s limit).
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(600);

/// A state-changing command for the daemon loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Restore the original settings now and pause.
    RestoreNow,
    /// Stop following the VPN.
    Pause,
    /// Follow the VPN again.
    Resume,
    /// Stop the daemon (restoring unless `--sticky`).
    Shutdown,
}

impl Control {
    /// The control for a wire command; `None` for `status`, which the server
    /// answers itself.
    pub fn from_command(cmd: Command) -> Option<Self> {
        match cmd {
            Command::Status => None,
            Command::RestoreNow => Some(Control::RestoreNow),
            Command::Pause => Some(Control::Pause),
            Command::Resume => Some(Control::Resume),
            Command::Shutdown => Some(Control::Shutdown),
        }
    }
}

/// Answer to a [`ControlRequest`].
pub type ControlReply = Result<DaemonStatus, CommandError>;

/// A command queued for the daemon loop, with the channel for its answer.
#[derive(Debug)]
pub struct ControlRequest {
    /// What to do.
    pub control: Control,
    /// Where to send the status afterwards (or the error).
    pub reply: oneshot::Sender<ControlReply>,
}

/// The [`Handler`] the IPC server calls.
pub struct DaemonHandle {
    status: watch::Receiver<DaemonStatus>,
    controls: mpsc::Sender<ControlRequest>,
    timeout: Duration,
}

impl DaemonHandle {
    /// A handle reading `status` and queueing commands on `controls`.
    pub fn new(status: watch::Receiver<DaemonStatus>, controls: mpsc::Sender<ControlRequest>) -> Self {
        Self { status, controls, timeout: CONTROL_TIMEOUT }
    }
}

impl Handler for DaemonHandle {
    async fn handle(&self, cmd: Command) -> Result<DaemonStatus, CommandError> {
        let Some(control) = Control::from_command(cmd) else {
            return Ok(self.status.borrow().clone());
        };
        let (reply, answer) = oneshot::channel();
        let stopping = || ErrorBody::new(ErrorKind::ShuttingDown, "the daemon is stopping");
        self.controls.send(ControlRequest { control, reply }).await.map_err(|_| stopping())?;
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(stopping()),
            Err(_) => Err(ErrorBody::new(
                ErrorKind::Internal,
                format!("`{}` did not finish within {}s", cmd.as_str(), self.timeout.as_secs()),
            )),
        }
    }
}

/// What the daemon knows, for [`build_status`].
#[derive(Debug, Clone, Default)]
pub struct StatusParts {
    /// A snapshot is saved.
    pub engaged: bool,
    /// Following the VPN is paused.
    pub paused: bool,
    /// `--sticky`.
    pub sticky: bool,
    /// Applied profile.
    pub profile: Option<Profile>,
    /// The detector reports the VPN as up.
    pub vpn_up: bool,
    /// Current exit IP.
    pub exit_ip: Option<IpAddr>,
    /// VPN-like interfaces seen at the last poll.
    pub vpn_interfaces: Vec<String>,
    /// Last engage / restore error.
    pub last_error: Option<String>,
    /// State directory.
    pub state_dir: String,
}

/// The wire status for the daemon's current state.
pub fn build_status(parts: StatusParts) -> DaemonStatus {
    DaemonStatus {
        engaged: parts.engaged,
        paused: parts.paused,
        sticky: parts.sticky,
        profile: parts.profile.as_ref().map(profile_summary),
        vpn_up: parts.vpn_up,
        exit_ip: parts.exit_ip.map(|ip| ip.to_string()),
        vpn_interfaces: parts.vpn_interfaces,
        last_error: parts.last_error,
        state_dir: parts.state_dir,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The client-facing part of a profile.
pub fn profile_summary(p: &Profile) -> ProfileSummary {
    ProfileSummary {
        country: p.country.clone(),
        timezone: p.timezone.clone(),
        locale: p.locale.clone(),
        lat: p.lat,
        lon: p.lon,
    }
}

/// A running IPC server; stopped when dropped.
pub struct IpcServer {
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl IpcServer {
    /// Stops accepting connections and waits for the accept loop to end.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Binds the current user's endpoint and serves `handle` on it. Returns
/// `None` (after logging why) when the endpoint is unavailable.
pub async fn start(handle: DaemonHandle) -> Option<IpcServer> {
    let endpoint = match Endpoint::for_current_user() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("IPC disabled: cannot determine the endpoint: {e}");
            return None;
        }
    };
    if transport::is_listening(&endpoint).await {
        tracing::warn!(
            "IPC disabled: another geoveil daemon already answers on {} (the tray will talk to that one)",
            endpoint.display()
        );
        return None;
    }
    let listener = match transport::bind(&endpoint) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("IPC disabled: cannot listen on {}: {e}", endpoint.display());
            return None;
        }
    };
    tracing::info!("IPC listening on {}", endpoint.display());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(transport::serve(listener, Arc::new(handle), async {
        let _ = stopped.await;
    }));
    Some(IpcServer { stop: Some(stop), task: Some(task) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoveil_core::geo;
    use geoveil_ipc::protocol::handle_line;

    fn handle_with_loop(
        timeout: Duration,
    ) -> (DaemonHandle, watch::Sender<DaemonStatus>, mpsc::Receiver<ControlRequest>) {
        let (status_tx, status_rx) = watch::channel(DaemonStatus::default());
        let (tx, rx) = mpsc::channel(4);
        let mut h = DaemonHandle::new(status_rx, tx);
        h.timeout = timeout;
        (h, status_tx, rx)
    }

    #[test]
    fn wire_commands_map_to_controls() {
        assert_eq!(Control::from_command(Command::Status), None);
        assert_eq!(Control::from_command(Command::RestoreNow), Some(Control::RestoreNow));
        assert_eq!(Control::from_command(Command::Pause), Some(Control::Pause));
        assert_eq!(Control::from_command(Command::Resume), Some(Control::Resume));
        assert_eq!(Control::from_command(Command::Shutdown), Some(Control::Shutdown));
    }

    #[tokio::test]
    async fn status_reads_the_latest_published_value() {
        let (h, status_tx, mut rx) = handle_with_loop(CONTROL_TIMEOUT);
        status_tx.send_replace(DaemonStatus { engaged: true, ..Default::default() });
        let r = handle_line(r#"{"v":1,"id":1,"cmd":"status"}"#, &h).await;
        assert!(r.ok && r.data.unwrap().engaged);
        assert!(rx.try_recv().is_err(), "status must not involve the daemon loop");
    }

    #[tokio::test]
    async fn commands_go_through_the_daemon_loop() {
        let (h, _status_tx, mut rx) = handle_with_loop(CONTROL_TIMEOUT);
        let daemon = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(req) = rx.recv().await {
                seen.push(req.control);
                let reply = match req.control {
                    Control::RestoreNow => Err(ErrorBody::new(ErrorKind::Failed, "locale not restored")),
                    c => Ok(DaemonStatus { paused: c == Control::Pause, ..Default::default() }),
                };
                let _ = req.reply.send(reply);
            }
            seen
        });
        let r = handle_line(r#"{"v":1,"id":2,"cmd":"pause"}"#, &h).await;
        assert!(r.ok && r.data.unwrap().paused);
        let r = handle_line(r#"{"v":1,"id":3,"cmd":"restore_now"}"#, &h).await;
        assert_eq!(r.error.unwrap().kind, ErrorKind::Failed);
        let r = handle_line(r#"{"v":1,"id":4,"cmd":"resume"}"#, &h).await;
        assert!(r.ok);
        drop(h);
        assert_eq!(daemon.await.unwrap(), vec![Control::Pause, Control::RestoreNow, Control::Resume]);
    }

    #[tokio::test]
    async fn stopped_daemon_reports_shutting_down() {
        let (h, _status_tx, rx) = handle_with_loop(CONTROL_TIMEOUT);
        drop(rx);
        let r = handle_line(r#"{"v":1,"id":5,"cmd":"pause"}"#, &h).await;
        assert_eq!(r.error.unwrap().kind, ErrorKind::ShuttingDown);
    }

    #[tokio::test]
    async fn dropped_reply_reports_shutting_down() {
        let (h, _status_tx, mut rx) = handle_with_loop(CONTROL_TIMEOUT);
        tokio::spawn(async move {
            // The loop takes the request and exits without answering.
            let _ = rx.recv().await;
        });
        let r = handle_line(r#"{"v":1,"id":6,"cmd":"shutdown"}"#, &h).await;
        assert_eq!(r.error.unwrap().kind, ErrorKind::ShuttingDown);
    }

    #[tokio::test]
    async fn busy_loop_times_out() {
        let (h, _status_tx, mut rx) = handle_with_loop(Duration::from_millis(20));
        let keep = tokio::spawn(async move {
            let req = rx.recv().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(req);
        });
        let r = handle_line(r#"{"v":1,"id":7,"cmd":"resume"}"#, &h).await;
        assert_eq!(r.error.unwrap().kind, ErrorKind::Internal);
        keep.abort();
    }

    #[test]
    fn status_is_built_from_daemon_state() {
        let profile = geo::manual_profile(52.52, 13.405, 100.0);
        let s = build_status(StatusParts {
            engaged: true,
            paused: true,
            sticky: false,
            profile: Some(profile.clone()),
            vpn_up: true,
            exit_ip: Some("203.0.113.7".parse().unwrap()),
            vpn_interfaces: vec!["wg0".into()],
            last_error: Some("x".into()),
            state_dir: "st".into(),
        });
        assert!(s.engaged && s.paused && s.vpn_up && !s.sticky);
        assert_eq!(s.exit_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(s.vpn_interfaces, ["wg0"]);
        let p = s.profile.unwrap();
        assert_eq!((p.timezone.as_str(), p.locale.as_str()), (profile.timezone.as_str(), profile.locale.as_str()));
        assert_eq!(s.pid, std::process::id());
        assert_eq!(s.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(s.state_dir, "st");
    }
}
