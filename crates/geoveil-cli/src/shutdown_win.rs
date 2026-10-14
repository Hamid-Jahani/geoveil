//! Windows sign-out / shutdown watcher.
//!
//! Console control handlers for `CTRL_LOGOFF_EVENT` and `CTRL_SHUTDOWN_EVENT`
//! are not called once a process has loaded `user32` or `gdi32` (and
//! `geoveil.exe` does, through its dependencies), so `geoveil run` would not
//! restore the original settings at sign-out or shutdown. This module listens
//! the way GUI programs do instead: a dedicated thread owns a hidden top-level
//! window and runs a message loop.
//!
//! - `WM_QUERYENDSESSION`: register a shutdown block reason ("Geoveil is
//!   restoring your settings") and allow the session to end (`TRUE`).
//! - `WM_ENDSESSION` with `wParam` `TRUE`: ask the daemon to stop and restore
//!   through [`crate::session_end`], wait until it acknowledges or
//!   [`RESTORE_TIMEOUT`] passes, then remove the block reason. Windows may end
//!   the process as soon as this message returns, so the restore must be done
//!   by then. While waiting, messages other threads send to this window are
//!   still handled ([`pump_sent_messages`]): the restore broadcasts
//!   `WM_SETTINGCHANGE` / `WM_TIMECHANGE`, and a window that does not answer
//!   would stall each broadcast until its timeout.
//! - `WM_ENDSESSION` with `wParam` `FALSE` (sign-out cancelled): remove the
//!   block reason and keep running.
//!
//! The window is top-level on purpose: message-only windows (`HWND_MESSAGE`)
//! do not receive the `WM_QUERYENDSESSION` / `WM_ENDSESSION` broadcasts. It is
//! never shown.

use crate::session_end::{on_session_end, StopOutcome, StopRequester, RESTORE_TIMEOUT};
use std::cell::RefCell;
use std::io;
use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use std::time::Duration;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, ENDSESSION_LOGOFF, MSG, PM_NOREMOVE, PM_QS_SENDMESSAGE,
    QS_SENDMESSAGE, WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

/// Window class of the hidden watcher window.
const CLASS_NAME: &str = "GeoveilSessionEndWatcher";
/// Shown by Windows on the "apps are preventing sign-out" screen.
const BLOCK_REASON: &str = "Geoveil is restoring your settings";

thread_local! {
    /// The requester used by the window procedure (one per watcher thread).
    static REQUESTER: RefCell<Option<StopRequester>> = const { RefCell::new(None) };
}

/// Start the watcher thread. Returns once its window exists, or with the
/// error that prevented creating it. The thread lives as long as the process.
pub fn spawn(requester: StopRequester) -> io::Result<()> {
    spawn_window(requester).map(|_| ())
}

/// Like [`spawn`], returning the window handle (as an integer, since `HWND`
/// is not `Send`).
fn spawn_window(requester: StopRequester) -> io::Result<isize> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("geoveil-session-end".into()).spawn(move || {
        REQUESTER.with(|r| *r.borrow_mut() = Some(requester));
        match create_window() {
            Ok(hwnd) => {
                let _ = ready_tx.send(Ok(hwnd as isize));
                message_loop();
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    ready_rx
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("the sign-out watcher thread exited early")))
}

/// NUL-terminated UTF-16 for Win32 `PCWSTR` arguments.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Register the window class (once per process) and create the hidden window.
fn create_window() -> io::Result<HWND> {
    let class = wide(CLASS_NAME);
    // SAFETY: plain Win32 calls; every pointer passed is valid for the call
    // (the class name buffer outlives both calls, Windows copies it).
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
                return Err(err);
            }
        }
        // Top-level (no parent, not HWND_MESSAGE) and never shown.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(hwnd)
    }
}

