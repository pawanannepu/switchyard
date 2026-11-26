//! PTYs through portable-pty, with our own reaping.
//!
//! portable-pty opens the PTY and spawns the child in a new session with the
//! PTY as its controlling terminal (so `pgid = pid`). It reports a death by
//! signal only as a name, so the engine waits for the child itself: a
//! `waitid` with `WNOWAIT` sees the exit (and the real signal number) while
//! the child is still a zombie, then, under the same lock group signals take,
//! the child is reaped. A group signal is therefore never sent after the
//! reap, so a reused process group id is never hit.

use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use portable_pty::{CommandBuilder, MasterPty, PtySize as PortableSize, native_pty_system};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};
use sy_adapter_api::{ExitInfo, PtySize};
use sy_core::agents::ports::{
    ChildProcess, GroupSignal, Pty, PtyChild, PtyError, PtyIo, SpawnRequest,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

const fn portable(size: PtySize) -> PortableSize {
    PortableSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Spawns agents in real PTYs.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortablePty;

impl Pty for PortablePty {
    fn spawn(&self, req: SpawnRequest) -> Result<PtyChild, PtyError> {
        // portable-pty would quietly start in the home dir instead.
        if !Path::new(&req.cwd).is_dir() {
            return Err(PtyError("the working directory does not exist".into()));
        }
        let pair = native_pty_system()
            .openpty(portable(req.size))
            .map_err(|e| PtyError(format!("opening a terminal: {e}")))?;
        let mut cmd = CommandBuilder::new(&req.program);
        cmd.args(&req.args);
        cmd.cwd(&req.cwd);
        cmd.env_clear();
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError(format!("spawning: {e}")))?;
        // Only the child holds the terminal's slave end now, so its exit ends our reads.
        drop(pair.slave);
        let raw = child
            .process_id()
            .and_then(|p| i32::try_from(p).ok())
            .ok_or_else(|| PtyError("the child has no process id".into()))?;
        let pid = Pid::from_raw(raw).ok_or_else(|| PtyError("bad process id".into()))?;
        let process = std::sync::Arc::new(UnixProcess {
            pid,
            reaped: Mutex::new(false),
            _child: Mutex::new(child),
        });
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => {
                process.signal_group(GroupSignal::Kill);
                return Err(PtyError(format!("reading the terminal: {e}")));
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(e) => {
                process.signal_group(GroupSignal::Kill);
                return Err(PtyError(format!("writing the terminal: {e}")));
            }
        };
        Ok(PtyChild {
            pid: i64::from(raw),
            pgid: i64::from(raw),
            reader,
            io: Box::new(MasterIo {
                master: pair.master,
                writer,
            }),
            process,
        })
    }
}

struct MasterIo {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
}

impl PtyIo for MasterIo {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    fn resize(&mut self, size: PtySize) -> std::io::Result<()> {
        self.master
            .resize(portable(size))
            .map_err(std::io::Error::other)
    }
}

struct UnixProcess {
    pid: Pid,
    /// True once the child is reaped. Group signals check it under this lock.
    reaped: Mutex<bool>,
    /// Kept so the handle lives as long as the process; never waited on.
    _child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl ChildProcess for UnixProcess {
    fn wait(&self) -> std::io::Result<ExitInfo> {
        let status = loop {
            match rustix::process::waitid(
                WaitId::Pid(self.pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(status)) => break status,
                Ok(None) | Err(rustix::io::Errno::INTR) => {}
                Err(err) => return Err(err.into()),
            }
        };
        let info = ExitInfo {
            code: status.exit_status(),
            signal: status.terminating_signal(),
        };
        let mut reaped = lock(&self.reaped);
        let _ = rustix::process::waitpid(Some(self.pid), WaitOptions::empty());
        *reaped = true;
        Ok(info)
    }

    fn signal_group(&self, signal: GroupSignal) -> bool {
        let reaped = lock(&self.reaped);
        if *reaped {
            return false;
        }
        let signal = match signal {
            GroupSignal::Hangup => Signal::HUP,
            GroupSignal::Kill => Signal::KILL,
        };
        rustix::process::kill_process_group(self.pid, signal).is_ok()
    }
}
