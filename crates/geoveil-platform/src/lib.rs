//! OS backends. Every backend module is compiled on every OS (they only shell
//! out through [`runner::CommandRunner`]), so all of them are unit tested
//! everywhere. Only [`default_backends`] picks the set for the host OS.

pub mod interfaces;
pub mod linux;
pub mod macos;
pub mod runner;
pub mod windows;

use geoveil_core::Backend;
use std::sync::Arc;

pub use runner::{CommandOutput, CommandRunner, FakeRunner, SystemRunner};

/// Which optional backends to enable.
#[derive(Debug, Clone, Default)]
pub struct BackendOptions {
    /// Also spoof system locale / region (opt-in, like GeoSpoof).
    pub locale: bool,
    /// Enable OS location backends where available.
    pub location: bool,
}

/// Backends for the host OS, in apply order.
pub fn default_backends(runner: Arc<dyn CommandRunner>, opts: &BackendOptions) -> Vec<Box<dyn Backend>> {
    #[cfg(target_os = "windows")]
    {
        windows::backends(runner, opts)
    }
    #[cfg(target_os = "linux")]
    {
        linux::backends(runner, opts)
    }
    #[cfg(target_os = "macos")]
    {
        macos::backends(runner, opts)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = (runner, opts);
        Vec::new()
    }
}
