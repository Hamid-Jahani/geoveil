//! Background worker: talks to the daemon over IPC on its own thread (tokio
//! runtime) and sends [`TrayState`] updates to the UI thread.
//!
//! - Polls `status` every [`POLL_INTERVAL`].
//! - When no daemon answers on the first poll, starts `geoveil run` with the
//!   saved options (once; "Start the daemon" in the menu tries again) and
//!   waits up to [`START_TIMEOUT`] for it to listen.
//! - Runs menu commands (pause, resume, restore now) and reports failures as
//!   a notice.
//! - On Quit, stops a daemon the tray started (the daemon restores the
//!   original settings, unless it runs `--sticky`) before the tray exits.
//!
//! The daemon side is behind [`DaemonLink`], so the logic is tested with a
//! fake.

use crate::launcher;
use crate::model::{Link, TrayState};
use geoveil_ipc::{Client, ClientError, Command, DaemonOptions, DaemonStatus};
use std::future::Future;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Time between status polls.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long a started daemon may take to answer.
pub const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout for a `status` request.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for pause / resume / restore (a restore runs external tools).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
/// How long Quit waits for a stopped daemon to finish its restore.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(90);

/// Commands from the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    /// IPC `pause`.
    Pause,
    /// IPC `resume`.
    Resume,
    /// IPC `restore_now`.
    RestoreNow,
    /// Start `geoveil run` now.
    StartDaemon,
    /// Stop a daemon the tray started, then exit.
    Quit,
}

/// Events for the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// New state to show.
    State(Box<TrayState>),
    /// The worker is done; the UI should exit.
    Exit,
}

/// The daemon as the worker needs it.
pub trait DaemonLink: Send {
    /// Connects if not connected.
    fn connect(&mut self) -> impl Future<Output = Result<(), ClientError>> + Send;
    /// Sends a command on the current connection.
    fn request(
        &mut self,
        cmd: Command,
        timeout: Duration,
    ) -> impl Future<Output = Result<DaemonStatus, ClientError>> + Send;
    /// Drops the connection.
    fn disconnect(&mut self);
    /// Whether a connection is open.
    fn is_connected(&self) -> bool;
    /// Starts `geoveil run`; the error is a user-facing message.
    fn spawn_daemon(&mut self) -> Result<(), String>;
    /// If the daemon this link started has exited, a message saying so.
    fn spawned_exit(&mut self) -> Option<String>;
    /// Waits up to `timeout` for the daemon this link started to exit.
    fn wait_spawned(&mut self, timeout: Duration) -> impl Future<Output = bool> + Send;
}

/// The worker state machine.
pub struct Worker<L: DaemonLink> {
    link: L,
    link_state: Link,
    status: Option<DaemonStatus>,
    /// Problem with the connection or the started daemon.
    conn_notice: Option<String>,
    /// The last command failed.
    cmd_notice: Option<String>,
    /// Start the daemon when none answers.
    auto_start: bool,
    spawn_attempted: bool,
    spawned: bool,
    starting_until: Option<Instant>,
}

impl<L: DaemonLink> Worker<L> {
    /// A worker over `link`; `auto_start` starts a daemon when none answers.
    pub fn new(link: L, auto_start: bool) -> Self {
        Self {
            link,
            link_state: Link::Connecting,
            status: None,
            conn_notice: None,
            cmd_notice: None,
            auto_start,
            spawn_attempted: false,
            spawned: false,
            starting_until: None,
        }
    }

    /// The state for the UI.
    pub fn state(&self) -> TrayState {
        TrayState {
            link: self.link_state,
            status: if self.link_state == Link::Connected { self.status.clone() } else { None },
            notice: self.cmd_notice.clone().or_else(|| self.conn_notice.clone()),
            spawned: self.spawned,
        }
    }

    fn set_disconnected(&mut self, notice: Option<String>) {
        self.link.disconnect();
        self.link_state = Link::Disconnected;
        self.status = None;
        if notice.is_some() {
            self.conn_notice = notice;
        }
    }

    fn start_daemon(&mut self) {
        self.spawn_attempted = true;
        match self.link.spawn_daemon() {
            Ok(()) => {
                self.spawned = true;
                self.conn_notice = None;
                self.link_state = Link::Starting;
                self.starting_until = Some(Instant::now() + START_TIMEOUT);
            }
            Err(msg) => self.set_disconnected(Some(msg)),
        }
    }