/// Pump messages for this thread's windows until `WM_QUIT` or an error.
fn message_loop() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid, writable MSG for every call.
    unsafe {
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Waits up to `max` for a message sent to this thread's windows, then lets
/// Windows deliver every pending sent message to the window procedure.
/// Posted messages stay queued for the main loop.
fn pump_sent_messages(max: Duration) {
    let ms = u32::try_from(max.as_millis()).unwrap_or(u32::MAX);
    let mut msg = MSG::default();
    // SAFETY: no handles are passed (count 0, null array); `msg` is a valid,
    // writable MSG. PeekMessageW dispatches incoming sent messages itself,
    // and PM_NOREMOVE leaves any posted message where it is.
    unsafe {
        MsgWaitForMultipleObjectsEx(0, std::ptr::null(), ms, QS_SENDMESSAGE, 0);
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
    }
}

/// Whether a `WM_QUERYENDSESSION` / `WM_ENDSESSION` `lParam` means only the
/// user is signing out (not a shutdown or restart).
fn is_logoff(lparam: LPARAM) -> bool {
    (lparam as u32) & ENDSESSION_LOGOFF != 0
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            let reason = wide(BLOCK_REASON);
            // SAFETY: `hwnd` is this window; `reason` is NUL-terminated and
            // lives for the call.
            if unsafe { ShutdownBlockReasonCreate(hwnd, reason.as_ptr()) } == 0 {
                tracing::debug!("ShutdownBlockReasonCreate failed: {}", io::Error::last_os_error());
            }
            1 // TRUE: let the session end; the restore happens on WM_ENDSESSION.
        }
        WM_ENDSESSION => {
            let ending = wparam != 0;
            let requester = REQUESTER.with(|r| r.borrow().clone());
            if let Some(requester) = requester {
                match on_session_end(&requester, ending, is_logoff(lparam), RESTORE_TIMEOUT, pump_sent_messages) {
                    None => tracing::debug!("sign-out / shutdown cancelled; still running"),
                    Some(StopOutcome::Acknowledged) => tracing::debug!("daemon stopped; letting the session end"),
                    Some(StopOutcome::TimedOut) => tracing::warn!(
                        "settings not restored within {}s; letting the session end anyway (run `geoveil restore` after signing in)",
                        RESTORE_TIMEOUT.as_secs()
                    ),
                    Some(StopOutcome::DaemonGone) => tracing::debug!("daemon already stopped"),
                }
            }
            // SAFETY: `hwnd` is this window. Fails harmlessly with no reason set.
            unsafe { ShutdownBlockReasonDestroy(hwnd) };
            0
        }
        WM_DESTROY => {
            // SAFETY: called on the thread that owns the message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: forwarding the original arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_end::channel;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_CLOSE, WM_SETTINGCHANGE};

    /// Send a message to the watcher window from a plain thread and wait.
    fn send(hwnd: isize, msg: u32, wparam: WPARAM, lparam: LPARAM) -> std::thread::JoinHandle<LRESULT> {
        // SAFETY: SendMessageW to a window we created; it may already be gone,
        // in which case the call just fails.
        std::thread::spawn(move || unsafe { SendMessageW(hwnd as HWND, msg, wparam, lparam) })
    }

    #[test]
    fn logoff_flag() {
        assert!(is_logoff(ENDSESSION_LOGOFF as i32 as LPARAM));
        assert!(is_logoff(ENDSESSION_LOGOFF as LPARAM));
        assert!(!is_logoff(0));
    }

    #[tokio::test]
    async fn end_session_waits_for_daemon_ack() {
        let (requester, mut rx) = channel();
        let hwnd = spawn_window(requester).expect("watcher window");
        // Only WM_ENDSESSION is sent to our own hidden window: this exercises
        // the handshake without any real sign-out and without touching
        // system settings.
        let reply = send(hwnd, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as LPARAM);
        let req = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("stop request delivered")
            .expect("watcher alive");
        assert_eq!(req.reason, "user sign-out");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!reply.is_finished(), "WM_ENDSESSION returned before the ack");
        // While the restore runs, a setting-change notification sent to the
        // window must still be answered (tzutil / Set-Culture broadcast it).
        let notify = send(hwnd, WM_SETTINGCHANGE, 0, 0);
        let start = std::time::Instant::now();
        while !notify.is_finished() && start.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(notify.is_finished(), "a sent message was not handled during the wait");
        assert!(!reply.is_finished(), "still waiting for the ack");
        req.ack();
        assert_eq!(reply.join().unwrap(), 0);
        send(hwnd, WM_CLOSE, 0, 0).join().unwrap();
    }

    #[tokio::test]
    async fn cancelled_end_session_does_not_stop() {
        let (requester, mut rx) = channel();
        let hwnd = spawn_window(requester).expect("watcher window");
        assert_eq!(send(hwnd, WM_ENDSESSION, 0, 0).join().unwrap(), 0);
        let r = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(r.is_err(), "no stop request on a cancelled sign-out");
        send(hwnd, WM_CLOSE, 0, 0).join().unwrap();
    }
}
