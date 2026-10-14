//! Transport: line framing over any async stream, the per-user endpoint, the
//! server accept loop and the client.
//!
//! The endpoint is a named pipe on Windows and a Unix domain socket elsewhere
//! (through `interprocess`), restricted to the current user:
//!
//! - **Windows:** `\\.\pipe\geoveil-<user SID>`, created with a protected
//!   DACL that grants access to the current user only, as the first pipe
//!   instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`, so a squatter's pipe makes the
//!   daemon fail instead of sharing the name) and with
//!   `PIPE_REJECT_REMOTE_CLIENTS` (interprocess sets both).
//! - **Unix:** `$XDG_RUNTIME_DIR/geoveil.sock` on Linux, otherwise
//!   `<local data dir>/geoveil/geoveil.sock`, created with mode 0600 in a
//!   directory only the user can enter.
//!
//! Both sides also check the peer: the server refuses a client, and the
//! client refuses a server, that runs as another user (see
//! [`crate::peer`]).

use crate::peer;
use crate::protocol::{
    handle_line, Command, DaemonStatus, ErrorBody, ErrorKind, Handler, Request, Response, MAX_LINE,
    PROTOCOL_VERSION,
};
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{ListenerOptions, Name};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// How long a client waits for a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Errors seen by a client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing listens on the endpoint (no daemon running).
    #[error("no geoveil daemon is listening ({0})")]
    NotRunning(String),
    /// The daemon answered with an error.
    #[error("{}", .0.message)]
    Daemon(ErrorBody),
    /// The server runs as another user.
    #[error("refusing to talk to the daemon endpoint: {0}")]
    Untrusted(String),
    /// I/O error or timeout.
    #[error("IPC error: {0}")]
    Io(#[from] io::Error),
    /// The server sent something that is not a valid response.
    #[error("invalid response from the daemon: {0}")]
    Protocol(String),
}

/// Result of reading one line.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A complete line, newline stripped.
    Text(String),
    /// The line exceeded [`MAX_LINE`].
    TooLong,
    /// The line is not valid UTF-8.
    NotUtf8,
    /// End of stream.
    Eof,
}

/// Reads one `\n`-terminated line of at most [`MAX_LINE`] bytes.
pub async fn read_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Line> {
    let mut buf = Vec::new();
    let limit = MAX_LINE as u64 + 1;
    let n = (&mut *reader).take(limit).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() > MAX_LINE {
        return Ok(Line::TooLong);
    }
    // A final line without a newline at EOF is still accepted.
    match String::from_utf8(buf) {
        Ok(s) => Ok(Line::Text(s)),
        Err(_) => Ok(Line::NotUtf8),
    }
}

/// Serves one connection until the peer closes it: one response per request
/// line. A line that is too long ends the connection after the error reply.
pub async fn serve_connection<S, H>(stream: S, handler: &H) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    H: Handler + ?Sized,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    loop {
        let response = match read_line(&mut reader).await? {
            Line::Eof => return Ok(()),
            Line::Text(text) if text.trim().is_empty() => continue,
            Line::Text(text) => handle_line(&text, handler).await,
            Line::NotUtf8 => Response::failure(None, ErrorBody::new(ErrorKind::BadRequest, "request is not UTF-8")),
            Line::TooLong => {
                let r = Response::failure(
                    None,
                    ErrorBody::new(ErrorKind::LineTooLong, format!("request longer than {MAX_LINE} bytes")),
                );
                write.write_all(r.to_line().as_bytes()).await?;
                write.flush().await?;
                return Ok(());
            }
        };
        write.write_all(response.to_line().as_bytes()).await?;
        write.flush().await?;
    }
}

/// The per-user endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Windows named pipe name (without the `\\.\pipe\` prefix).
    Pipe(String),
    /// Unix socket path.
    Socket(PathBuf),
}

impl Endpoint {
    /// The endpoint for the current user.
    pub fn for_current_user() -> io::Result<Self> {
        #[cfg(windows)]
        {
            let sid = peer::current_user_sid()?;
            Ok(Endpoint::Pipe(pipe_name_for(&sid)))
        }
        #[cfg(not(windows))]
        {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
            socket_path_for(runtime, dirs::data_local_dir())
                .map(Endpoint::Socket)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no runtime or data directory for the IPC socket"))
        }
    }

    /// Human-readable form for logs.
    pub fn display(&self) -> String {
        match self {
            Endpoint::Pipe(name) => format!(r"\\.\pipe\{name}"),
            Endpoint::Socket(path) => path.display().to_string(),
        }
    }

