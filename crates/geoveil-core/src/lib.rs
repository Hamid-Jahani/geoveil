//! Geoveil core: pure logic, no direct OS calls.

pub mod backend;
pub mod geo;
pub mod leaks;
pub mod locale;
pub mod orchestrator;
pub mod snapshot;
pub mod timezone;
pub mod types;
pub mod vpn;

pub use backend::{Backend, BackendError};
pub use types::*;
