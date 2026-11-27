//! The process tree scanner (spec 0004, AC-7): one engine wide task that,
//! while any run is live or sweeping, scans the process table every
//! [`Timings::scan_every`](super::Timings::scan_every) on a blocking thread
//! and updates every open run's tracker.
//!
//! Scans never overlap: a tick that comes while one still runs is skipped,
//! never queued. A failed scan changes nothing. A process missing from a
//! scan is checked directly before its row is deleted.

use std::sync::{Arc, Weak};

use tokio::time::MissedTickBehavior;

use super::supervise::{ProcKey, is_gone};
use super::{AgentRuns, Tracking, lock};
use crate::ids::RunId;
use crate::ports::TrackedProcess;

impl AgentRuns {
    /// The scanner loop. Ends once the use case is dropped.
    pub async fn scan_loop(runs: Weak<Self>) {
        let Some(every) = runs.upgrade().map(|r| r.timings.scan_every) else {
            return;
        };
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(runs) = runs.upgrade() else {
                return;
            };
            if runs.live_count() > 0 {
                runs.scan_once().await;
            }
        }
    }

    /// One scan, applied to every run still open when it finishes.
    pub(super) async fn scan_once(&self) {
        let table = Arc::clone(&self.processes);
        let scan = match tokio::task::spawn_blocking(move || table.scan()).await {
            Ok(Ok(scan)) => scan,
            Ok(Err(err)) => {
                if self.scan_warn.ready() {
                    tracing::warn!(%err, "a process table scan failed; it changed nothing");
                }
                return;
            }
            Err(err) => {
                tracing::error!(%err, "a process table scan panicked");
                return;
            }
        };
        let open: Vec<(RunId, Tracking)> = lock(&self.tracking)
            .iter()
            .map(|(id, t)| (id.clone(), t.clone()))
            .collect();
        for (run, tracking) in open {
            let observed = lock(&tracking.tracker).observe(&scan);
            if observed.warn_cap {
                tracing::warn!(
                    %run,
                    over = observed.over_cap,
                    "the run reached its process tracking cap; new processes are not recorded"
                );
            }
            let gone = self.confirm_gone(observed.recheck).await;
            let (rows, deletes) = {
                let mut t = lock(&tracking.tracker);
                if t.is_closed() {
                    continue;
                }
                (observed.insert, t.confirm_gone(&gone))
            };
            let now = self.clock.now_ms();
            let rows: Vec<TrackedProcess> = rows
                .into_iter()
                .map(|r| TrackedProcess {
                    pid: r.pid,
                    started_at: r.started_at,
                    name: r.name,
                    first_seen_at: now,
                })
                .collect();
            if !rows.is_empty()
                && let Err(err) = tracking.store.track_processes(&run, &rows, false).await
                && self.write_warn.ready()
            {
                tracing::warn!(%err, "could not record a run's processes");
            }
            if !deletes.is_empty()
                && let Err(err) = tracking.store.untrack_processes(&run, &deletes).await
                && self.write_warn.ready()
            {
                tracing::warn!(%err, "could not delete a run's process rows");
            }
        }
    }

    /// Of `keys`, the ones a direct check confirms gone (absent, another
    /// start time, or a zombie).
    async fn confirm_gone(&self, keys: Vec<ProcKey>) -> Vec<ProcKey> {
        if keys.is_empty() {
            return keys;
        }
        let table = Arc::clone(&self.processes);
        tokio::task::spawn_blocking(move || {
            keys.into_iter()
                .filter(|&k| is_gone(k, table.started_at(k.0)))
                .collect()
        })
        .await
        .unwrap_or_default()
    }
}