    /// One poll: connect if needed (starting the daemon when allowed), then
    /// refresh the status.
    pub async fn poll(&mut self) {
        if !self.link.is_connected() {
            match self.link.connect().await {
                Ok(()) => {}
                Err(ClientError::NotRunning(_)) => {
                    if let Some(msg) = self.link.spawned_exit() {
                        self.starting_until = None;
                        self.spawned = false;
                        self.set_disconnected(Some(msg));
                        return;
                    }
                    if let Some(until) = self.starting_until {
                        if Instant::now() < until {
                            self.link_state = Link::Starting;
                            return;
                        }
                        self.starting_until = None;
                        self.set_disconnected(Some("the daemon did not start listening; see the log".into()));
                        return;
                    }
                    if self.auto_start && !self.spawn_attempted {
                        self.start_daemon();
                        return;
                    }
                    self.set_disconnected(None);
                    return;
                }
                Err(e) => {
                    self.set_disconnected(Some(e.to_string()));
                    return;
                }
            }
        }
        match self.link.request(Command::Status, STATUS_TIMEOUT).await {
            Ok(status) => {
                self.link_state = Link::Connected;
                self.status = Some(status);
                self.conn_notice = None;
                self.starting_until = None;
            }
            Err(e) => self.set_disconnected(Some(format!("lost the daemon: {e}"))),
        }
    }

    /// Runs a UI command. Returns `true` when the worker should exit.
    pub async fn command(&mut self, cmd: UiCommand) -> bool {
        let ipc = match cmd {
            UiCommand::Pause => Command::Pause,
            UiCommand::Resume => Command::Resume,
            UiCommand::RestoreNow => Command::RestoreNow,
            UiCommand::StartDaemon => {
                if !self.link.is_connected() {
                    self.cmd_notice = None;
                    self.start_daemon();
                }
                return false;
            }
            UiCommand::Quit => {
                self.quit().await;
                return true;
            }
        };
        if !self.link.is_connected() {
            self.cmd_notice = Some("the daemon is not running".into());
            return false;
        }
        match self.link.request(ipc, COMMAND_TIMEOUT).await {
            Ok(status) => {
                self.status = Some(status);
                self.link_state = Link::Connected;
                self.cmd_notice = None;
            }
            Err(ClientError::Daemon(e)) => {
                self.cmd_notice = Some(format!("{}: {}", ipc.as_str(), e.message));
                // Refresh so the menu shows what did happen.
                if let Ok(status) = self.link.request(Command::Status, STATUS_TIMEOUT).await {
                    self.status = Some(status);
                }
            }
            Err(e) => self.set_disconnected(Some(format!("{}: {e}", ipc.as_str()))),
        }
        false
    }

    async fn quit(&mut self) {
        self.link_state = Link::Stopping;
        if !self.spawned {
            return;
        }
        if !self.link.is_connected() && self.link.connect().await.is_err() {
            return;
        }
        if self.link.request(Command::Shutdown, STATUS_TIMEOUT).await.is_ok() {
            self.link.disconnect();
            self.link.wait_spawned(STOP_TIMEOUT).await;
        }
    }
}

/// Runs the worker until Quit, sending every state change to `sink`.
pub async fn run<L: DaemonLink>(
    mut worker: Worker<L>,
    mut commands: mpsc::UnboundedReceiver<UiCommand>,
    sink: impl Fn(UiEvent),
) {
    let mut last: Option<TrayState> = None;
    let mut publish = |w: &Worker<L>| {
        let state = w.state();
        if last.as_ref() != Some(&state) {
            sink(UiEvent::State(Box::new(state.clone())));
            last = Some(state);
        }
    };
    loop {
        worker.poll().await;
        publish(&worker);
        let deadline = tokio::time::sleep(POLL_INTERVAL);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                cmd = commands.recv() => {
                    // The UI is gone: behave like Quit.
                    let cmd = cmd.unwrap_or(UiCommand::Quit);
                    if cmd == UiCommand::Quit {
                        worker.link_state = Link::Stopping;
                        publish(&worker);
                    }
                    if worker.command(cmd).await {
                        sink(UiEvent::Exit);
                        return;
                    }
                    publish(&worker);
                    if cmd == UiCommand::StartDaemon {
                        break;
                    }
                }
            }
        }
    }
}

/// The real link: IPC client plus the daemon process the tray started.
pub struct IpcLink {
    client: Option<Client>,
    options: DaemonOptions,
    log_dir: PathBuf,
    child: Option<Child>,
}

impl IpcLink {
    /// A link that starts the daemon with `options`, logging to `log_dir`.
    pub fn new(options: DaemonOptions, log_dir: PathBuf) -> Self {
        Self { client: None, options, log_dir, child: None }
    }
}

impl DaemonLink for IpcLink {
    async fn connect(&mut self) -> Result<(), ClientError> {
        if self.client.is_none() {
            self.client = Some(Client::connect().await?);
        }
        Ok(())
    }

