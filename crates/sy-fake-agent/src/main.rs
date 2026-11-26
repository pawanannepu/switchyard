//! `sy-fake-agent`: a scripted fake coding agent (spec 0003, *Fake agent*).
//!
//! `sy-fake-agent --script <path> --signal <helper> --socket <socket> --token <token>`
//!
//! It runs the JSON steps in the script file one by one, in a real PTY, and
//! reaches the engine only by running `sy-signal`, as a real agent's hook
//! does: the `status`, `session` and `event` steps each run it once, with
//! the step's value as JSON on its stdin, and wait for it to exit. An
//! `approval` step blocks on its reply; if the engine can't answer (the
//! fallback `{"decision":"ask"}`, or no reply at all) it asks in its own
//! terminal, `Allow <summary>? [y/n] `, and never goes ahead unasked. Like a real TUI it puts its terminal in raw mode, so Ctrl C arrives
//! as a byte: it exits with code 130 on Ctrl C unless a `hang` step is
//! ignoring input. Dev and test only; release bundles never contain it.
//!
//! The step shape is a copy of `sy-adapter-fake`'s (this crate depends on
//! no workspace crate), so a change there comes here too.

use std::io::{Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;

/// Ctrl C, as raw mode delivers it.
const INTERRUPT: u8 = 0x03;
/// The exit code after Ctrl C (128 + SIGINT, like a shell reports it).
const INTERRUPTED: i32 = 130;
const FLOOD_TICK: Duration = Duration::from_millis(16);
const FLOOD_LINE: usize = 78;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    Print(String),
    PrintB64(String),
    Status(serde_json::Value),
    Session(serde_json::Value),
    Event(serde_json::Value),
    Approval(Approval),
    ExpectInput(String),
    EchoUntil(String),
    Flood(Flood),
    WriteFile(WriteFile),
    DeleteFile(DeleteFile),
    Hang(bool),
    IgnoreSighup(bool),
    DieBySignal(Signal),
    Exit(i32),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    kind: String,
    summary: String,
    #[serde(default)]
    detail: serde_json::Value,
    #[serde(default)]
    on_approve: Vec<Step>,
    #[serde(default)]
    on_deny: Vec<Step>,
}

/// The engine's reply to an `approval` call.
#[derive(Debug, Deserialize)]
struct Answer {
    decision: String,
}

/// The `approval` hook's fallback stdout (as in `sy-adapter-fake`), base64.
const ASK_B64: &str = "eyJkZWNpc2lvbiI6ImFzayJ9";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Flood {
    bytes_per_sec: u64,
    #[serde(default)]
    total_bytes: Option<u64>,
    #[serde(default)]
    echo: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFile {
    path: String,
    contents: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteFile {
    path: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum Signal {
    #[serde(rename = "SIGSEGV")]
    Segv,
    #[serde(rename = "SIGKILL")]
    Kill,
    #[serde(rename = "SIGTERM")]
    Term,
}

/// The command line.
struct Args {
    script: String,
    hooks: Hooks,
}

/// How hook steps reach the engine.
struct Hooks {
    helper: String,
    socket: String,
    token: String,
}

fn parse_args() -> Result<Args, String> {
    let (mut script, mut helper, mut socket, mut token) = (None, None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--script" => script = Some(value),
            "--signal" => helper = Some(value),
            "--socket" => socket = Some(value),
            "--token" => token = Some(value),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        script: script.ok_or("--script is required")?,
        hooks: Hooks {
            helper: helper.ok_or("--signal is required")?,
            socket: socket.ok_or("--socket is required")?,
            token: token.ok_or("--token is required")?,
        },
    })
}

impl Hooks {
    /// Run `sy-signal` for one hook with `payload` as JSON on its stdin, and
    /// return its stdout once it exits. The argv is `hook_command`'s (from
    /// `sy-adapter-api`), with the fallback the fake adapter expects for
    /// these hooks: exit 0, nothing on stdout.
    fn call(&self, hook: &str, payload: &serde_json::Value) -> Result<Vec<u8>, String> {
        self.call_with(hook, payload, "")
    }

    /// [`Hooks::call`] with this fallback stdout (base64).
    fn call_with(
        &self,
        hook: &str,
        payload: &serde_json::Value,
        fallback_b64: &str,
    ) -> Result<Vec<u8>, String> {
        let stdin = serde_json::to_vec(payload).map_err(|e| format!("{hook}: {e}"))?;
        let mut child = Command::new(&self.helper)
            .args(["--socket", &self.socket, "--token", &self.token])
            .args(["--hook", hook, "--fallback-exit", "0"])
            .args(["--fallback-stdout-b64", fallback_b64])
            .arg("--")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("running sy-signal: {e}"))?;
        if let Some(mut pipe) = child.stdin.take() {
            // The helper may stop reading at its cap; it answers anyway.
            let _ = pipe.write_all(&stdin);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("waiting for sy-signal: {e}"))?;
        Ok(out.stdout)
    }
}

/// Terminal input, read on its own thread so Ctrl C is seen whatever step runs.
struct Input {
    bytes: Receiver<Vec<u8>>,
    /// Set by `hang`: input (Ctrl C included) is read and thrown away.
    ignore: Arc<AtomicBool>,
}

impl Input {
    fn start() -> Self {
        let (tx, bytes) = std::sync::mpsc::channel();
        let ignore = Arc::new(AtomicBool::new(false));
        let ignoring = Arc::clone(&ignore);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0_u8; 4096];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if ignoring.load(Ordering::SeqCst) {
                    continue;
                }
                if buf[..n].contains(&INTERRUPT) {
                    let _ = std::io::stdout().flush();
                    std::process::exit(INTERRUPTED);
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self { bytes, ignore }
    }

    /// The next input chunk, or `None` when the terminal closed.
    fn next(&self) -> Option<Vec<u8>> {
        self.bytes.recv().ok()
    }
}

fn out(bytes: &[u8]) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("writing output: {e}"))
}