    fn name(&self) -> io::Result<Name<'static>> {
        match self {
            Endpoint::Pipe(name) => {
                use interprocess::local_socket::GenericNamespaced;
                name.clone().to_ns_name::<GenericNamespaced>().map(Name::into_owned)
            }
            Endpoint::Socket(path) => {
                use interprocess::local_socket::GenericFilePath;
                path.clone().to_fs_name::<GenericFilePath>().map(Name::into_owned)
            }
        }
    }
}

/// Pipe name for a user SID.
pub fn pipe_name_for(sid: &str) -> String {
    format!("geoveil-{sid}")
}

/// Socket path: inside `$XDG_RUNTIME_DIR` when it is set to an absolute path
/// (Linux; the directory is private to the user), otherwise inside the
/// geoveil data directory.
pub fn socket_path_for(runtime_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Option<PathBuf> {
    match runtime_dir.filter(|d| d.is_absolute()) {
        Some(dir) => Some(dir.join("geoveil.sock")),
        None => data_dir.map(|d| d.join("geoveil").join("geoveil.sock")),
    }
}

/// SDDL for the pipe: protected DACL, generic-all for the user SID only.
pub fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

/// Binds the endpoint for the server. Fails when another live daemon owns it.
pub fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
    let name = endpoint.name()?;
    #[allow(unused_mut)]
    let mut opts = ListenerOptions::new().name(name);
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sid = peer::current_user_sid()?;
        let sddl = widestring::U16CString::from_str(pipe_sddl(&sid))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        opts = opts.security_descriptor(SecurityDescriptor::deserialize(&sddl)?);
    }
    #[cfg(unix)]
    {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        if let Endpoint::Socket(path) = endpoint {
            prepare_socket_dir(path)?;
        }
        opts = opts.mode(0o600);
    }
    match opts.create_tokio() {
        Ok(l) => Ok(l),
        #[cfg(unix)]
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // A stale socket file from a crashed daemon, or a live daemon.
            // The caller checked that nothing answers; remove and retry once.
            if let Endpoint::Socket(path) = endpoint {
                remove_stale_socket(path)?;
            }
            let name = endpoint.name()?;
            use interprocess::os::unix::local_socket::ListenerOptionsExt;
            ListenerOptions::new().name(name).mode(0o600).create_tokio()
        }
        Err(e) => Err(e),
    }
}

/// Creates the socket's directory (mode 0700 when we create it).
#[cfg(unix)]
fn prepare_socket_dir(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
    }
    Ok(())
}

/// Removes a stale socket file we own; refuses anything else.
#[cfg(unix)]
fn remove_stale_socket(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() || meta.uid() != peer::current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} exists and is not our socket; remove it by hand", path.display()),
        ));
    }
    std::fs::remove_file(path)
}

/// Accepts connections until `shutdown` resolves. Each connection is served
/// on its own task; clients running as another user are dropped.
pub async fn serve<H, F>(listener: Listener, handler: Arc<H>, shutdown: F)
where
    H: Handler + 'static,
    F: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        let stream = tokio::select! {
            _ = &mut shutdown => return,
            accepted = listener.accept() => match accepted {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("IPC accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            },
        };
        if let Err(why) = peer::check_peer(&stream) {
            tracing::warn!("IPC: refused a client: {why}");
            continue;
        }
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(e) = serve_connection(stream, &*handler).await {
                tracing::debug!("IPC connection ended: {e}");
            }
        });
    }
}

/// A connected client.
pub struct Client {
    reader: BufReader<tokio::io::ReadHalf<Stream>>,
    writer: tokio::io::WriteHalf<Stream>,
    next_id: u64,
}

impl Client {
    /// Connects to the current user's daemon and checks that it runs as the
    /// same user.
    pub async fn connect() -> Result<Self, ClientError> {
        let endpoint = Endpoint::for_current_user()?;
        Self::connect_to(&endpoint).await
    }

