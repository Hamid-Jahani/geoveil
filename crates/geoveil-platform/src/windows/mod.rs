//! Windows backends.
//!
//! - [`TimezoneBackend`]: `tzutil /g` and `tzutil /s` with the IANA to Windows
//!   zone map from `geoveil_core::timezone`.
//! - [`LocaleBackend`] (opt-in): `Set-Culture` and `Set-WinHomeLocation`
//!   through PowerShell (per-user, no admin needed).
//!
//! Location spoofing on Windows (virtual NMEA port / GNSS driver) is a later phase.

mod locale;
mod timezone;

pub use locale::LocaleBackend;
pub use timezone::TimezoneBackend;

use crate::runner::{CommandOutput, CommandRunner};
use crate::BackendOptions;
use geoveil_core::{Backend, BackendError};
use std::io;
use std::sync::Arc;

/// Windows backends in apply order: timezone, then locale if `opts.locale`.
pub fn backends(runner: Arc<dyn CommandRunner>, opts: &BackendOptions) -> Vec<Box<dyn Backend>> {
    let mut out: Vec<Box<dyn Backend>> = vec![Box::new(TimezoneBackend::new(runner.clone()))];
    if opts.locale {
        out.push(Box::new(LocaleBackend::new(runner)));
    }
    if opts.location {
        tracing::warn!("Windows location spoofing lands in phase 3; the location backend is not available yet");
    }
    out
}

/// Phrases in command output that mean "you need to elevate".
const PRIVILEGE_MARKERS: &[&str] = &[
    "access is denied",
    "access denied",
    "accessdenied",
    "unauthorizedaccess",
    "permission denied",
    "requires elevation",
    "not root",
    "interactive authentication required",
    "polkit",
    "privilege",
];

/// True if command output indicates a missing privilege.
pub(crate) fn is_privilege_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    PRIVILEGE_MARKERS.iter().any(|m| lower.contains(m))
}

/// Combines stdout and stderr into one trimmed diagnostic string.
pub(crate) fn output_detail(out: &CommandOutput) -> String {
    let parts: Vec<&str> = [out.stderr.trim(), out.stdout.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        format!("exit status {}", out.status)
    } else {
        format!("exit status {}: {}", out.status, parts.join(" / "))
    }
}

/// Maps a failed command's output to the right [`BackendError`].
pub(crate) fn classify_failure(backend: &str, command: &str, out: &CommandOutput) -> BackendError {
    let detail = output_detail(out);
    if is_privilege_error(&out.stdout) || is_privilege_error(&out.stderr) {
        BackendError::NeedsPrivilege { backend: backend.to_string(), detail }
    } else {
        BackendError::Command { backend: backend.to_string(), command: command.to_string(), detail }
    }
}

/// Maps an io error from spawning a process to a [`BackendError`].
pub(crate) fn classify_io(backend: &str, command: &str, err: &io::Error) -> BackendError {
    match err.kind() {
        io::ErrorKind::NotFound => BackendError::Unsupported {
            backend: backend.to_string(),
            detail: format!("`{command}` not found: {err}"),
        },
        io::ErrorKind::PermissionDenied => BackendError::NeedsPrivilege {
            backend: backend.to_string(),
            detail: format!("cannot run `{command}`: {err}"),
        },
        _ => BackendError::Command {
            backend: backend.to_string(),
            command: command.to_string(),
            detail: err.to_string(),
        },
    }
}

/// Runs a command and returns its output only if it exited successfully.
///
/// `display` is the human-readable command used in error messages (scripts
/// can be long, so callers may pass a shortened form).
pub(crate) fn run_checked(
    runner: &dyn CommandRunner,
    backend: &str,
    display: &str,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput, BackendError> {
    let out = runner.run(program, args).map_err(|e| classify_io(backend, display, &e))?;
    if out.success() {
        Ok(out)
    } else {
        Err(classify_failure(backend, display, &out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    #[test]
    fn backends_order_and_options() {
        let runner: Arc<dyn CommandRunner> = Arc::new(FakeRunner::new());
        let names = |opts: &BackendOptions| {
            backends(runner.clone(), opts).iter().map(|b| b.name()).collect::<Vec<_>>()
        };
        assert_eq!(names(&BackendOptions::default()), vec!["timezone"]);
        assert_eq!(
            names(&BackendOptions { locale: true, location: false }),
            vec!["timezone", "locale"]
        );
        // No Windows location backend in this phase.
        assert_eq!(
            names(&BackendOptions { locale: true, location: true }),
            vec!["timezone", "locale"]
        );
    }

    #[test]
    fn privilege_detection() {
        assert!(is_privilege_error("Error: Access is denied."));
        assert!(is_privilege_error("Set-WinHomeLocation : UnauthorizedAccessException"));
        assert!(!is_privilege_error("Error: The system cannot find the file specified."));
    }

    #[test]
    fn io_not_found_is_unsupported() {
        let e = io::Error::from(io::ErrorKind::NotFound);
        assert!(matches!(classify_io("timezone", "tzutil /g", &e), BackendError::Unsupported { .. }));
        let e = io::Error::from(io::ErrorKind::Other);
        assert!(matches!(classify_io("timezone", "tzutil /g", &e), BackendError::Command { .. }));
    }
}