/// Raw mode, as a TUI uses it: no line editing, no echo, Ctrl C as a byte.
fn raw_mode() {
    let stdin = std::io::stdin();
    if let Ok(mut termios) = rustix::termios::tcgetattr(&stdin) {
        termios.make_raw();
        let _ = rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &termios);
    }
}

fn inside_cwd(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|p| p == "..") {
        return Err(format!("path {path:?} must be relative with no `..`"));
    }
    Ok(())
}

fn expect_input(input: &Input, literal: &str) -> Result<(), String> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let chunk = input.next().ok_or("the terminal closed")?;
        for byte in chunk {
            if byte == b'\r' || byte == b'\n' {
                line.clear();
                continue;
            }
            line.push(byte);
            if String::from_utf8_lossy(&line).contains(literal) {
                return Ok(());
            }
        }
    }
}

/// Read one line of terminal input (to `\r` or `\n`).
fn read_line(input: &Input) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let chunk = input.next().ok_or("the terminal closed")?;
        for byte in chunk {
            if byte == b'\r' || byte == b'\n' {
                return Ok(String::from_utf8_lossy(&line).trim().to_owned());
            }
            line.push(byte);
        }
    }
}

/// Ask the engine, then, if it couldn't answer, ask in the terminal.
/// True means approved.
fn approval(input: &Input, hooks: &Hooks, a: &Approval) -> Result<bool, String> {
    let ask = serde_json::json!({ "kind": a.kind, "summary": a.summary, "detail": a.detail });
    let reply = hooks.call_with("approval", &ask, ASK_B64)?;
    match serde_json::from_slice::<Answer>(&reply).map(|r| r.decision) {
        Ok(decision) if decision == "approve" => Ok(true),
        Ok(decision) if decision == "deny" => Ok(false),
        // "ask", or no usable reply at all: never go ahead unasked.
        _ => {
            out(format!("Allow {}? [y/n] ", a.summary).as_bytes())?;
            let answer = read_line(input)?;
            out(b"\r\n")?;
            Ok(answer.eq_ignore_ascii_case("y"))
        }
    }
}

fn echo_until(input: &Input, literal: &str) -> Result<(), String> {
    let literal = literal.as_bytes();
    let mut seen: Vec<u8> = Vec::new();
    loop {
        let chunk = input.next().ok_or("the terminal closed")?;
        out(&chunk)?;
        seen.extend_from_slice(&chunk);
        if !literal.is_empty() && seen.windows(literal.len()).any(|w| w == literal) {
            return Ok(());
        }
        // Keep only enough to match a literal split across chunks.
        let keep = literal.len().saturating_sub(1);
        if seen.len() > keep {
            seen.drain(..seen.len() - keep);
        }
    }
}

/// Bytes in the seeded pattern `flood` cycles through.
const PATTERN_BYTES: usize = 64 * 1024;

/// Seeded printable text in lines: the same bytes every run, made once and
/// then cycled, so a fast flood costs a copy per tick.
struct Filler {
    pattern: Vec<u8>,
    at: usize,
}

impl Filler {
    fn new() -> Self {
        let mut state: u64 = 0x5157_4954_4348;
        let mut pattern = Vec::with_capacity(PATTERN_BYTES);
        let mut column = 0;
        while pattern.len() < PATTERN_BYTES {
            if column == FLOOD_LINE {
                pattern.extend_from_slice(b"\r\n");
                column = 0;
                continue;
            }
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            pattern.push(b' ' + u8::try_from((state >> 33) % 95).unwrap_or(0));
            column += 1;
        }
        pattern.truncate(PATTERN_BYTES);
        Self { pattern, at: 0 }
    }