    /// Connects to `endpoint`.
    pub async fn connect_to(endpoint: &Endpoint) -> Result<Self, ClientError> {
        let name = endpoint.name()?;
        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
            Err(_) => return Err(ClientError::NotRunning(format!("{} did not accept in time", endpoint.display()))),
            Ok(Err(e)) if is_not_running(&e) => return Err(ClientError::NotRunning(endpoint.display())),
            Ok(Err(e)) => return Err(ClientError::Io(e)),
            Ok(Ok(s)) => s,
        };
        peer::check_peer(&stream).map_err(ClientError::Untrusted)?;
        let (read, writer) = tokio::io::split(stream);
        Ok(Self { reader: BufReader::new(read), writer, next_id: 1 })
    }

    /// Sends `cmd` and waits up to `timeout` for the answer.
    pub async fn request(&mut self, cmd: Command, timeout: Duration) -> Result<DaemonStatus, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = Request::new(id, cmd).to_line();
        let exchange = async {
            self.writer.write_all(line.as_bytes()).await?;
            self.writer.flush().await?;
            read_line(&mut self.reader).await
        };
        let reply = tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| ClientError::Io(io::Error::new(io::ErrorKind::TimedOut, "the daemon did not answer in time")))??;
        let text = match reply {
            Line::Text(t) => t,
            Line::Eof => return Err(ClientError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
            other => return Err(ClientError::Protocol(format!("{other:?}"))),
        };
        parse_response(&text, id)
    }
}

/// Validates a response line for request `id`.
pub fn parse_response(text: &str, id: u64) -> Result<DaemonStatus, ClientError> {
    let r: Response = serde_json::from_str(text).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if r.v != PROTOCOL_VERSION {
        return Err(ClientError::Protocol(format!("daemon speaks protocol version {}", r.v)));
    }
    if r.ok {
        if r.id != Some(id) {
            return Err(ClientError::Protocol(format!("response id {:?} does not match request {id}", r.id)));
        }
        r.data.ok_or_else(|| ClientError::Protocol("success without data".into()))
    } else {
        Err(ClientError::Daemon(
            r.error.unwrap_or_else(|| ErrorBody::new(ErrorKind::Internal, "error without details")),
        ))
    }
}

/// Whether a connect error means "nobody listens".
fn is_not_running(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::AddrNotAvailable)
}

