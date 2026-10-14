//! Stop -> restore -> acknowledge protocol between an OS session-end
//! watcher and the daemon.
//!
//! On Windows, console handlers for `CTRL_LOGOFF_EVENT` and
//! `CTRL_SHUTDOWN_EVENT` are not called once a process has loaded `user32` or
//! `gdi32`, so `geoveil run` learns about a sign-out or shutdown from a hidden
//! window instead (see `shutdown_win`). That window's thread is not async and
//! must not return from `WM_ENDSESSION` before the original settings are back,
//! because Windows may end the process right after. This module carries that
//! handshake:
//!
//! 1. the watcher calls [`StopRequester::request_and_wait`], which sends a
//!    [`StopRequest`] and blocks;
//! 2. the daemon receives it through [`StopReceiver::recv`], stops, restores
//!    the original settings, then calls [`StopRequest::ack`];
//! 3. the watcher wakes up with [`StopOutcome::Acknowledged`], or with
//!    [`StopOutcome::TimedOut`] / [`StopOutcome::DaemonGone`].
//!
//! Everything here is plain channels, so it is compiled and tested on every OS.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// How long the session-end watcher waits for the daemon to restore before
/// letting the sign-out or shutdown continue anyway.
pub const RESTORE_TIMEOUT: Duration = Duration::from_secs(20);

/// A request for the daemon to stop and restore the original settings.
///
/// Dropping it without calling [`StopRequest::ack`] also wakes the watcher
/// (with [`StopOutcome::DaemonGone`]), so it never waits for the full timeout
/// on a request nobody will answer.
#[derive(Debug)]
pub struct StopRequest {
    /// Why the daemon is asked to stop, for the log ("user sign-out", ...).
    pub reason: &'static str,
    done: SyncSender<()>,
}

impl StopRequest {
    /// Tell the watcher the daemon has finished stopping (restore done or
    /// given up). Never blocks; a watcher that already timed out is ignored.
    pub fn ack(self) {
        let _ = self.done.try_send(());
    }
}

/// How a stop request ended, from the watcher's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The daemon stopped and acknowledged.
    Acknowledged,
    /// No acknowledgement within the timeout.
    TimedOut,
    /// The daemon is no longer listening (exited, or dropped the request).
    DaemonGone,
}

/// Watcher side: sends stop requests. Cheap to clone; usable from any thread,
/// with or without a Tokio runtime.
#[derive(Debug, Clone)]
pub struct StopRequester {
    tx: UnboundedSender<StopRequest>,
}

impl StopRequester {
    /// Ask the daemon to stop and block until it acknowledges, `timeout`
    /// passes, or the daemon turns out to be gone.
    pub fn request_and_wait(&self, reason: &'static str, timeout: Duration) -> StopOutcome {
        let (done, wait) = sync_channel(1);
        if self.tx.send(StopRequest { reason, done }).is_err() {
            return StopOutcome::DaemonGone;
        }
        wait_for_ack(&wait, timeout)
    }
}

fn wait_for_ack(wait: &Receiver<()>, timeout: Duration) -> StopOutcome {
    match wait.recv_timeout(timeout) {
        Ok(()) => StopOutcome::Acknowledged,
        Err(RecvTimeoutError::Timeout) => StopOutcome::TimedOut,
        Err(RecvTimeoutError::Disconnected) => StopOutcome::DaemonGone,
    }
}

/// Daemon side: receives stop requests.
#[derive(Debug)]
pub struct StopReceiver {
    rx: UnboundedReceiver<StopRequest>,
}

impl StopReceiver {
    /// The next stop request; `None` once every requester is gone.
    /// Cancel-safe, so it can sit in a `tokio::select!`.
    pub async fn recv(&mut self) -> Option<StopRequest> {
        self.rx.recv().await
    }

    /// Acknowledge every request already queued. Call it after the final
    /// restore, so a watcher that asked while the daemon was already stopping
    /// for another reason (Ctrl-C, console close) is not left waiting.
    pub fn ack_pending(&mut self) {
        while let Ok(req) = self.rx.try_recv() {
            req.ack();
        }
    }
}

/// A connected requester / receiver pair.
pub fn channel() -> (StopRequester, StopReceiver) {
    let (tx, rx) = unbounded_channel();
    (StopRequester { tx }, StopReceiver { rx })
}

/// The stop reason to log for a session end; `logoff` is true when only the
/// user is signing out (Windows: `ENDSESSION_LOGOFF` in `lParam`).
pub fn session_end_reason(logoff: bool) -> &'static str {
    if logoff {
        "user sign-out"
    } else {
        "system shutdown"
    }
}

