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
//! It never signals leftover agent processes (features 5 and 23 own that,
//! from the stored `pid`, `pgid` and `pid_started_at`).

use std::sync::Arc;

use crate::model::Actor;
use crate::ports::{RunLog, RunLogError, Store, StoreError};

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