    fn next_bytes(&mut self, n: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(n);
        while bytes.len() < n {
            let take = (n - bytes.len()).min(self.pattern.len() - self.at);
            bytes.extend_from_slice(&self.pattern[self.at..self.at + take]);
            self.at = (self.at + take) % self.pattern.len();
        }
        bytes
    }
}

fn flood(input: &Input, f: &Flood) -> Result<(), String> {
    let mut filler = Filler::new();
    let started = Instant::now();
    let mut written: u64 = 0;
    let mut next_tick = started + FLOOD_TICK;
    loop {
        // Echo input until the next tick is due.
        let wait = next_tick.saturating_duration_since(Instant::now());
        match input.bytes.recv_timeout(wait) {
            Ok(chunk) => {
                if f.echo {
                    out(&chunk)?;
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
        }
        next_tick += FLOOD_TICK;
        let due = u64::try_from(started.elapsed().as_micros())
            .unwrap_or(u64::MAX)
            .saturating_mul(f.bytes_per_sec)
            / 1_000_000;
        let due = f.total_bytes.map_or(due, |total| due.min(total));
        if due > written {
            let n = usize::try_from(due - written).unwrap_or(usize::MAX);
            out(&filler.next_bytes(n))?;
            written = due;
        }
        if f.total_bytes.is_some_and(|total| written >= total) {
            return Ok(());
        }
    }
}

fn die_by(signal: Signal) -> Result<(), String> {
    let number = match signal {
        Signal::Segv => signal_hook::consts::SIGSEGV,
        Signal::Kill => signal_hook::consts::SIGKILL,
        Signal::Term => signal_hook::consts::SIGTERM,
    };
    // Restores the default action first: Rust's own SIGSEGV handler would
    // otherwise swallow a signal that wasn't a real fault.
    signal_hook::low_level::emulate_default_handler(number)
        .map_err(|e| format!("raising signal {number}: {e}"))?;
    Err(format!("signal {number} did not end the process"))
}

fn run_step(input: &Input, hooks: &Hooks, step: &Step) -> Result<Option<i32>, String> {
    match step {
        Step::Print(text) => out(text.as_bytes())?,
        Step::PrintB64(text) => out(&base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|e| format!("print_b64: {e}"))?)?,
        Step::Status(value) => {
            hooks.call("status", value)?;
        }
        Step::Session(value) => {
            hooks.call("session", value)?;
        }
        Step::Event(value) => {
            hooks.call("event", value)?;
        }
        Step::Approval(a) => {
            let branch = if approval(input, hooks, a)? {
                &a.on_approve
            } else {
                &a.on_deny
            };
            return run_steps(input, hooks, branch);
        }
        Step::ExpectInput(literal) => expect_input(input, literal)?,
        Step::EchoUntil(literal) => echo_until(input, literal)?,
        Step::Flood(f) => flood(input, f)?,
        Step::WriteFile(w) => {
            inside_cwd(&w.path)?;
            std::fs::write(&w.path, &w.contents).map_err(|e| format!("write_file: {e}"))?;
        }
        Step::DeleteFile(d) => {
            inside_cwd(&d.path)?;
            std::fs::remove_file(&d.path).map_err(|e| format!("delete_file: {e}"))?;
        }
        Step::Hang(false) | Step::IgnoreSighup(false) => {
            return Err("hang and ignore_sighup take only true".to_owned());
        }
        Step::Hang(true) => {
            input.ignore.store(true, Ordering::SeqCst);
            loop {
                std::thread::park();
            }
        }
        Step::IgnoreSighup(true) => {
            signal_hook::flag::register(
                signal_hook::consts::SIGHUP,
                Arc::new(AtomicBool::new(false)),
            )
            .map_err(|e| format!("ignore_sighup: {e}"))?;
        }
        Step::DieBySignal(signal) => die_by(*signal)?,
        Step::Exit(code) => return Ok(Some(*code)),
    }
    Ok(None)
}

fn run() -> Result<i32, String> {
    let args = parse_args()?;
    let text = std::fs::read(&args.script).map_err(|e| format!("reading the script: {e}"))?;
    let steps: Vec<Step> =
        serde_json::from_slice(&text).map_err(|e| format!("parsing the script: {e}"))?;
    raw_mode();
    let input = Input::start();
    Ok(run_steps(&input, &args.hooks, &steps)?.unwrap_or(0))
}

/// Run steps in order. `Some(code)` when one of them exits.
fn run_steps(input: &Input, hooks: &Hooks, steps: &[Step]) -> Result<Option<i32>, String> {
    for step in steps {
        if let Some(code) = run_step(input, hooks, step)? {
            return Ok(Some(code));
        }
    }
    Ok(None)
}

fn main() -> ExitCode {
    let code = run().unwrap_or_else(|err| {
        let _ = out(format!("sy-fake-agent: {err}\r\n").as_bytes());
        1
    });
    let _ = std::io::stdout().flush();
    // `ExitCode` only carries a u8; exit with the script's full code.
    std::process::exit(code)
}