/// Whether a daemon answers on `endpoint` right now.
pub async fn is_listening(endpoint: &Endpoint) -> bool {
    match Client::connect_to(endpoint).await {
        Ok(mut c) => c.request(Command::Status, CONNECT_TIMEOUT).await.is_ok(),
        Err(ClientError::NotRunning(_)) => false,
        // Something is there (maybe someone else's): treat it as taken.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::FakeHandler;
    use tokio::io::{duplex, AsyncWriteExt};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// Runs `serve_connection` over an in-memory pipe, sends `input`, and
    /// returns everything the server wrote.
    fn exchange(input: Vec<u8>) -> String {
        rt().block_on(async move {
            let (client, server) = duplex(256 * 1024);
            let handler = FakeHandler::default();
            let srv = async { serve_connection(server, &handler).await.unwrap() };
            let cli = async move {
                let (mut r, mut w) = tokio::io::split(client);
                w.write_all(&input).await.unwrap();
                w.shutdown().await.unwrap();
                let mut out = String::new();
                r.read_to_string(&mut out).await.unwrap();
                out
            };
            let ((), out) = tokio::join!(srv, cli);
            out
        })
    }

    fn responses(out: &str) -> Vec<Response> {
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn one_response_per_request_in_order() {
        let out = exchange(b"{\"v\":1,\"id\":1,\"cmd\":\"status\"}\n\n{\"v\":1,\"id\":2,\"cmd\":\"pause\"}\r\n".to_vec());
        let r = responses(&out);
        assert_eq!(r.len(), 2, "{out}");
        assert_eq!((r[0].id, r[0].ok), (Some(1), true));
        assert_eq!((r[1].id, r[1].ok), (Some(2), true));
        assert!(r[1].data.as_ref().unwrap().paused);
    }

    #[test]
    fn last_line_without_newline_is_served() {
        let r = responses(&exchange(b"{\"v\":1,\"id\":5,\"cmd\":\"status\"}".to_vec()));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, Some(5));
    }

    #[test]
    fn bad_line_does_not_end_the_connection() {
        let r = responses(&exchange(b"garbage\n{\"v\":1,\"id\":2,\"cmd\":\"status\"}\n".to_vec()));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].error.as_ref().unwrap().kind, ErrorKind::BadRequest);
        assert!(r[1].ok);
    }

    #[test]
    fn non_utf8_is_refused() {
        let r = responses(&exchange(b"\xff\xfe\n".to_vec()));
        assert_eq!(r[0].error.as_ref().unwrap().kind, ErrorKind::BadRequest);
    }

    #[test]
    fn overlong_line_is_refused_and_closes() {
        let mut input = vec![b'a'; MAX_LINE + 10];
        input.extend_from_slice(b"\n{\"v\":1,\"id\":2,\"cmd\":\"status\"}\n");
        let r = responses(&exchange(input));
        assert_eq!(r.len(), 1, "connection must close after the error");
        assert_eq!(r[0].error.as_ref().unwrap().kind, ErrorKind::LineTooLong);
    }

    #[test]
    fn line_of_exactly_max_length_is_read() {
        rt().block_on(async {
            let mut data = vec![b'x'; MAX_LINE];
            data.push(b'\n');
            let mut reader = BufReader::new(&data[..]);
            match read_line(&mut reader).await.unwrap() {
                Line::Text(t) => assert_eq!(t.len(), MAX_LINE),
                other => panic!("{other:?}"),
            }
            assert_eq!(read_line(&mut reader).await.unwrap(), Line::Eof);
        });
    }

    #[test]
    fn client_response_validation() {
        let ok = Response::success(3, DaemonStatus { pid: 9, ..Default::default() }).to_line();
        assert_eq!(parse_response(ok.trim(), 3).unwrap().pid, 9);
        assert!(matches!(parse_response(ok.trim(), 4), Err(ClientError::Protocol(_))));
        let err = Response::failure(Some(3), ErrorBody::new(ErrorKind::Failed, "nope")).to_line();
        match parse_response(err.trim(), 3) {
            Err(ClientError::Daemon(e)) => assert_eq!(e.kind, ErrorKind::Failed),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_response("{}", 1), Err(ClientError::Protocol(_))));
        let v2 = ok.replace("\"v\":1", "\"v\":2");
        assert!(matches!(parse_response(v2.trim(), 3), Err(ClientError::Protocol(_))));
    }

    /// A private endpoint for this test process (never the user's real one).
    fn test_endpoint(tag: &str) -> Endpoint {
        if cfg!(windows) {
            Endpoint::Pipe(format!("geoveil-test-{tag}-{}", std::process::id()))
        } else {
            Endpoint::Socket(std::env::temp_dir().join(format!("gv-{tag}-{}", std::process::id())).join("s.sock"))
        }
    }

    #[test]
    fn real_endpoint_round_trip_and_single_owner() {
        rt().block_on(async {
            let endpoint = test_endpoint("rt");
            assert!(!is_listening(&endpoint).await);
            let listener = bind(&endpoint).unwrap();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(serve(listener, Arc::new(FakeHandler::default()), async {
                let _ = stop_rx.await;
            }));

            let mut client = Client::connect_to(&endpoint).await.unwrap();
            let status = client.request(Command::Pause, Duration::from_secs(5)).await.unwrap();
            assert!(status.paused);
            assert_eq!(client.request(Command::Status, Duration::from_secs(5)).await.unwrap().pid, 42);
            assert!(is_listening(&endpoint).await);

            // Windows: a second server on the same name is refused
            // (FILE_FLAG_FIRST_PIPE_INSTANCE).
            #[cfg(windows)]
            assert!(bind(&endpoint).is_err());

            let _ = stop_tx.send(());
            server.await.unwrap();
            #[cfg(unix)]
            if let Endpoint::Socket(p) = &endpoint {
                let _ = std::fs::remove_dir_all(p.parent().unwrap());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn socket_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        rt().block_on(async {
            let endpoint = test_endpoint("mode");
            let _listener = bind(&endpoint).unwrap();
            let Endpoint::Socket(path) = &endpoint else { unreachable!() };
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o077, 0, "socket mode {mode:o} must not grant group/other access");
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        });
    }

    #[test]
    fn endpoint_naming() {
        assert_eq!(pipe_name_for("S-1-5-21-1-2-3-1001"), "geoveil-S-1-5-21-1-2-3-1001");
        assert_eq!(pipe_sddl("S-1-5-21-1-2-3-1001"), "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        assert_eq!(Endpoint::Pipe("geoveil-S-1".into()).display(), r"\\.\pipe\geoveil-S-1");
    }

    #[test]
    fn socket_path_prefers_absolute_runtime_dir() {
        // Absolute on every OS.
        let runtime = std::env::temp_dir().join("run-user-1000");
        let data = std::env::temp_dir().join("share");
        let in_data = data.join("geoveil").join("geoveil.sock");
        assert_eq!(socket_path_for(Some(runtime.clone()), Some(data.clone())), Some(runtime.join("geoveil.sock")));
        // A relative XDG_RUNTIME_DIR is invalid per the spec and ignored.
        assert_eq!(socket_path_for(Some(PathBuf::from("relative")), Some(data.clone())), Some(in_data.clone()));
        assert_eq!(socket_path_for(None, Some(data)), Some(in_data));
        assert_eq!(socket_path_for(None, None), None);
    }
}
