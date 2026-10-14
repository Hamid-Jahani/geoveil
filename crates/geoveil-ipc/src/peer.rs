//! Peer identity checks: both ends refuse a peer that runs as another user.
//!
//! - Windows: the peer's process id comes from the pipe
//!   (`GetNamedPipeClientProcessId` / `GetNamedPipeServerProcessId`), and its
//!   token user SID is compared with ours. A peer whose token cannot be read
//!   (a process of another user or a protected process) is refused.
//! - Unix: the peer's effective uid (`SO_PEERCRED` / `getpeereid`) must equal
//!   ours.

use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::StreamCommon;

/// Checks that the peer of `stream` runs as the current user.
pub fn check_peer(stream: &Stream) -> Result<(), String> {
    let creds = stream.peer_creds().map_err(|e| format!("cannot read peer credentials: {e}"))?;
    #[cfg(windows)]
    {
        let pid = creds.pid().ok_or("peer process id unknown")?;
        let ours = current_user_sid().map_err(|e| format!("cannot read our own user: {e}"))?;
        let theirs = process_user_sid(pid).map_err(|e| format!("cannot read the user of process {pid}: {e}"))?;
        same_identity(&ours, &theirs).then_some(()).ok_or_else(|| format!("process {pid} runs as {theirs}"))
    }
    #[cfg(unix)]
    {
        let euid = creds.euid().ok_or("peer uid unknown")?;
        same_identity(&current_uid(), &euid).then_some(()).ok_or_else(|| format!("peer runs as uid {euid}"))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = creds;
        Err("peer checks are not implemented on this platform".into())
    }
}

/// Identity comparison (kept separate so the rule is testable).
pub fn same_identity<T: PartialEq + ?Sized>(ours: &T, theirs: &T) -> bool {
    ours == theirs
}

/// Effective uid of this process.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail; uid_t is u32 on
    // Linux and macOS.
    unsafe { geteuid() }
}

/// String SID (`S-1-5-21-...`) of the user this process runs as.
#[cfg(windows)]
pub fn current_user_sid() -> std::io::Result<String> {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    win::token_user_sid(unsafe { GetCurrentProcess() })
}

/// String SID of the user process `pid` runs as.
#[cfg(windows)]
pub fn process_user_sid(pid: u32) -> std::io::Result<String> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain FFI call; a null handle signals failure.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let process = win::Handle(process);
    win::token_user_sid(process.0)
}

#[cfg(windows)]
mod win {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    /// Owned kernel handle, closed on drop.
    pub(super) struct Handle(pub HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: we own the handle and close it exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// The token user SID of `process`, as a string.
    pub(super) fn token_user_sid(process: HANDLE) -> io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `token` is a valid out pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);

        let mut needed = 0u32;
        // SAFETY: size query with a null buffer; fails with
        // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 storage keeps the TOKEN_USER suitably aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `needed` bytes.
        if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success the buffer starts with a TOKEN_USER whose SID
        // pointer points into the same buffer, which outlives this use.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let mut wide: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is valid; `wide` receives a LocalAlloc'ed string.
        if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `wide` is a NUL-terminated UTF-16 string from the OS.
        let s = unsafe {
            let len = (0..).take_while(|&i| *wide.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
        };
        // SAFETY: frees the string allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(wide.cast()) };
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_rule() {
        assert!(same_identity("S-1-5-21-1-1001", "S-1-5-21-1-1001"));
        assert!(!same_identity("S-1-5-21-1-1001", "S-1-5-21-1-1002"));
        assert!(same_identity(&1000u32, &1000u32));
        assert!(!same_identity(&1000u32, &0u32));
    }

    #[cfg(windows)]
    #[test]
    fn own_sid_is_readable_and_matches_own_process() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), sid);
    }
}
