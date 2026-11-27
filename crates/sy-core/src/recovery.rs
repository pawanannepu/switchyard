//! Startup recovery (spec 0002, AC-6): bring the database and `runs/` back in
//! line after the engine stopped, cleanly or not.
//!
//! Runs once at start, before any client is served, in this order:
//! 1. every live run (`starting`, `running`, `waiting_input`) becomes `lost`,
//!    ended at the engine's start time; `queued` runs stay `queued`;
//! 2. every workspace still in `preparing` becomes `discarded`;
//! 3. each run's `log_bytes` becomes its file's real length, and a run that
//!    had output but lost its file becomes `deleted`;
//! 4. files in `runs/` with no run row, or whose run is `trimmed` or
//!    `deleted`, are removed.
//!
//! It never signals leftover agent processes itself. Feature 5 (spec 0004,
//! AC-10) does that afterwards: [`snapshot_leftovers`] reads every
//! `run_process` row before any connection is accepted (so no run of this
//! engine is in it), and [`cleanup_leftovers`] stops them in the background.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::agents::ports::{ProcSignal, ProcessTable, SignalOutcome};
use crate::agents::supervise::{ProcKey, is_gone};
use crate::ids::RunId;
use crate::model::Actor;
use crate::ports::{RunLog, RunLogError, Store, StoreError, TrackedProcess};

/// What startup recovery changed, for the engine log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Runs marked `lost`.
    pub lost: usize,
    /// Workspaces moved from `preparing` to `discarded`.
    pub discarded: usize,
    /// Runs whose log file vanished, now `deleted`.
    pub deleted_logs: usize,
    /// Files removed from `runs/`.
    pub removed_files: usize,
}

/// Why startup recovery failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryError {
    /// A store command failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Listing `runs/` failed.
    #[error(transparent)]
    RunLog(#[from] RunLogError),
    /// The blocking task that touches `runs/` stopped before it answered.
    #[error("the run log task stopped")]
    Stopped,
}

/// Run startup recovery. `started_at` is the engine's start time, read once
/// from the `Clock`; every lost run ends then, and every discarded workspace
/// closes then.
pub async fn recover_at_startup(
    store: &dyn Store,
    run_log: Arc<dyn RunLog>,
    started_at: i64,
) -> Result<Recovered, RecoveryError> {
    let lost = store
        .mark_live_runs_lost(Actor::Engine, started_at)
        .await?
        .value
        .len();
    let discarded = store
        .discard_preparing_workspaces(Actor::Engine, started_at)
        .await?
        .value
        .len();

    let lister = Arc::clone(&run_log);
    let files = tokio::task::spawn_blocking(move || lister.list())
        .await
        .map_err(|_| RecoveryError::Stopped)??;
    let reconciled = store.reconcile_log_bytes(files).await?;

    let remove = reconciled.remove;
    let removed_files = tokio::task::spawn_blocking(move || {
        remove
            .iter()
            .filter(|run| match run_log.delete(run) {
                Ok(()) => true,
                Err(err) => {
                    // The next start tries again; one stuck file never blocks the engine.
                    tracing::warn!(%run, %err, "could not remove a stale run log");
                    false
                }
            })
            .count()
    })
    .await
    .map_err(|_| RecoveryError::Stopped)?;

    Ok(Recovered {
        lost,
        discarded,
        deleted_logs: reconciled.deleted.len(),
        removed_files,
    })
}

/// What a crashed engine's runs left behind, read at start (spec 0004, AC-10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// Every `run_process` row.
    pub rows: Vec<(RunId, TrackedProcess)>,
    /// Each run's recorded main `pid` and `pgid`, for the group signal.
    pub groups: HashMap<RunId, (i64, i64)>,
}

