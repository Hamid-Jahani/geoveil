//! Wire protocol: versioned JSON lines, one object per line, UTF-8.
//!
//! ```text
//! -> {"v":1,"id":7,"cmd":"status"}
//! <- {"v":1,"id":7,"ok":true,"data":{"engaged":true,"paused":false,...}}
//! -> {"v":1,"id":8,"cmd":"pause"}
//! <- {"v":1,"id":8,"ok":false,"error":{"kind":"failed","message":"..."}}
//! ```
//!
//! - Every request carries the protocol version `v` and a client-chosen `id`
//!   that the response echoes. A request the server cannot parse is answered
//!   with `id: null`.
//! - A line longer than [`MAX_LINE`] bytes is refused and the connection is
//!   closed.
//! - Every successful command answers with the daemon status after the
//!   command, so a client never needs a second round trip to refresh.
//!
//! Requests are untrusted input even though only the current user can
//! connect: parsing never panics, and unknown fields are ignored.

use serde::{Deserialize, Serialize};
use std::future::Future;

/// Protocol version spoken by this build. A request with another version is
/// refused with [`ErrorKind::UnsupportedVersion`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest accepted line in bytes, newline excluded.
pub const MAX_LINE: usize = 64 * 1024;

/// What a client asks the daemon to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// Report the current status. Changes nothing.
    Status,
    /// Restore the original settings now and pause following the VPN (so the
    /// next poll does not re-apply the profile). `resume` re-engages.
    RestoreNow,
    /// Stop following the VPN: the current system state is kept as is until
    /// `resume`.
    Pause,
    /// Follow the VPN again (engage or restore at the next poll).
    Resume,
    /// Stop the daemon the normal way: restore originals (unless the daemon
    /// runs with `--sticky`) and exit.
    Shutdown,
}

impl Command {
    /// Wire name of the command (the `cmd` field).
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::RestoreNow => "restore_now",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Shutdown => "shutdown",
        }
    }

    /// Parses a wire name.
    pub fn parse(s: &str) -> Option<Self> {
        [Command::Status, Command::RestoreNow, Command::Pause, Command::Resume, Command::Shutdown]
            .into_iter()
            .find(|c| c.as_str() == s)
    }
}

/// One request line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version, [`PROTOCOL_VERSION`].
    pub v: u32,
    /// Client-chosen id, echoed in the response.
    pub id: u64,
    /// The command.
    pub cmd: Command,
}

impl Request {
    /// A request for `cmd` with the current protocol version.
    pub fn new(id: u64, cmd: Command) -> Self {
        Self { v: PROTOCOL_VERSION, id, cmd }
    }

    /// The request as one line, newline included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_default();
        s.push('\n');
        s
    }
}

/// The applied profile, as far as a client needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProfileSummary {
    /// ISO-3166 alpha-2 country, for example `DE`. Empty for manual coordinates.
    pub country: String,
    /// IANA time zone, for example `Europe/Berlin`.
    pub timezone: String,
    /// BCP-47 locale, for example `de-DE`.
    pub locale: String,
    /// Latitude in degrees.
    pub lat: f64,
    /// Longitude in degrees.
    pub lon: f64,
}

/// Daemon status, returned by every successful command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    /// A snapshot of the original settings is saved, so the system is (at
    /// least partly) spoofed.
    pub engaged: bool,
    /// Following the VPN is paused (`pause` or `restore_now`).
    pub paused: bool,
    /// The daemon runs with `--sticky` (keeps the profile when the VPN drops).
    pub sticky: bool,
    /// The profile currently applied, if any.
    pub profile: Option<ProfileSummary>,
    /// The VPN detector reports the VPN as up.
    pub vpn_up: bool,
    /// The VPN exit IP, when known.
    pub exit_ip: Option<String>,
    /// Names of the interfaces that look like VPN adapters.
    pub vpn_interfaces: Vec<String>,
    /// The last engage or restore error, cleared by the next success.
    pub last_error: Option<String>,
    /// The daemon's state directory (snapshot and lock file).
    pub state_dir: String,
    /// Daemon process id.
    pub pid: u32,
    /// Daemon version (`CARGO_PKG_VERSION`).
    pub version: String,
}

/// Error classes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The line is not a JSON object with `v`, `id` and `cmd`.
    BadRequest,
    /// `v` is not [`PROTOCOL_VERSION`].
    UnsupportedVersion,
    /// `cmd` is not a known command.
    UnknownCommand,
    /// The line exceeds [`MAX_LINE`].
    LineTooLong,
    /// The daemon is stopping and no longer takes commands.
    ShuttingDown,
    /// The command ran but did not fully succeed (for example a partial restore).
    Failed,
    /// Anything else on the server side.
    Internal,
}

/// Error part of a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable class.
    pub kind: ErrorKind,
    /// Human-readable explanation.
    pub message: String,
}

