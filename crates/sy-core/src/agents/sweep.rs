//! The leftover sweep (spec 0004, AC-8 and AC-9), run by each run's monitor
//! once the run reached its end, whatever ended it.
//!
//! The main process is still an unreaped zombie here, so its group and
//! session ids can't be reused and a group signal is safe. Every per pid
//! signal goes through [`ProcessTable::signal`], which checks the recorded
//! start time, the owner, pid 1 and the engine first.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::ports::{ChildProcess, GroupSignal, ProcSignal, ProcessTable, SignalOutcome};
use super::supervise::{ProcKey, descendants, is_gone};
use super::{AgentRuns, lock};
use crate::ids::RunId;

/// Run blocking process table work off the async threads. A failed join
/// (a panic in the work) reads as `fallback`.
async fn off_thread<T: Send + 'static>(fallback: T, f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap_or(fallback)
}

/// Signal each of `targets`; keep the ones it reached.
async fn signal_each(
    table: &Arc<dyn ProcessTable>,
    targets: Vec<ProcKey>,
    signals: &'static [ProcSignal],
) -> Vec<ProcKey> {
    let table = Arc::clone(table);
    off_thread(Vec::new(), move || {
        targets
            .into_iter()
            .filter(|&(pid, started)| {
                let mut sent = false;
                for &signal in signals {
                    sent |= table.signal(pid, started, signal) == SignalOutcome::Sent;
                }
                sent
            })
            .collect()
    })
    .await
}

/// Wait up to `wait`, checking every `poll`, for `keys` to be gone. Returns
/// those still there.
pub(super) async fn wait_gone(
    table: &Arc<dyn ProcessTable>,
    mut keys: Vec<ProcKey>,
    wait: Duration,
    poll: Duration,
) -> Vec<ProcKey> {
    let deadline = Instant::now() + wait;
    loop {
        let check = Arc::clone(table);
        let current = keys.clone();
        keys = off_thread(current.clone(), move || {
            current
                .into_iter()
                .filter(|&k| !is_gone(k, check.started_at(k.0)))
                .collect()
        })
        .await;
        if keys.is_empty() || Instant::now() >= deadline {
            return keys;
        }
        tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

impl AgentRuns {
    /// AC-8's six steps for one ended run, then the main process is reaped.
    pub(super) async fn sweep(&self, id: &RunId, process: &Arc<dyn ChildProcess>) {
        let timings = self.timings;
        let table = Arc::clone(&self.processes);
        let Some(tracking) = self.tracking(id) else {
            let process = Arc::clone(process);
            off_thread((), move || process.reap()).await;
            return;
        };
        let (main_pid, main_started, tracked, roots) = {
            let t = lock(&tracking.tracker);
            (t.main_pid(), t.main_started(), t.tracked(), t.roots())
        };

        // (1) One more scan: every current descendant, whatever its age,
        // plus every tracked process.
        let mut targets: HashMap<ProcKey, String> = tracked
            .iter()
            .filter(|(k, _)| k.0 != main_pid)
            .cloned()
            .collect();
        let scanner = Arc::clone(&table);
        match off_thread(
            Err(super::ports::ProcError("scan panicked".into())),
            move || scanner.scan(),
        )
        .await
        {
            Ok(scan) => {
                for p in descendants(&scan, main_pid, main_started, &roots) {
                    targets.entry((p.pid, p.started_at_ms)).or_insert(p.name);
                }
            }
            Err(err) => {
                tracing::warn!(%err, "the sweep's scan failed; sweeping the tracked processes only");
            }
        }

        // (2) SIGCONT then SIGTERM, to the group and to each one.
        let group = |signal| process.signal_group(signal);
        group(GroupSignal::Cont);
        group(GroupSignal::Term);
        let keys: Vec<ProcKey> = targets.keys().copied().collect();
        let termed = signal_each(&table, keys.clone(), &[ProcSignal::Cont, ProcSignal::Term])
            .await
            .len();

        // (3) Wait for them, unless there was nothing besides the main process.
        let mut alive = keys;
        if !alive.is_empty() {
            alive = wait_gone(&table, alive, timings.sweep_term_wait, timings.sweep_poll).await;
        }

        // (4) SIGKILL to the group and each survivor, then confirm.
        group(GroupSignal::Kill);
        let killed = alive.len();
        signal_each(&table, alive.clone(), &[ProcSignal::Kill]).await;
        if !alive.is_empty() {
            alive = wait_gone(&table, alive, timings.sweep_kill_wait, timings.sweep_poll).await;
        }

        // (5) The main process is reaped.
        let reaper = Arc::clone(process);
        off_thread((), move || reaper.reap()).await;

        // (6) Each row confirmed gone is deleted.
        let mut rows: Vec<ProcKey> = lock(&tracking.tracker)
            .tracked()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        rows.retain(|k| !alive.contains(k));
        if !rows.is_empty()
            && let Err(err) = tracking.store.untrack_processes(id, &rows).await
            && self.write_warn.ready()
        {
            tracing::warn!(%err, "could not delete the run's process rows; engine start retries them");
        }

        let names: BTreeSet<&str> = targets.values().map(String::as_str).collect();
        if !targets.is_empty() {
            tracing::info!(
                found = targets.len(),
                sigterm = termed,
                sigkill = killed,
                names = ?names,
                "swept the run's leftover processes"
            );
        }
        for (pid, _) in &alive {
            let name = targets
                .iter()
                .find(|(k, _)| k.0 == *pid)
                .map_or("?", |(_, n)| n.as_str());
            tracing::warn!(
                pid,
                name,
                "a leftover process could not be confirmed gone; its row stays for the next start"
            );
        }
    }
}
