//! The ports agent runs need (spec 0003, *Runtime shape*). Real ones live in
//! `sy-infra`; test doubles live beside the tests.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use sy_adapter_api::{ExitInfo, ProbeOutcome, PtySize};

use crate::ids::RunId;

/// What to spawn in a new PTY session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The program's absolute path (already resolved and allowed).
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The working directory.
    pub cwd: String,
    /// The whole environment; nothing else is inherited.
    pub env: Vec<(String, String)>,
    /// The terminal size.
    pub size: PtySize,
}

/// Why a spawn failed. The text never holds the arguments or environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not start the agent: {0}")]
pub struct PtyError(pub String);

/// A signal for the child's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSignal {
    /// SIGHUP.
    Hangup,
    /// SIGKILL.
    Kill,
    /// SIGCONT (the sweep sends it first, so a stopped process acts on SIGTERM).
    Cont,
    /// SIGTERM.
    Term,
}

/// Writing to the PTY and resizing it. Blocking: used from one thread.
pub trait PtyIo: Send {
    /// Write all of `data` to the terminal (the agent's input).
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// Resize the terminal.
    fn resize(&mut self, size: PtySize) -> io::Result<()>;
}

/// The spawned process.
pub trait ChildProcess: Send + Sync {
    /// Block until the process exits and say how it ended, without reaping
    /// it: the unreaped zombie keeps its pid, group id and session id
    /// reserved until [`ChildProcess::reap`] (spec 0004, AC-8). Called once,
    /// from its own thread.
    fn wait(&self) -> io::Result<ExitInfo>;

    /// Reap the exited process. Only the first call does anything; group
    /// signals are refused from then on. Blocking.
    fn reap(&self);

    /// Signal the process group, but only while the main process has not
    /// been reaped (so a reused group id is never hit). Returns whether it
    /// was sent.
    fn signal_group(&self, signal: GroupSignal) -> bool;
}

/// A signal for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSignal {
    /// SIGCONT.
    Cont,
    /// SIGTERM.
    Term,
    /// SIGKILL.
    Kill,
}

/// One process, as a scan of the process table sees it (spec 0004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: i64,
    /// Parent process id.
    pub ppid: Option<i64>,
    /// Session id.
    pub sid: Option<i64>,
    /// Start time, Unix ms from whole seconds.
    pub started_at_ms: i64,
    /// Owned by the engine's real uid.
    pub uid_is_self: bool,
    /// Exited but not reaped.
    pub zombie: bool,
    /// The short executable name, never arguments.
    pub name: String,
}

/// A single process, checked directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcState {
    /// Start time, Unix ms from whole seconds.
    pub started_at_ms: i64,
    /// Exited but not reaped.
    pub zombie: bool,
}

/// What a per pid signal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// Sent to the recorded process.
    Sent,
    /// The recorded process is gone (or failed the safety check), so nothing was sent.
    Gone,
}

/// Why a scan of the process table failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not scan the process table: {0}")]
pub struct ProcError(pub String);

/// Process facts and per process signals (spec 0004). Blocking: called from
/// blocking threads. Never reads command lines or environments.
pub trait ProcessTable: Send + Sync {
    /// Every process.
    fn scan(&self) -> Result<Vec<ProcInfo>, ProcError>;

    /// One process, checked directly. `None` when it is absent.
    fn started_at(&self, pid: i64) -> Option<ProcState>;

    /// Send `signal` to `pid` only if, checked right before sending, it
    /// exists, started at `started_at_ms`, belongs to the engine's real uid,
    /// is greater than 1, and is not the engine itself (AC-9).
    fn signal(&self, pid: i64, started_at_ms: i64, signal: ProcSignal) -> SignalOutcome;

    /// Signal the process group `pgid` (restart cleanup, only while its
    /// recorded main process still matches). Returns whether it was sent.
    fn signal_group(&self, pgid: i64, signal: ProcSignal) -> bool;

    /// The engine's own pid.
    fn engine_pid(&self) -> i64;
}