/// Read every `run_process` row, and each of their runs' `pid` and `pgid`.
/// Call it after [`recover_at_startup`] and before accepting connections.
pub async fn snapshot_leftovers(store: &dyn Store) -> Result<Leftovers, StoreError> {
    let rows = store.list_tracked_processes().await?;
    let mut groups = HashMap::new();
    for (run, _) in &rows {
        if groups.contains_key(run) {
            continue;
        }
        match store.get_run(run).await {
            Ok(row) => {
                if let (Some(pid), Some(pgid)) = (row.pid, row.pgid) {
                    groups.insert(run.clone(), (pid, pgid));
                }
            }
            Err(StoreError::NotFound) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(Leftovers { rows, groups })
}

/// What [`cleanup_leftovers`] did, for the engine log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// Rows in the snapshot.
    pub rows: usize,
    /// Processes that still matched and got SIGTERM.
    pub sigterm: usize,
    /// Of those, the ones that needed SIGKILL.
    pub sigkill: usize,
    /// Rows deleted (confirmed gone, a reused pid included).
    pub deleted: usize,
    /// Rows kept: their process could not be confirmed gone.
    pub kept: usize,
}

fn group_send(table: &dyn ProcessTable, pgid: i64, signals: &[ProcSignal]) {
    for &signal in signals {
        table.signal_group(pgid, signal);
    }
}

fn matches(table: &dyn ProcessTable, key: ProcKey) -> bool {
    !is_gone(key, table.started_at(key.0))
}

/// The restart cleanup's blocking work: AC-10 steps 1 to 3. Returns the
/// SIGTERM and SIGKILL counts and the keys still matching after SIGKILL.
fn stop_leftovers(
    table: &dyn ProcessTable,
    snapshot: &Leftovers,
    term_wait: Duration,
) -> (usize, usize, Vec<(RunId, ProcKey)>) {
    // A main row that still matches: its group id is reserved by it, so the
    // group gets the signals too.
    let groups: Vec<(RunId, i64, ProcKey)> = snapshot
        .rows
        .iter()
        .filter_map(|(run, row)| {
            let (pid, pgid) = *snapshot.groups.get(run)?;
            (row.pid == pid && matches(table, (row.pid, row.started_at))).then_some((
                run.clone(),
                pgid,
                (row.pid, row.started_at),
            ))
        })
        .collect();
    for (_, pgid, _) in &groups {
        group_send(table, *pgid, &[ProcSignal::Cont, ProcSignal::Term]);
    }
    let targets: Vec<(RunId, ProcKey)> = snapshot
        .rows
        .iter()
        .map(|(run, row)| (run.clone(), (row.pid, row.started_at)))
        .collect();
    let mut termed = 0;
    for (_, (pid, started)) in &targets {
        let cont = table.signal(*pid, *started, ProcSignal::Cont);
        if table.signal(*pid, *started, ProcSignal::Term) == SignalOutcome::Sent
            || cont == SignalOutcome::Sent
        {
            termed += 1;
        }
    }
    if termed > 0 || !groups.is_empty() {
        std::thread::sleep(term_wait);
    }
    for (run, pgid, main) in &groups {
        if matches(table, *main) {
            group_send(table, *pgid, &[ProcSignal::Kill]);
        } else {
            tracing::debug!(%run, "main process gone; no group SIGKILL");
        }
    }
    let mut killed = 0;
    for (_, (pid, started)) in &targets {
        if table.signal(*pid, *started, ProcSignal::Kill) == SignalOutcome::Sent {
            killed += 1;
        }
    }
    if killed > 0 {
        // A moment for the kernel to tear them down before confirming.
        std::thread::sleep(Duration::from_millis(100));
    }
    let left = targets
        .into_iter()
        .filter(|(_, key)| matches(table, *key))
        .collect();
    (termed, killed, left)
}

/// Stop what a crashed engine left behind (spec 0004, AC-10), working only
/// from `snapshot`: SIGCONT and SIGTERM to each matching row (and the group
/// of a main row that still matches), SIGKILL after `term_wait` to those
/// still matching, then delete each row confirmed gone by its exact key. A
/// row whose pid is now another process (a different start time) is deleted
/// and never signalled. Runs in the background while clients are served.
pub async fn cleanup_leftovers(
    store: Arc<dyn Store>,
    table: Arc<dyn ProcessTable>,
    snapshot: Leftovers,
    term_wait: Duration,
) -> Cleaned {
    let rows = snapshot.rows.len();
    if rows == 0 {
        return Cleaned::default();
    }
    let work = Arc::new(snapshot);
    let shared = Arc::clone(&work);
    let (sigterm, sigkill, left) =
        tokio::task::spawn_blocking(move || stop_leftovers(table.as_ref(), &shared, term_wait))
            .await
            .unwrap_or_else(|err| {
                tracing::error!(%err, "restart cleanup panicked; its rows wait for the next start");
                (
                    0,
                    0,
                    work.rows
                        .iter()
                        .map(|(r, t)| (r.clone(), (t.pid, t.started_at)))
                        .collect(),
                )
            });
    let mut gone: HashMap<RunId, Vec<ProcKey>> = HashMap::new();
    for (run, row) in &work.rows {
        let key = (row.pid, row.started_at);
        if !left.iter().any(|(r, k)| r == run && *k == key) {
            gone.entry(run.clone()).or_default().push(key);
        }
    }
    let mut deleted = 0;
    for (run, keys) in gone {
        match store.untrack_processes(&run, &keys).await {
            Ok(()) => deleted += keys.len(),
            Err(err) => tracing::warn!(%run, %err, "could not delete leftover process rows"),
        }
    }
    for (run, (pid, _)) in &left {
        tracing::warn!(%run, pid, "a leftover process could not be confirmed gone; retried at the next start");
    }
    let cleaned = Cleaned {
        rows,
        sigterm,
        sigkill,
        deleted,
        kept: left.len(),
    };
    tracing::info!(
        rows = cleaned.rows,
        sigterm = cleaned.sigterm,
        sigkill = cleaned.sigkill,
        deleted = cleaned.deleted,
        kept = cleaned.kept,
        "restart cleanup done"
    );
    cleaned
}

#[cfg(test)]
mod leftover_tests {
    use std::sync::Mutex;

    use super::*;
    use crate::agents::ports::{ProcError, ProcInfo, ProcState};

    /// Live processes by pid (start time), and every signal sent.
    #[derive(Default)]
    struct Table {
        live: Mutex<HashMap<i64, i64>>,
        sent: Mutex<Vec<String>>,
        /// Pids that survive SIGKILL (stuck in the kernel).
        stuck: Vec<i64>,
    }

    impl ProcessTable for Table {
        fn scan(&self) -> Result<Vec<ProcInfo>, ProcError> {
            Ok(Vec::new())
        }
        fn started_at(&self, pid: i64) -> Option<ProcState> {
            let live = self
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            live.get(&pid).map(|&started_at_ms| ProcState {
                started_at_ms,
                zombie: false,
            })
        }
        fn signal(&self, pid: i64, started: i64, signal: ProcSignal) -> SignalOutcome {
            if self.started_at(pid).map(|s| s.started_at_ms) != Some(started) {
                return SignalOutcome::Gone;
            }
            let mut sent = self
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sent.push(format!("{signal:?} {pid}"));
            if signal == ProcSignal::Kill && !self.stuck.contains(&pid) {
                drop(sent);
                self.live
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&pid);
            }
            SignalOutcome::Sent
        }
        fn signal_group(&self, pgid: i64, signal: ProcSignal) -> bool {
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("group {signal:?} {pgid}"));
            true
        }
        fn engine_pid(&self) -> i64 {
            1
        }
    }

    fn row(pid: i64, started_at: i64) -> TrackedProcess {
        TrackedProcess {
            pid,
            started_at,
            name: "x".into(),
            first_seen_at: 0,
        }
    }

    #[test]
    fn only_a_matching_main_row_should_get_its_group_signalled() {
        let (alive_run, dead_run) = (RunId::new(), RunId::new());
        let table = Table::default();
        {
            let mut live = table
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The first run's main still runs; the second's pid is reused.
            live.insert(10, 1000);
            live.insert(20, 9999);
            live.insert(21, 2100);
        }
        let snapshot = Leftovers {
            rows: vec![
                (alive_run.clone(), row(10, 1000)),
                (dead_run.clone(), row(20, 2000)),
                (dead_run.clone(), row(21, 2100)),
            ],
            groups: HashMap::from([(alive_run, (10, 10)), (dead_run, (20, 20))]),
        };
        let (term, kill, left) = stop_leftovers(&table, &snapshot, Duration::ZERO);
        let sent = table
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(sent.contains(&"group Term 10".to_owned()), "{sent:?}");
        assert!(sent.iter().all(|s| !s.ends_with(" 20")), "{sent:?}");
        assert!(sent.contains(&"Term 21".to_owned()));
        assert_eq!((term, kill, left), (2, 2, Vec::new()));
    }

    fn table_with(live: &[(i64, i64)], stuck: Vec<i64>) -> Table {
        Table {
            live: Mutex::new(live.iter().copied().collect()),
            sent: Mutex::default(),
            stuck,
        }
    }

    fn sent(table: &Table) -> Vec<String> {
        table
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    // covers: AC-10
    #[test]
    fn a_process_that_survives_sigkill_should_be_left_for_the_next_start() {
        let run = RunId::new();
        let table = table_with(&[(30, 3000), (31, 3100)], vec![31]);
        let snapshot = Leftovers {
            rows: vec![(run.clone(), row(30, 3000)), (run.clone(), row(31, 3100))],
            groups: HashMap::new(),
        };
        let (term, kill, left) = stop_leftovers(&table, &snapshot, Duration::ZERO);
        assert_eq!((term, kill), (2, 2));
        assert_eq!(left, vec![(run, (31, 3100))]);
    }

    // covers: AC-9, AC-10
    #[test]
    fn rows_whose_pids_were_all_reused_should_get_no_signal_and_no_wait() {
        let run = RunId::new();
        // Both pids now belong to processes with other start times.
        let table = table_with(&[(40, 9000), (41, 9100)], Vec::new());
        let snapshot = Leftovers {
            rows: vec![(run.clone(), row(40, 4000)), (run.clone(), row(41, 4100))],
            groups: HashMap::from([(run, (40, 40))]),
        };
        let started = std::time::Instant::now();
        let (term, kill, left) = stop_leftovers(&table, &snapshot, Duration::from_secs(3));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!((term, kill), (0, 0));
        // Gone (a mismatch counts as gone), so their rows are deleted.
        assert!(left.is_empty());
        assert!(sent(&table).is_empty(), "{:?}", sent(&table));
    }

    // covers: AC-10
    #[test]
    fn a_main_row_without_a_recorded_group_should_get_only_per_pid_signals() {
        let run = RunId::new();
        let table = table_with(&[(50, 5000)], Vec::new());
        let snapshot = Leftovers {
            rows: vec![(run, row(50, 5000))],
            groups: HashMap::new(),
        };
        stop_leftovers(&table, &snapshot, Duration::ZERO);
        let sent = sent(&table);
        assert!(sent.iter().all(|s| !s.starts_with("group")), "{sent:?}");
        assert!(sent.contains(&"Term 50".to_owned()), "{sent:?}");
    }
}