/// An error returned by a [`Handler`].
pub type CommandError = ErrorBody;

impl ErrorBody {
    /// Builds an error body.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// One response line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Protocol version of the server.
    pub v: u32,
    /// The request id, or `None` when the request could not be parsed.
    pub id: Option<u64>,
    /// Whether the command succeeded.
    pub ok: bool,
    /// Status after the command (when `ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DaemonStatus>,
    /// Why the command failed (when not `ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    /// A success response.
    pub fn success(id: u64, data: DaemonStatus) -> Self {
        Self { v: PROTOCOL_VERSION, id: Some(id), ok: true, data: Some(data), error: None }
    }

    /// A failure response.
    pub fn failure(id: Option<u64>, error: ErrorBody) -> Self {
        Self { v: PROTOCOL_VERSION, id, ok: false, data: None, error: Some(error) }
    }

    /// The response as one line, newline included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| {
            // Serializing these plain types cannot fail; keep a valid line anyway.
            r#"{"v":1,"id":null,"ok":false,"error":{"kind":"internal","message":"serialization failed"}}"#.to_string()
        });
        s.push('\n');
        s
    }
}

/// Server-side command execution, implemented by the daemon.
pub trait Handler: Send + Sync {
    /// Runs `cmd` and returns the status afterwards.
    fn handle(&self, cmd: Command) -> impl Future<Output = Result<DaemonStatus, CommandError>> + Send;
}

/// Parses one request line. On failure returns the id (when it could be
/// read) and the error to send back.
pub fn parse_request(line: &str) -> Result<Request, (Option<u64>, ErrorBody)> {
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| (None, ErrorBody::new(ErrorKind::BadRequest, format!("not valid JSON: {e}"))))?;
    let obj = value
        .as_object()
        .ok_or_else(|| (None, ErrorBody::new(ErrorKind::BadRequest, "request must be a JSON object")))?;
    let id = obj.get("id").and_then(serde_json::Value::as_u64);
    let v = obj
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| (id, ErrorBody::new(ErrorKind::BadRequest, "missing protocol version `v`")))?;
    if v != u64::from(PROTOCOL_VERSION) {
        return Err((
            id,
            ErrorBody::new(
                ErrorKind::UnsupportedVersion,
                format!("protocol version {v} is not supported (this daemon speaks {PROTOCOL_VERSION})"),
            ),
        ));
    }
    let id = id.ok_or_else(|| (None, ErrorBody::new(ErrorKind::BadRequest, "missing or invalid `id`")))?;
    let name = obj
        .get("cmd")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| (Some(id), ErrorBody::new(ErrorKind::BadRequest, "missing `cmd`")))?;
    let cmd = Command::parse(name).ok_or_else(|| {
        let shown: String = name.chars().take(64).collect();
        (Some(id), ErrorBody::new(ErrorKind::UnknownCommand, format!("unknown command `{shown}`")))
    })?;
    Ok(Request { v: PROTOCOL_VERSION, id, cmd })
}

