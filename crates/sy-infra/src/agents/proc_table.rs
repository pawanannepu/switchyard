//! Process facts through `sysinfo` (spec 0004): the process table scan, a
//! direct check of one pid, and per pid signals with the AC-9 safety check.
//!
//! Each scan and each check builds a fresh `sysinfo::System`. On macOS a
//! process `sysinfo` saw before keeps reading as alive after it becomes a
//! zombie, while one it never saw is left out (it has no name to read), so a
//! fresh table makes a zombie read as absent every time: both count as gone.
//! The refresh reads users only, never command lines or environments.
//! Scans never overlap: one mutex holds them. (The engine's scanner skips a
//! tick while its last scan still runs, so its scans never queue.)

use std::sync::{Mutex, PoisonError};

use rustix::process::{Pid, Signal, getpid, getuid, kill_process, kill_process_group};
use sy_core::agents::ports::{
    ProcError, ProcInfo, ProcSignal, ProcState, ProcessTable, SignalOutcome,
};
use sysinfo::{Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

/// The real [`ProcessTable`].
#[derive(Debug)]
pub struct SysProcessTable {
    /// Held by a scan, so two never run at once.
    scanning: Mutex<()>,
    uid: u32,
    engine: i64,
}

impl Default for SysProcessTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SysProcessTable {
    /// The process table as this engine sees it.
    pub fn new() -> Self {
        Self {
            scanning: Mutex::new(()),
            uid: getuid().as_raw(),
            engine: i64::from(getpid().as_raw_nonzero().get()),
        }
    }

    fn refresh(which: ProcessesToUpdate<'_>) -> Result<System, ProcError> {
        let mut system = System::new().map_err(|e| ProcError(e.to_string()))?;
        system.refresh_processes_specifics(
            which,
            true,
            ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
        );
        Ok(system)
    }

    fn info(&self, p: &Process) -> ProcInfo {
        let start = i64::try_from(p.start_time()).unwrap_or(i64::MAX);
        ProcInfo {
            pid: i64::from(p.pid().as_u32()),
            ppid: p.parent().map(|pp| i64::from(pp.as_u32())),
            sid: p.session_id().map(|s| i64::from(s.as_u32())),
            started_at_ms: start.saturating_mul(1000),
            uid_is_self: p.user_id().is_some_and(|u| **u == self.uid),
            zombie: p.status() == ProcessStatus::Zombie,
            name: p.name().to_string_lossy().into_owned(),
        }
    }

    fn one(&self, pid: i64) -> Option<ProcInfo> {
        let raw = u32::try_from(pid).ok()?;
        let wanted = sysinfo::Pid::from_u32(raw);
        let system = Self::refresh(ProcessesToUpdate::Some(&[wanted])).ok()?;
        system.process(wanted).map(|p| self.info(p))
    }
}

const fn signal_of(signal: ProcSignal) -> Signal {
    match signal {
        ProcSignal::Cont => Signal::CONT,
        ProcSignal::Term => Signal::TERM,
        ProcSignal::Kill => Signal::KILL,
    }
}

fn pid_of(pid: i64) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

impl ProcessTable for SysProcessTable {
    fn scan(&self) -> Result<Vec<ProcInfo>, ProcError> {
        let _held = self.scanning.lock().unwrap_or_else(PoisonError::into_inner);
        let system = Self::refresh(ProcessesToUpdate::All)?;
        let all: Vec<ProcInfo> = system.processes().values().map(|p| self.info(p)).collect();
        // The engine itself is always there: an empty table is a failed read.
        if all.iter().all(|p| p.pid != self.engine) {
            return Err(ProcError(
                "the process list did not include the engine".into(),
            ));
        }
        Ok(all)
    }

    fn started_at(&self, pid: i64) -> Option<ProcState> {
        self.one(pid).map(|p| ProcState {
            started_at_ms: p.started_at_ms,
            zombie: p.zombie,
        })
    }

    fn signal(&self, pid: i64, started_at_ms: i64, signal: ProcSignal) -> SignalOutcome {
        if pid <= 1 || pid == self.engine {
            return SignalOutcome::Gone;
        }
        let Some(target) = pid_of(pid) else {
            return SignalOutcome::Gone;
        };
        match self.one(pid) {
            Some(p) if p.started_at_ms == started_at_ms && p.uid_is_self && !p.zombie => {
                match kill_process(target, signal_of(signal)) {
                    Err(rustix::io::Errno::SRCH) => SignalOutcome::Gone,
                    // Refused (`EPERM`) still counts as sent: only the
                    // confirmation decides whether it is gone.
                    Ok(()) | Err(_) => SignalOutcome::Sent,
                }
            }
            _ => SignalOutcome::Gone,
        }
    }

    fn signal_group(&self, pgid: i64, signal: ProcSignal) -> bool {
        if pgid <= 1 || pgid == self.engine {
            return false;
        }
        pid_of(pgid).is_some_and(|g| kill_process_group(g, signal_of(signal)).is_ok())
    }

    fn engine_pid(&self) -> i64 {
        self.engine
    }
}
