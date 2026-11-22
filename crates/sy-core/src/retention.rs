//! Keeping history and disk use in check (spec 0002, AC-9 and AC-10).
//!
//! Permanent delete removes the rows in one store transaction, then the run
//! log files the store reported. A crash between the two leaves orphan files,
//! which startup recovery sweeps.
//!
//! The log cap keeps the total size of run logs under the global setting
//! [`LOG_CAP_KEY`]. It trims whole log files from ended runs of archived tasks
//! only, oldest archive first, committing `trimmed` before each unlink. A non
//! archived task's log, and a live run's log, are never trimmed.

use std::sync::Arc;

use crate::ids::{ProjectId, RunId, TaskId};
use crate::model::Actor;
use crate::ports::{RunLog, Store, StoreError};

/// The global setting holding the log cap, in bytes.
pub const LOG_CAP_KEY: &str = "storage.log_cap_bytes";

/// The log cap when [`LOG_CAP_KEY`] is unset: 5 GiB.
pub const DEFAULT_LOG_CAP_BYTES: u64 = 5_368_709_120;

/// Why a retention use case failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RetentionError {
    /// A store command failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The blocking task that touches `runs/` stopped before it answered.
    #[error("the run log task stopped")]
    Stopped,
}

/// Permanently delete an archived task, then its run log files. Returns the
/// removed run ids. Never touches the repo, a worktree or a branch.
pub async fn delete_task(
    store: &dyn Store,
    run_log: Arc<dyn RunLog>,
    task: &TaskId,
) -> Result<Vec<RunId>, RetentionError> {
    let removed = store.delete_task(task).await?;
    remove_logs(run_log, removed.clone()).await?;
    Ok(removed)
}

/// Permanently delete an archived project, then its run log files. Returns
/// the removed run ids. Never touches the repo, a worktree or a branch.
pub async fn delete_project(
    store: &dyn Store,
    run_log: Arc<dyn RunLog>,
    project: &ProjectId,
) -> Result<Vec<RunId>, RetentionError> {
    let removed = store.delete_project(project).await?;
    remove_logs(run_log, removed.clone()).await?;
    Ok(removed)
}

/// Remove log files whose rows are gone. A file that won't go is left for the
/// next start's orphan sweep, never failing the delete.
async fn remove_logs(run_log: Arc<dyn RunLog>, runs: Vec<RunId>) -> Result<(), RetentionError> {
    tokio::task::spawn_blocking(move || {
        for run in &runs {
            if let Err(err) = run_log.delete(run) {
                tracing::warn!(%run, %err, "could not remove a deleted run's log; the next start sweeps it");
            }
        }
    })
    .await
    .map_err(|_| RetentionError::Stopped)
}

/// What one log cap check did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapCheck {
    /// The cap in force.
    pub cap: u64,
    /// The total before trimming.
    pub before: u64,
    /// The total after trimming.
    pub after: u64,
    /// Log files trimmed.
    pub trimmed: usize,
}

impl CapCheck {
    /// True when the total is still over the cap (only untrimmable logs remain).
    pub const fn still_over(&self) -> bool {
        self.after > self.cap
    }
}

/// The log cap in force: [`LOG_CAP_KEY`] when it holds a non negative
/// integer, else [`DEFAULT_LOG_CAP_BYTES`].
pub async fn log_cap(store: &dyn Store) -> Result<u64, StoreError> {
    let value = store.get_setting(LOG_CAP_KEY).await?;
    Ok(match value {
        None => DEFAULT_LOG_CAP_BYTES,
        Some(value) => value.as_u64().unwrap_or_else(|| {
            tracing::warn!(
                key = LOG_CAP_KEY,
                "the log cap is not a byte count, using the default"
            );
            DEFAULT_LOG_CAP_BYTES
        }),
    })
}

/// Run one log cap check (AC-10). `live_bytes` is the bytes appended so far
/// by every live run (from the run hub), which count toward the total but
/// are never trimmed. Logs a warning when only untrimmable logs remain over.
///
/// Callers run one check at a time; the engine serialises them.
pub async fn enforce_log_cap(
    store: &dyn Store,
    run_log: Arc<dyn RunLog>,
    live_bytes: u64,
) -> Result<CapCheck, RetentionError> {
    let cap = log_cap(store).await?;
    let before = store.log_total_ended().await?.saturating_add(live_bytes);
    let mut check = CapCheck {
        cap,
        before,
        after: before,
        trimmed: 0,
    };
    if before <= cap {
        return Ok(check);
    }
    for candidate in store.log_trim_candidates().await? {
        if check.after <= cap {
            break;
        }
        let committed = store
            .mark_log_trimmed(Actor::Engine, &candidate.run_id)
            .await?;
        if committed.events.is_empty() {
            continue;
        }
        // `trimmed` is committed, so the file can go; if the unlink fails,
        // the next start's sweep removes files of trimmed runs.
        let log = Arc::clone(&run_log);
        let run = candidate.run_id.clone();
        let unlinked = tokio::task::spawn_blocking(move || log.delete(&run))
            .await
            .map_err(|_| RetentionError::Stopped)?;
        if let Err(err) = unlinked {
            tracing::warn!(run = %candidate.run_id, %err, "could not unlink a trimmed run log; the next start sweeps it");
        }
        tracing::info!(run = %candidate.run_id, bytes = committed.value, "trimmed a run log");
        check.after = check.after.saturating_sub(committed.value);
        check.trimmed += 1;
    }
    if check.still_over() {
        tracing::warn!(
            cap,
            total = check.after,
            "run logs are over the cap, but only active tasks' and live runs' logs remain"
        );
    }
    Ok(check)
}
