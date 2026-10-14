//! Linux backends: `timedatectl` (timezone), `localectl` (locale) and the
//! geoclue static source (location).
//!
//! Everything here shells out through [`CommandRunner`] or touches files under
//! a configurable root, so it compiles and is unit tested on every OS.

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

/// Linux backends in apply order: timezone, then locale (if `opts.locale`),
/// then location (if `opts.location`).
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
    "interactive authentication required",
    "access denied",
    "permission denied",
    "not authorized",
    "polkit",
    "must be root",
    "not root",
    "operation not permitted",
];

/// Render a command line for error messages.
pub(crate) fn command_line(program: &str, args: &[&str]) -> String {
    std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ")
}

/// True if the command output indicates missing privileges.
fn indicates_privilege_problem(out: &CommandOutput) -> bool {
    let text = format!("{}\n{}", out.stderr, out.stdout).to_lowercase();
    PRIVILEGE_MARKERS.iter().any(|m| text.contains(m))
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

/// Map a non-zero exit to [`BackendError::NeedsPrivilege`] or [`BackendError::Command`].
pub(crate) fn exit_error(backend: &str, command: &str, out: &CommandOutput) -> BackendError {
    let detail = describe_output(out);
    if indicates_privilege_problem(out) {
        BackendError::NeedsPrivilege { backend: backend.to_string(), detail: format!("`{command}`: {detail}") }
    } else {
        BackendError::Command { backend: backend.to_string(), command: command.to_string(), detail }
    }
}

/// Short human description of a failed command's output.
fn describe_output(out: &CommandOutput) -> String {
    let stderr = out.stderr.trim();
    let msg = if stderr.is_empty() { out.stdout.trim() } else { stderr };
    if msg.is_empty() {
        format!("exit status {}", out.status)
    } else {
        format!("exit status {}: {msg}", out.status)
    }
}

/// Run a command and require exit status 0; returns the output on success.
pub(crate) fn run_checked(
    runner: &dyn CommandRunner,
    backend: &str,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput, BackendError> {
    let command = command_line(program, args);
    let out = runner.run(program, args).map_err(|e| spawn_error(backend, &command, &e))?;
    if out.success() {
        Ok(out)
    } else {
        Err(exit_error(backend, &command, &out))
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

    fn fail(stderr: &str) -> CommandOutput {
        CommandOutput { status: 1, stdout: String::new(), stderr: stderr.to_string() }
    }

    #[test]
    fn backends_order_default() {
        let r: Arc<dyn CommandRunner> = Arc::new(FakeRunner::new());
        let names: Vec<_> = backends(r, &BackendOptions::default()).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone"]);
    }

    #[test]
    fn backends_order_all() {
        let r: Arc<dyn CommandRunner> = Arc::new(FakeRunner::new());
        let opts = BackendOptions { locale: true, location: true };
        let names: Vec<_> = backends(r, &opts).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone", "locale", "location"]);
    }

    #[test]
    fn backends_location_without_locale() {
        let r: Arc<dyn CommandRunner> = Arc::new(FakeRunner::new());
        let opts = BackendOptions { locale: false, location: true };
        let names: Vec<_> = backends(r, &opts).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["timezone", "location"]);
    }

    #[test]
    fn run_checked_maps_errors() {
        let fake = FakeRunner::new();
        fake.push_err(io::ErrorKind::NotFound);
        fake.push(fail("Failed to set time zone: Interactive authentication required."));
        fake.push(fail("Failed to set time zone: Access denied"));
        fake.push(fail("Invalid time zone 'X'"));
        fake.push_stdout("ok");

        assert!(matches!(run_checked(&fake, "t", "x", &[]), Err(BackendError::Unsupported { .. })));
        assert!(matches!(run_checked(&fake, "t", "x", &[]), Err(BackendError::NeedsPrivilege { .. })));
        assert!(matches!(run_checked(&fake, "t", "x", &[]), Err(BackendError::NeedsPrivilege { .. })));
        match run_checked(&fake, "t", "x", &["a", "b"]) {
            Err(BackendError::Command { command, detail, .. }) => {
                assert_eq!(command, "x a b");
                assert!(detail.contains("Invalid time zone"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(run_checked(&fake, "t", "x", &[]).unwrap().stdout, "ok");
    }

    #[test]
    fn snapshot_owner_check() {
        assert!(check_snapshot_owner("timezone", "timezone").is_ok());
        assert!(check_snapshot_owner("timezone", "locale").is_err());
    }
}
