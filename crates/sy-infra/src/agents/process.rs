//! Short lived helper processes: detection probes and the login shell
//! capture (spec 0003, AC-3 and AC-16). Each runs from an argument array,
//! with stdin null, no TTY, in its own process group, with its output capped
//! and a timeout that kills the whole group.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use sy_adapter_api::{ExitInfo, ProbeOutcome};
use sy_core::agents::ports::{PROBE_OUTPUT_BYTES, ProbeRequest, ProbeRun, ProcessRunner};

/// How often the wait loop checks for the exit.
const POLL: Duration = Duration::from_millis(10);
/// After the exit, how long to wait for the output pipes to close (a
/// grandchild may hold them open).
const PIPE_GRACE: Duration = Duration::from_millis(200);

/// What a capped run produced.
pub(crate) struct Captured {
    pub outcome: ProbeOutcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Read a pipe to its end, keeping at most `cap` bytes (the rest is read
/// and dropped, so the child never blocks on a full pipe).
fn drain(mut pipe: impl Read + Send + 'static, cap: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

fn kill_group(child: &Child) {
    if let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

/// Run `cmd` with stdin null in a new process group; kill the group after
/// `timeout`. Stdout and stderr keep at most `cap` bytes each.
pub(crate) fn run_capped(mut cmd: Command, timeout: Duration, cap: usize) -> Captured {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Captured {
                outcome: ProbeOutcome::SpawnError(err.to_string()),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
        }
    };
    let stdout = child.stdout.take().map(|p| drain(p, cap));
    let stderr = child.stderr.take().map(|p| drain(p, cap));
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break ProbeOutcome::Exited(ExitInfo {
                    code: status.code(),
                    signal: status.signal(),
                });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                kill_group(&child);
                let _ = child.wait();
                break ProbeOutcome::TimedOut;
            }
            Err(err) => {
                kill_group(&child);
                let _ = child.wait();
                break ProbeOutcome::SpawnError(err.to_string());
            }
        }
    };
    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|rx| rx.recv_timeout(PIPE_GRACE).ok())
            .unwrap_or_default()
    };
    Captured {
        outcome,
        stdout: collect(stdout),
        stderr: collect(stderr),
    }
}

/// The real [`ProcessRunner`]: the program with exactly the given
/// environment, in the given directory.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, req: ProbeRequest) -> ProbeRun {
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .current_dir(&req.cwd)
            .env_clear()
            .envs(req.env.iter().map(|(k, v)| (k, v)));
        let captured = run_capped(cmd, req.timeout, PROBE_OUTPUT_BYTES);
        ProbeRun {
            outcome: captured.outcome,
            stdout: captured.stdout,
            stderr: captured.stderr,
        }
    }
}
