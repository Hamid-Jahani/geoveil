//! The trait every OS backend (timezone, locale, location, ...) implements.

use crate::types::{BackendSnapshot, Profile, VerifyReport};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{backend}: needs elevated privileges: {detail}")]
    NeedsPrivilege { backend: String, detail: String },
    #[error("{backend}: not supported on this system: {detail}")]
    Unsupported { backend: String, detail: String },
    #[error("{backend}: command `{command}` failed: {detail}")]
    Command { backend: String, command: String, detail: String },
    #[error("{backend}: {detail}")]
    Other { backend: String, detail: String },
}

/// A system setting Geoveil can change and later undo.
///
/// Contract:
/// - `snapshot` reads current state without changing anything.
/// - `apply` sets the system to match `profile`. Must be idempotent.
/// - `restore` returns the system to exactly what `snapshot` captured.
/// - `verify` reads back current state and compares with `profile`.
pub trait Backend: Send + Sync {
    /// Stable identifier, used as the key in snapshots (e.g. "timezone").
    fn name(&self) -> &'static str;
    fn snapshot(&self) -> Result<BackendSnapshot, BackendError>;
    fn apply(&self, profile: &Profile) -> Result<(), BackendError>;
    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError>;
    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError>;
}