/// A process running in its own PTY session.
pub struct PtyChild {
    /// Its process id.
    pub pid: i64,
    /// Its process group id (equal to `pid`: a new session).
    pub pgid: i64,
    /// The terminal's output. Blocking; read from its own thread.
    pub reader: Box<dyn io::Read + Send>,
    /// The terminal's input and size.
    pub io: Box<dyn PtyIo>,
    /// The process.
    pub process: Arc<dyn ChildProcess>,
}

/// Spawns programs in PTYs.
pub trait Pty: Send + Sync {
    /// Spawn `req` in a new PTY session with that PTY as its controlling terminal.
    fn spawn(&self, req: SpawnRequest) -> Result<PtyChild, PtyError>;
}

/// The per run launch dirs, `<data dir>/agent-runtime/<run_id>/`. Blocking.
pub trait RuntimeDir: Send + Sync {
    /// `<data dir>/agent-runtime/` itself: the working directory of detection probes.
    fn root(&self) -> String;

    /// The launch dir's path for a run.
    fn path(&self, run: &RunId) -> String;

    /// Create the run's launch dir, `0700`. Returns its path.
    fn create(&self, run: &RunId) -> io::Result<String>;

    /// Write a file in the launch dir, `0600`, created new (never through a
    /// symlink). `rel_path` was already checked by [`super::launch::valid_rel_path`].
    fn write_file(&self, run: &RunId, rel_path: &str, contents: &[u8]) -> io::Result<()>;

    /// Remove the run's launch dir and everything in it. A missing dir is fine.
    fn remove(&self, run: &RunId) -> io::Result<()>;

    /// Remove every launch dir (at engine start). Returns how many.
    fn sweep(&self) -> io::Result<usize>;
}

/// A captured login environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginEnvSnapshot {
    /// Every variable, as the login shell has it.
    pub vars: Vec<(String, String)>,
    /// Set when the capture failed and a fallback `PATH` is used, with why.
    pub fallback: Option<String>,
}

impl LoginEnvSnapshot {
    /// The value of `name`, if set.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The environment agents and probes run with (spec 0001, *Login environment*).
#[async_trait::async_trait]
pub trait LoginEnv: Send + Sync {
    /// The current capture. Waits for the first capture to finish.
    async fn snapshot(&self) -> Arc<LoginEnvSnapshot>;

    /// Capture again (`env.refresh`) and return the new capture. Only later
    /// launches and probes see it; live runs keep the one they started with.
    async fn refresh(&self) -> Arc<LoginEnvSnapshot>;

    /// The current capture, or `None` until the first one finishes.
    fn current(&self) -> Option<Arc<LoginEnvSnapshot>>;

    /// Find a bare program name on `env`'s `PATH`: the first executable
    /// file. Blocking (it looks at the file system).
    fn resolve(&self, env: &LoginEnvSnapshot, program: &str) -> Option<String>;
}

/// One detection probe to run (spec 0003, AC-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// The program's absolute path (already resolved and allowed).
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The working directory (`<data dir>/agent-runtime/`).
    pub cwd: String,
    /// The whole environment; nothing else is inherited.
    pub env: Vec<(String, String)>,
    /// Kill it (and its process group) after this long.
    pub timeout: Duration,
}

/// How a probe went, with what it wrote (each capped at [`PROBE_OUTPUT_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRun {
    /// `Exited`, `TimedOut` or `SpawnError`.
    pub outcome: ProbeOutcome,
    /// Its stdout.
    pub stdout: Vec<u8>,
    /// Its stderr.
    pub stderr: Vec<u8>,
}

/// Most bytes of a probe's stdout, and of its stderr, that are kept.
pub const PROBE_OUTPUT_BYTES: usize = 64 * 1024;

/// Runs detection probes: no TTY, stdin null, output capped. Blocking.
pub trait ProcessRunner: Send + Sync {
    /// Run one probe to its end or its timeout.
    fn run(&self, req: ProbeRequest) -> ProbeRun;
}

/// Mints signal tokens: 128 random bits, lowercase hex.
pub trait TokenSource: Send + Sync {
    /// A new token.
    fn mint(&self) -> String;
}
