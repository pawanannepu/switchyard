//! Fault isolation (spec 0004, AC-11): a panic in one run's supervision
//! fails only that run, as `supervisor_error`.
//!
//! A panic in the PTY reader, writer or waiter thread is caught at the
//! thread's boundary and sends [`Control::SupervisorFault`](super::driver)
//! to the driver; a panic in the driver task itself is seen by the run's
//! monitor as a panicked `JoinError`. Either way [`AgentRuns::fault_end`]
//! runs the same idempotent end order. The run id is kept in a task local
//! (the driver and monitor) or a thread local (the PTY threads), so the
//! daemon's panic hook can log the panic's location with it.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::approvals::ApprovalNotice;
use super::ports::{ChildProcess, GroupSignal};
use super::{AgentRuns, RunWrites};
use crate::ids::RunId;
use crate::model::{Actor, EndReason, RunEnd, RunStatus, RunTransition};

tokio::task_local! {
    static TASK_RUN: String;
}

thread_local! {
    static THREAD_RUN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The run whose supervision code is running on this task or thread, for
/// the daemon's panic hook (a `JoinError` carries no location).
pub fn current_run_id() -> Option<String> {
    TASK_RUN
        .try_with(Clone::clone)
        .ok()
        .or_else(|| THREAD_RUN.with(|r| r.borrow().clone()))
}

/// Run `fut` with its run id in the task local.
pub(super) fn with_run_id<F: Future>(
    run: &RunId,
    fut: F,
) -> impl Future<Output = F::Output> + use<F> {
    TASK_RUN.scope(run.to_string(), fut)
}

/// Set (or clear) this thread's run id.
pub(super) fn set_thread_run(run: Option<String>) {
    THREAD_RUN.with(|r| *r.borrow_mut() = run);
}

/// What the fault end needs, shared by the driver and the monitor, so the
/// monitor can still end the run after the driver task panicked.
pub(super) struct RunKit {
    /// The run's signal token.
    pub token: String,
    /// The main process.
    pub process: Arc<dyn ChildProcess>,
    /// Tells the reader thread to stop.
    pub cut: Arc<AtomicBool>,
    /// Set once the reader thread is done (EOF, cut, or a panic).
    pub reader_finished: Arc<AtomicBool>,
    /// Set once the waiter thread observed the exit.
    pub exited: Arc<AtomicBool>,
}

/// Wait up to `wait` for `flag`. Returns whether it was set.
async fn wait_flag(flag: &AtomicBool, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl AgentRuns {
    /// AC-11's end order for a run whose supervision panicked. Idempotent:
    /// a run already at its terminal transition skips steps 4 to 6, and the
    /// launch dir and the sweep (by the monitor) still follow. A failure in
    /// a step is logged, never retried; the next start marks the run `lost`.
    pub(super) async fn fault_end(&self, store: &dyn RunWrites, id: &RunId, kit: &RunKit) {
        tracing::error!(
            end_reason = EndReason::SupervisorError.as_str(),
            "the run's supervision failed; ending only this run"
        );
        let timings = self.timings;
        // (1) No new hook calls. (2) Held hooks get no reply: their reply
        // channels are dropped with the driver's state, so `sy-signal` falls back.
        self.revoke(&kit.token);
        // (3) SIGHUP, the wait, SIGKILL: no adapter stop input, since the
        // decoder can't be trusted.
        if !kit.exited.load(Ordering::SeqCst) {
            kit.process.signal_group(GroupSignal::Hangup);
            if !wait_flag(&kit.exited, timings.hangup_wait).await {
                kit.process.signal_group(GroupSignal::Kill);
                wait_flag(&kit.exited, timings.kill_wait).await;
            }
        }
        match store.get_run(id).await {
            Ok(row) if row.status.is_terminal() => {
                tracing::debug!("the run had already ended; only its cleanup is left");
            }
            Ok(_) => {
                // (4) Its pending approvals become `cancelled`, announced.
                match store
                    .cancel_pending_approvals(Actor::Engine, id, None)
                    .await
                {
                    Ok(committed) => {
                        for (i, approval) in committed.value.into_iter().enumerate() {
                            self.announce(ApprovalNotice::Decided {
                                event_id: committed.events.get(i).map(|e| e.id),
                                approval,
                            });
                        }
                    }
                    Err(err) => tracing::warn!(%err, "could not cancel the run's approvals"),
                }
                // (5) The PTY drain and the hub's final flush.
                wait_flag(&kit.reader_finished, timings.drain).await;
                kit.cut.store(true, Ordering::SeqCst);
                let log_bytes = self.remove_from_hub(id).await;
                // (6) `failed` with `supervisor_error`, from its stored status.
                let end = RunTransition::end(
                    RunStatus::Failed,
                    RunEnd {
                        reason: EndReason::SupervisorError,
                        exit_code: None,
                        log_bytes,
                    },
                );
                if let Err(err) = store.transition_run(Actor::Engine, id, end).await {
                    tracing::warn!(%err, "could not record the run's end; the next start marks it lost");
                }
            }
            Err(err) => {
                tracing::warn!(%err, "could not read the run; the next start marks it lost");
                kit.cut.store(true, Ordering::SeqCst);
                self.remove_from_hub(id).await;
            }
        }
        self.ended(id);
        // (7) The launch dir goes; the monitor then sweeps.
        self.remove_launch_dir(id).await;
    }
}
