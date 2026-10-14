//! Process execution behind a trait so backends can be tested with a fake.

use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;

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
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let out = std::process::Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
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