/// What the watcher does when the session really ends (`ending` true) or a
/// pending end was cancelled (`ending` false): only a real end stops the
/// daemon. Returns the outcome, or `None` when nothing was requested.
pub fn on_session_end(requester: &StopRequester, ending: bool, logoff: bool, timeout: Duration) -> Option<StopOutcome> {
    if !ending {
        return None;
    }
    Some(requester.request_and_wait(session_end_reason(logoff), timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A fake daemon: waits for a stop request, runs a fake restore, acks.
    async fn fake_daemon(mut rx: StopReceiver, restored: Arc<AtomicBool>) -> &'static str {
        let req = rx.recv().await.expect("a stop request");
        tokio::time::sleep(Duration::from_millis(20)).await;
        restored.store(true, Ordering::SeqCst);
        let reason = req.reason;
        req.ack();
        reason
    }

    #[tokio::test]
    async fn stop_restore_ack_round_trip() {
        let (requester, rx) = channel();
        let restored = Arc::new(AtomicBool::new(false));
        let daemon = tokio::spawn(fake_daemon(rx, Arc::clone(&restored)));
        let seen = Arc::clone(&restored);
        // The watcher is a plain OS thread, like the Windows window thread.
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = on_session_end(&requester, true, true, Duration::from_secs(5));
            // The restore finished before the watcher was released.
            (outcome, seen.load(Ordering::SeqCst))
        })
        .await
        .unwrap();
        assert_eq!(outcome, (Some(StopOutcome::Acknowledged), true));
        assert_eq!(daemon.await.unwrap(), "user sign-out");
    }

    #[tokio::test]
    async fn times_out_when_daemon_never_acks() {
        let (requester, mut rx) = channel();
        let outcome = tokio::task::spawn_blocking(move || requester.request_and_wait("system shutdown", Duration::from_millis(50)));
        // Hold the request without acking until the watcher gave up.
        let req = rx.recv().await.unwrap();
        assert_eq!(outcome.await.unwrap(), StopOutcome::TimedOut);
        // A late ack must not panic or block.
        req.ack();
    }

    #[test]
    fn daemon_gone_when_receiver_dropped() {
        let (requester, rx) = channel();
        drop(rx);
        assert_eq!(requester.request_and_wait("x", Duration::from_secs(5)), StopOutcome::DaemonGone);
    }

    #[tokio::test]
    async fn dropped_request_releases_watcher_early() {
        let (requester, mut rx) = channel();
        let outcome = tokio::task::spawn_blocking(move || requester.request_and_wait("x", Duration::from_secs(30)));
        drop(rx.recv().await.unwrap());
        let start = std::time::Instant::now();
        assert_eq!(outcome.await.unwrap(), StopOutcome::DaemonGone);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn ack_pending_releases_queued_requests() {
        let (requester, mut rx) = channel();
        let other = requester.clone();
        let a = tokio::task::spawn_blocking(move || requester.request_and_wait("a", Duration::from_secs(5)));
        let b = tokio::task::spawn_blocking(move || other.request_and_wait("b", Duration::from_secs(5)));
        // Wait until both requests are queued, then drain them.
        let mut pending = Vec::new();
        while pending.len() < 2 {
            pending.push(rx.recv().await.unwrap());
        }
        for req in pending {
            req.ack();
        }
        rx.ack_pending(); // nothing left: must not block
        assert_eq!(a.await.unwrap(), StopOutcome::Acknowledged);
        assert_eq!(b.await.unwrap(), StopOutcome::Acknowledged);
    }

    #[tokio::test]
    async fn ack_pending_drains_without_recv() {
        let (requester, mut rx) = channel();
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let watcher = std::thread::spawn(move || {
            sent_tx.send(()).unwrap();
            requester.request_and_wait("late", Duration::from_secs(5))
        });
        sent_rx.recv().unwrap();
        // The request may not be queued yet right after the signal; retry briefly.
        let start = std::time::Instant::now();
        while !watcher.is_finished() && start.elapsed() < Duration::from_secs(5) {
            rx.ack_pending();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(watcher.join().unwrap(), StopOutcome::Acknowledged);
    }

    #[test]
    fn cancelled_session_end_does_not_stop() {
        let (requester, mut rx) = channel();
        assert_eq!(on_session_end(&requester, false, false, Duration::from_secs(5)), None);
        assert!(rx.rx.try_recv().is_err(), "no request sent");
    }

    #[test]
    fn reasons() {
        assert_eq!(session_end_reason(true), "user sign-out");
        assert_eq!(session_end_reason(false), "system shutdown");
    }
}
