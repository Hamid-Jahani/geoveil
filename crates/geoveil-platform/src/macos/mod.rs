//! macOS backends: `systemsetup` (timezone), `defaults -g AppleLocale`
//! (locale) and Location Services (block only — see [`location`]).
//!
//! Everything shells out through [`CommandRunner`], so it compiles and is
//! unit tested on every OS.

pub mod locale;
pub mod location;
pub mod timezone;

pub use locale::LocaleBackend;
pub use location::LocationBackend;
pub use timezone::TimezoneBackend;

use crate::runner::{CommandOutput, CommandRunner};
use crate::BackendOptions;
use geoveil_core::{Backend, BackendError};
use std::io;
use std::sync::Arc;

/// macOS backends in apply order: timezone, then locale (if `opts.locale`),
/// then location (if `opts.location`; blocks Location Services).
pub fn backends(runner: Arc<dyn CommandRunner>, opts: &BackendOptions) -> Vec<Box<dyn Backend>> {
    let mut out: Vec<Box<dyn Backend>> = vec![Box::new(TimezoneBackend::new(runner.clone()))];
    if opts.locale {
        out.push(Box::new(LocaleBackend::new(runner.clone())));
    }
    if opts.location {
        out.push(Box::new(LocationBackend::new(runner)));
    }
    out
}

/// Output fragments (lower case) that mean "you are not allowed to do this".
const PRIVILEGE_MARKERS: &[&str] = &[
    "you need administrator access",
    "must be run as root",
    "must be root",
    "permission denied",
    "operation not permitted",
    "not privileged",
    "access denied",
];

/// `systemsetup` prints this and exits 0 when not run as root.
const ADMIN_REQUIRED_MARKER: &str = "you need administrator access";

/// `defaults` stderr fragment when a key or domain is missing.
const DEFAULTS_MISSING_MARKER: &str = "does not exist";

/// Render a command line for error messages.
pub(crate) fn command_line(program: &str, args: &[&str]) -> String {
    std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ")
}

fn combined_lower(out: &CommandOutput) -> String {
    format!("{}\n{}", out.stderr, out.stdout).to_lowercase()
}

/// True if a `defaults` failure means "key/domain not set".
pub(crate) fn is_defaults_missing(out: &CommandOutput) -> bool {
    !out.success() && combined_lower(out).contains(DEFAULTS_MISSING_MARKER)
}

/// Map an io error from spawning a process to a [`BackendError`].
pub(crate) fn spawn_error(backend: &str, command: &str, err: &io::Error) -> BackendError {
    match err.kind() {
        io::ErrorKind::NotFound => BackendError::Unsupported {
            backend: backend.to_string(),
            detail: format!("`{command}` is not available: {err}"),
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

/// Map a failed command to [`BackendError::NeedsPrivilege`] or [`BackendError::Command`].
pub(crate) fn exit_error(backend: &str, command: &str, out: &CommandOutput) -> BackendError {
    let stderr = out.stderr.trim();
    let msg = if stderr.is_empty() { out.stdout.trim() } else { stderr };
    let detail = if msg.is_empty() { format!("exit status {}", out.status) } else { format!("exit status {}: {msg}", out.status) };
    let text = combined_lower(out);
    if PRIVILEGE_MARKERS.iter().any(|m| text.contains(m)) {
        BackendError::NeedsPrivilege { backend: backend.to_string(), detail: format!("`{command}`: {detail}") }
    } else {
        BackendError::Command { backend: backend.to_string(), command: command.to_string(), detail }
    }
}

/// Run a command, returning its output whatever the exit status
/// (spawn failures are still mapped to errors).
pub(crate) fn run_raw(
    runner: &dyn CommandRunner,
    backend: &str,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput, BackendError> {
    runner.run(program, args).map_err(|e| spawn_error(backend, &command_line(program, args), &e))
}

/// Run a command and require success. Also treats the "You need
/// administrator access" banner (which `systemsetup` prints with exit 0)
/// as a privilege failure.
pub(crate) fn run_checked(
    runner: &dyn CommandRunner,
    backend: &str,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput, BackendError> {
    let out = run_raw(runner, backend, program, args)?;
    if out.success() && !combined_lower(&out).contains(ADMIN_REQUIRED_MARKER) {
        Ok(out)
    } else {
        Err(exit_error(backend, &command_line(program, args), &out))
    }
}

/// Check that a snapshot belongs to `backend`.
pub(crate) fn check_snapshot_owner(backend: &str, snapshot_backend: &str) -> Result<(), BackendError> {
    if snapshot_backend == backend {
        Ok(())
    } else {
        Err(BackendError::Other {
            backend: backend.to_string(),
            detail: format!("snapshot belongs to backend `{snapshot_backend}`"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput { status, stdout: stdout.to_string(), stderr: stderr.to_string() }
    }

    #[test]
    fn backends_order() {
        let r: Arc<dyn CommandRunner> = Arc::new(FakeRunner::new());
        let names: Vec<_> = backends(r.clone(), &BackendOptions::default()).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone"]);
        let all = BackendOptions { locale: true, location: true };
        let names: Vec<_> = backends(r.clone(), &all).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone", "locale", "location"]);
        let loc_only = BackendOptions { locale: false, location: true };
        let names: Vec<_> = backends(r, &loc_only).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone", "location"]);
    }

    #[test]
    fn admin_banner_with_exit_zero_needs_privilege() {
        let fake = FakeRunner::new();
        fake.push(out(0, "You need administrator access to run this tool... exiting!\n", ""));
        assert!(matches!(
            run_checked(&fake, "timezone", "systemsetup", &["-gettimezone"]),
            Err(BackendError::NeedsPrivilege { .. })
        ));
    }

    #[test]
    fn error_mapping() {
        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        fake.push(out(1, "", "Could not write domain; Operation not permitted"));
        fake.push(out(1, "", "something broke"));
        assert!(matches!(run_checked(&fake, "b", "x", &[]), Err(BackendError::Unsupported { .. })));
        assert!(matches!(run_checked(&fake, "b", "x", &[]), Err(BackendError::NeedsPrivilege { .. })));
        assert!(matches!(run_checked(&fake, "b", "x", &[]), Err(BackendError::Command { .. })));
    }

    #[test]
    fn defaults_missing_detection() {
        let missing = out(1, "", "The domain/default pair of (kCFPreferencesAnyApplication, AppleLocale) does not exist\n");
        assert!(is_defaults_missing(&missing));
        assert!(!is_defaults_missing(&out(0, "en_US\n", "")));
        assert!(!is_defaults_missing(&out(1, "", "other error")));
    }
}
