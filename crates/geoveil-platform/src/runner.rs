//! Process execution behind a trait so backends can be tested with a fake.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    /// Exit code; -1 if killed by a signal.
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

/// Runs real processes.
///
/// A process that does not finish within [`SYSTEM_RUNNER_TIMEOUT`] (a hung
/// tool, or an authentication prompt nobody answers) is killed and reported
/// as an [`io::ErrorKind::TimedOut`] error, so a restore can never hang
/// forever behind it.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

/// Longest a [`SystemRunner`] command may run (long enough to type a
/// password into a polkit or UAC prompt).
pub const SYSTEM_RUNNER_TIMEOUT: Duration = Duration::from_secs(120);

/// How often a running child is polled for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        run_with_timeout(program, args, SYSTEM_RUNNER_TIMEOUT)
    }
}

/// Runs `program` with `args`, capturing output, killing it after `timeout`.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> io::Result<CommandOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes on threads so a chatty child cannot block on a full pipe.
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("`{program}` did not finish within {}s and was killed", timeout.as_secs()),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    Ok(CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// Reads a pipe to the end on a background thread.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Joins a drain thread and decodes its bytes (lossy UTF-8).
fn collect(handle: Option<JoinHandle<Vec<u8>>>) -> String {
    let bytes = handle.and_then(|h| h.join().ok()).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Test double: records every call and replays scripted outputs in order.
/// When the script is exhausted it returns success with empty output.
#[derive(Debug, Default)]
pub struct FakeRunner {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    script: Mutex<VecDeque<io::Result<CommandOutput>>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the output for the next call.
    pub fn push(&self, out: CommandOutput) -> &Self {
        self.script.lock().unwrap().push_back(Ok(out));
        self
    }

    /// Queue a successful call with the given stdout.
    pub fn push_stdout(&self, stdout: &str) -> &Self {
        self.push(CommandOutput { status: 0, stdout: stdout.to_string(), stderr: String::new() })
    }

    /// Queue an io error (e.g. program not found).
    pub fn push_err(&self, kind: io::ErrorKind) -> &Self {
        self.script.lock().unwrap().push_back(Err(io::Error::from(kind)));
        self
    }

    /// Every call so far, as `program arg1 arg2 ...` strings.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(p, a)| std::iter::once(p.clone()).chain(a.iter().cloned()).collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// Every call so far, structured.
    pub fn raw_calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap()
            .push((program.to_string(), args.iter().map(|s| s.to_string()).collect()));
        self.script.lock().unwrap().pop_front().unwrap_or_else(|| Ok(CommandOutput::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn echo() -> (&'static str, Vec<&'static str>) {
        ("cmd", vec!["/C", "echo hello"])
    }
    #[cfg(not(windows))]
    fn echo() -> (&'static str, Vec<&'static str>) {
        ("sh", vec!["-c", "echo hello; echo oops >&2; exit 3"])
    }

    #[cfg(windows)]
    fn slow() -> (&'static str, Vec<&'static str>) {
        ("powershell", vec!["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 20"])
    }
    #[cfg(not(windows))]
    fn slow() -> (&'static str, Vec<&'static str>) {
        ("sleep", vec!["20"])
    }

    #[test]
    fn captures_output() {
        let (program, args) = echo();
        let out = run_with_timeout(program, &args, Duration::from_secs(30)).unwrap();
        assert!(out.stdout.contains("hello"), "{out:?}");
        #[cfg(not(windows))]
        {
            assert_eq!(out.status, 3);
            assert!(out.stderr.contains("oops"));
        }
    }

    #[test]
    fn kills_hung_process() {
        let (program, args) = slow();
        let start = Instant::now();
        let err = run_with_timeout(program, &args, Duration::from_millis(500)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(15), "took {:?}", start.elapsed());
    }

    #[test]
    fn missing_program_is_not_found() {
        let err = run_with_timeout("geoveil-no-such-program-xyz", &[], Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
