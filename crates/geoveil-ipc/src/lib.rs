//! Local IPC between the `geoveil run` daemon and its clients (the tray).
//!
//! - [`protocol`]: versioned JSON-lines messages and the server-side
//!   [`Handler`] contract. Pure, tested without any pipe or socket.
//! - [`transport`]: line framing, the per-user endpoint (named pipe on
//!   Windows, Unix socket elsewhere), the accept loop and the [`Client`].
//! - [`peer`]: both ends refuse peers running as another user.
//! - [`launch`]: the user's saved options for a tray-started daemon.

pub mod launch;
pub mod peer;
pub mod protocol;
pub mod transport;

pub use launch::DaemonOptions;
pub use protocol::{Command, CommandError, DaemonStatus, ErrorBody, ErrorKind, Handler, ProfileSummary};
pub use transport::{Client, ClientError, Endpoint};