    async fn request(&mut self, cmd: Command, timeout: Duration) -> Result<DaemonStatus, ClientError> {
        match self.client.as_mut() {
            Some(c) => c.request(cmd, timeout).await,
            None => Err(ClientError::NotRunning("not connected".into())),
        }
    }

    fn disconnect(&mut self) {
        self.client = None;
    }

    fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    fn spawn_daemon(&mut self) -> Result<(), String> {
        let tray_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from));
        let cmd = launcher::daemon_command(&self.options, tray_dir.as_deref(), |p| p.is_file());
        match launcher::spawn_daemon(&cmd, &self.log_dir) {
            Ok((child, log)) => {
                tracing::info!("started {} (pid {}), log {}", cmd.program.display(), child.id(), log.display());
                self.child = Some(child);
                Ok(())
            }
            Err(e) => Err(format!("cannot start {}: {e}", cmd.program.display())),
        }
    }

    fn spawned_exit(&mut self) -> Option<String> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                let log = self.log_dir.join(launcher::LOG_FILE);
                Some(format!("geoveil run stopped ({status}); see {}", log.display()))
            }
            _ => None,
        }
    }

    async fn wait_spawned(&mut self, timeout: Duration) -> bool {
        let Some(child) = self.child.as_mut() else { return true };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                self.child = None;
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoveil_ipc::{ErrorBody, ErrorKind};
    use std::collections::VecDeque;

    /// Scripted daemon.
    #[derive(Default)]
    struct FakeLink {
        /// Results of `connect`, in order; empty means "not running".
        connects: VecDeque<Result<(), ClientError>>,
        /// Results of `request`, in order; empty means a default status.
        replies: VecDeque<Result<DaemonStatus, ClientError>>,
        connected: bool,
        spawn_result: Option<Result<(), String>>,
        spawned: u32,
        exit_message: Option<String>,
        requests: Vec<Command>,
        waited: bool,
    }

    impl DaemonLink for FakeLink {
        async fn connect(&mut self) -> Result<(), ClientError> {
            let r = self.connects.pop_front().unwrap_or_else(|| Err(ClientError::NotRunning("fake".into())));
            self.connected = r.is_ok();
            r
        }
        async fn request(&mut self, cmd: Command, _t: Duration) -> Result<DaemonStatus, ClientError> {
            self.requests.push(cmd);
            self.replies.pop_front().unwrap_or_else(|| Ok(DaemonStatus::default()))
        }
        fn disconnect(&mut self) {
            self.connected = false;
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn spawn_daemon(&mut self) -> Result<(), String> {
            self.spawned += 1;
            self.spawn_result.clone().unwrap_or(Ok(()))
        }
        fn spawned_exit(&mut self) -> Option<String> {
            self.exit_message.take()
        }
        async fn wait_spawned(&mut self, _t: Duration) -> bool {
            self.waited = true;
            true
        }
    }

    fn worker(link: FakeLink) -> Worker<FakeLink> {
        Worker::new(link, true)
    }

    #[tokio::test]
    async fn running_daemon_is_used_as_is() {
        let mut w = worker(FakeLink { connects: [Ok(())].into(), ..Default::default() });
        w.poll().await;
        let s = w.state();
        assert_eq!(s.link, Link::Connected);
        assert!(s.status.is_some());
        assert!(!s.spawned);
        assert_eq!(w.link.spawned, 0);
        assert_eq!(w.link.requests, [Command::Status]);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_daemon_is_started_once() {
        let mut w = worker(FakeLink::default());
        w.poll().await;
        assert_eq!(w.state().link, Link::Starting);
        assert!(w.state().spawned);
        assert_eq!(w.link.spawned, 1);

        // Still starting: no second spawn.
        w.poll().await;
        assert_eq!(w.state().link, Link::Starting);
        assert_eq!(w.link.spawned, 1);

        // It starts listening.
        w.link.connects.push_back(Ok(()));
        w.poll().await;
        assert_eq!(w.state().link, Link::Connected);
        assert_eq!(w.state().notice, None);
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_that_never_listens_times_out() {
        let mut w = worker(FakeLink::default());
        w.poll().await;
        tokio::time::advance(START_TIMEOUT + Duration::from_secs(1)).await;
        w.poll().await;
        let s = w.state();
        assert_eq!(s.link, Link::Disconnected);
        assert!(s.notice.unwrap().contains("did not start listening"));
        // No automatic retry loop.
        w.poll().await;
        assert_eq!(w.link.spawned, 1);
    }

    #[tokio::test]
    async fn daemon_that_exits_is_reported() {
        let mut w = worker(FakeLink::default());
        w.poll().await;
        w.link.exit_message = Some("geoveil run stopped (exit code: 1)".into());
        w.poll().await;
        let s = w.state();
        assert_eq!(s.link, Link::Disconnected);
        assert!(!s.spawned);
        assert_eq!(s.notice.as_deref(), Some("geoveil run stopped (exit code: 1)"));
    }

    #[tokio::test]
    async fn no_auto_start_when_disabled() {
        let mut w = Worker::new(FakeLink::default(), false);
        w.poll().await;
        assert_eq!(w.state().link, Link::Disconnected);
        assert_eq!(w.link.spawned, 0);
        // The menu can still start it.
        assert!(!w.command(UiCommand::StartDaemon).await);
        assert_eq!(w.link.spawned, 1);
        assert_eq!(w.state().link, Link::Starting);
    }

    #[tokio::test]
    async fn spawn_failure_is_a_notice() {
        let mut w = worker(FakeLink { spawn_result: Some(Err("cannot start geoveil: not found".into())), ..Default::default() });
        w.poll().await;
        let s = w.state();
        assert_eq!(s.link, Link::Disconnected);
        assert_eq!(s.notice.as_deref(), Some("cannot start geoveil: not found"));
        assert!(!s.spawned);
    }

    #[tokio::test]
    async fn commands_update_status_and_errors_become_notices() {
        let mut w = worker(FakeLink { connects: [Ok(())].into(), ..Default::default() });
        w.poll().await;
        w.link.replies.push_back(Ok(DaemonStatus { paused: true, ..Default::default() }));
        assert!(!w.command(UiCommand::Pause).await);
        assert!(w.state().status.unwrap().paused);

        w.link.replies.push_back(Err(ClientError::Daemon(ErrorBody::new(ErrorKind::Failed, "locale not restored"))));
        w.command(UiCommand::RestoreNow).await;
        let s = w.state();
        assert_eq!(s.link, Link::Connected, "a daemon-side failure keeps the connection");
        assert_eq!(s.notice.as_deref(), Some("restore_now: locale not restored"));
        assert_eq!(w.link.requests, [Command::Status, Command::Pause, Command::RestoreNow, Command::Status]);

        w.command(UiCommand::Resume).await;
        assert_eq!(w.state().notice, None, "a later success clears the notice");
    }

    #[tokio::test]
    async fn broken_connection_disconnects() {
        let mut w = worker(FakeLink { connects: [Ok(())].into(), ..Default::default() });
        w.poll().await;
        w.link.replies.push_back(Err(ClientError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe))));
        w.poll().await;
        let s = w.state();
        assert_eq!(s.link, Link::Disconnected);
        assert!(s.notice.unwrap().starts_with("lost the daemon"));
    }

    #[tokio::test]
    async fn command_without_daemon_is_refused_locally() {
        let mut w = Worker::new(FakeLink::default(), false);
        w.poll().await;
        w.command(UiCommand::Pause).await;
        assert_eq!(w.state().notice.as_deref(), Some("the daemon is not running"));
        assert!(w.link.requests.is_empty());
    }

    #[tokio::test]
    async fn quit_leaves_a_foreign_daemon_running() {
        let mut w = worker(FakeLink { connects: [Ok(())].into(), ..Default::default() });
        w.poll().await;
        assert!(w.command(UiCommand::Quit).await);
        assert_eq!(w.link.requests, [Command::Status], "no shutdown for a daemon the tray did not start");
        assert_eq!(w.state().link, Link::Stopping);
    }

    #[tokio::test]
    async fn quit_stops_a_daemon_the_tray_started() {
        let mut w = worker(FakeLink::default());
        w.poll().await;
        w.link.connects.push_back(Ok(()));
        w.poll().await;
        assert!(w.command(UiCommand::Quit).await);
        assert_eq!(w.link.requests, [Command::Status, Command::Shutdown]);
        assert!(w.link.waited);
    }

    #[tokio::test(start_paused = true)]
    async fn run_loop_publishes_changes_and_exits_on_quit() {
        let w = worker(FakeLink { connects: [Ok(())].into(), ..Default::default() });
        let (tx, rx) = mpsc::unbounded_channel();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_events = events.clone();
        tx.send(UiCommand::Quit).unwrap();
        run(w, rx, move |e| sink_events.lock().unwrap().push(e)).await;
        let events = events.lock().unwrap();
        assert!(matches!(&events[0], UiEvent::State(s) if s.link == Link::Connected));
        assert!(matches!(&events[1], UiEvent::State(s) if s.link == Link::Stopping));
        assert_eq!(events.last(), Some(&UiEvent::Exit));
    }
}