/// Handles one request line and returns the response to send.
pub async fn handle_line<H: Handler + ?Sized>(line: &str, handler: &H) -> Response {
    match parse_request(line) {
        Ok(req) => match handler.handle(req.cmd).await {
            Ok(status) => Response::success(req.id, status),
            Err(e) => Response::failure(Some(req.id), e),
        },
        Err((id, e)) => Response::failure(id, e),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records commands and answers from a script.
    #[derive(Default)]
    pub(crate) struct FakeHandler {
        pub seen: Mutex<Vec<Command>>,
        pub fail: Option<ErrorBody>,
    }

    impl Handler for FakeHandler {
        async fn handle(&self, cmd: Command) -> Result<DaemonStatus, CommandError> {
            self.seen.lock().unwrap().push(cmd);
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            Ok(DaemonStatus { paused: cmd == Command::Pause, pid: 42, ..Default::default() })
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().build().unwrap()
    }

    fn handle(line: &str, h: &FakeHandler) -> Response {
        rt().block_on(handle_line(line, h))
    }

    #[test]
    fn request_wire_format() {
        assert_eq!(Request::new(7, Command::Status).to_line(), "{\"v\":1,\"id\":7,\"cmd\":\"status\"}\n");
        assert_eq!(Request::new(1, Command::RestoreNow).to_line(), "{\"v\":1,\"id\":1,\"cmd\":\"restore_now\"}\n");
    }

    #[test]
    fn command_names_round_trip() {
        for c in [Command::Status, Command::RestoreNow, Command::Pause, Command::Resume, Command::Shutdown] {
            assert_eq!(Command::parse(c.as_str()), Some(c));
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
        }
        assert_eq!(Command::parse("Status"), None);
    }

    #[test]
    fn status_request_answers_with_data() {
        let h = FakeHandler::default();
        let r = handle(r#"{"v":1,"id":7,"cmd":"status"}"#, &h);
        assert!(r.ok);
        assert_eq!(r.id, Some(7));
        assert_eq!(r.data.unwrap().pid, 42);
        assert_eq!(*h.seen.lock().unwrap(), vec![Command::Status]);
    }

    #[test]
    fn each_command_reaches_the_handler() {
        let h = FakeHandler::default();
        for (i, name) in ["restore_now", "pause", "resume", "shutdown"].iter().enumerate() {
            let r = handle(&format!(r#"{{"v":1,"id":{i},"cmd":"{name}"}}"#), &h);
            assert!(r.ok, "{name}");
        }
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![Command::RestoreNow, Command::Pause, Command::Resume, Command::Shutdown]
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let h = FakeHandler::default();
        let r = handle(r#"{"v":1,"id":3,"cmd":"pause","args":{"x":1},"extra":true}"#, &h);
        assert!(r.ok);
        assert!(r.data.unwrap().paused);
    }

    #[test]
    fn handler_errors_are_reported_with_the_id() {
        let h = FakeHandler { fail: Some(ErrorBody::new(ErrorKind::Failed, "partial restore")), ..Default::default() };
        let r = handle(r#"{"v":1,"id":9,"cmd":"restore_now"}"#, &h);
        assert!(!r.ok);
        assert_eq!(r.id, Some(9));
        assert!(r.data.is_none());
        assert_eq!(r.error.unwrap(), ErrorBody::new(ErrorKind::Failed, "partial restore"));
    }

    fn error_kind(line: &str) -> (Option<u64>, ErrorKind) {
        let h = FakeHandler::default();
        let r = handle(line, &h);
        assert!(!r.ok, "{line}");
        assert!(h.seen.lock().unwrap().is_empty(), "handler must not run for {line}");
        (r.id, r.error.unwrap().kind)
    }

    #[test]
    fn malformed_requests_are_refused() {
        assert_eq!(error_kind("not json"), (None, ErrorKind::BadRequest));
        assert_eq!(error_kind("[1,2]"), (None, ErrorKind::BadRequest));
        assert_eq!(error_kind(r#"{"id":1,"cmd":"status"}"#), (Some(1), ErrorKind::BadRequest));
        assert_eq!(error_kind(r#"{"v":1,"cmd":"status"}"#), (None, ErrorKind::BadRequest));
        assert_eq!(error_kind(r#"{"v":1,"id":-4,"cmd":"status"}"#), (None, ErrorKind::BadRequest));
        assert_eq!(error_kind(r#"{"v":1,"id":2}"#), (Some(2), ErrorKind::BadRequest));
        assert_eq!(error_kind(r#"{"v":1,"id":2,"cmd":5}"#), (Some(2), ErrorKind::BadRequest));
    }

    #[test]
    fn other_versions_are_refused() {
        assert_eq!(error_kind(r#"{"v":2,"id":5,"cmd":"status"}"#), (Some(5), ErrorKind::UnsupportedVersion));
        assert_eq!(error_kind(r#"{"v":0,"id":5,"cmd":"status"}"#), (Some(5), ErrorKind::UnsupportedVersion));
    }

    #[test]
    fn unknown_commands_are_refused() {
        assert_eq!(error_kind(r#"{"v":1,"id":6,"cmd":"format_disk"}"#), (Some(6), ErrorKind::UnknownCommand));
    }

    #[test]
    fn unknown_command_name_is_truncated_in_message() {
        let long = "x".repeat(1000);
        let r = handle(&format!(r#"{{"v":1,"id":1,"cmd":"{long}"}}"#), &FakeHandler::default());
        assert!(r.error.unwrap().message.len() < 120);
    }

    #[test]
    fn response_wire_format() {
        let ok = Response::success(7, DaemonStatus::default()).to_line();
        assert!(ok.starts_with(r#"{"v":1,"id":7,"ok":true,"data":{"#), "{ok}");
        assert!(ok.ends_with('\n') && !ok.contains("\"error\""));
        let err = Response::failure(None, ErrorBody::new(ErrorKind::LineTooLong, "too long")).to_line();
        assert_eq!(err, "{\"v\":1,\"id\":null,\"ok\":false,\"error\":{\"kind\":\"line_too_long\",\"message\":\"too long\"}}\n");
    }

    #[test]
    fn response_round_trip() {
        let status = DaemonStatus {
            engaged: true,
            profile: Some(ProfileSummary {
                country: "DE".into(),
                timezone: "Europe/Berlin".into(),
                locale: "de-DE".into(),
                lat: 52.52,
                lon: 13.405,
            }),
            vpn_up: true,
            exit_ip: Some("203.0.113.7".into()),
            vpn_interfaces: vec!["wg0".into()],
            ..Default::default()
        };
        let line = Response::success(1, status.clone()).to_line();
        let back: Response = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back.data, Some(status));
    }
}
