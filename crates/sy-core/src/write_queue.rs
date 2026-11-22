//! Degraded writes (spec 0002, AC-13): keep running when the disk is full.
//!
//! [`QueuedStore`] wraps the real [`Store`]. While writes succeed it passes
//! every command straight through. When one fails with a storage error (disk
//! full, an I/O error, or the database thread gone), or a run log append
//! fails, storage becomes **degraded writes**:
//!
//! - A failed `transition_run`, `set_*` or `append_event` goes into an in
//!   memory queue, in order, and every later command of those kinds joins the
//!   queue behind it, so order holds. The caller gets the projected result
//!   (the status machine and lifecycle rules still apply in memory) and no
//!   events. A queued run status change is announced as
//!   [`StorageNotice::Unjournaled`], which the engine sends as `run.status`
//!   with `event_id: null`.
//! - The queue holds at most [`QUEUE_CAP`] commands. Past that, new
//!   `append_event` commands are dropped and counted.
//! - Creates (`create_*`, `add_workspace`) are never queued. They, and every
//!   other write (archive, delete, trim, startup recovery), fail at once with
//!   [`StoreError::Full`] while degraded.
//!
//! The engine calls [`QueuedStore::retry`] every [`RETRY_INTERVAL`]: it
//! replays the queue in order, then probes with a one row write to the
//! `setting` key [`STORAGE_PROBE_KEY`]. Once the queue is empty and the probe
//! succeeds, storage is back to ok.
//!
//! This knowingly breaks persist before broadcast: an engine death while
//! degraded loses what is queued.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard, mpsc, watch};

use crate::ids::{ProjectId, RunId, TaskId, WorkspaceId};
use crate::model::{
    Actor, AdapterEvent, EventKind, JsonValue, LogState, NewProject, NewRun, NewTask, NewWorkspace,
    Project, ProjectSettings, Run, RunTransition, STORAGE_PROBE_KEY, Task, Workspace,
    WorkspaceChange, WorkspaceOutcome,
};
use crate::ports::{
    Clock, Committed, EventFilter, EventPage, ListQuery, LogReconcile, Page, RunFilter,
    RunLogError, RunProcess, Store, StoreError, TrimCandidate,
};

/// How often the engine retries the queue and probes while degraded.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// The most commands the queue holds before new `append_event` commands are dropped.
pub const QUEUE_CAP: usize = 10_000;

/// Whether writes are landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteHealth {
    /// Every write lands.
    Ok,
    /// A write failed; changes wait in memory (`degraded_writes`).
    Degraded {
        /// What failed, for `engine.info.storage.message`. Holds no row data.
        message: String,
    },
}

/// What the engine broadcasts for the write path, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageNotice {
    /// Write health changed (send `storage.status`).
    Health(WriteHealth),
    /// A run status change that was queued, not journaled: send `run.status`
    /// with `event_id: null`. Always a `run_status_changed` kind.
    Unjournaled(EventKind),
}

/// One queued write, replayed in order.
#[derive(Debug, Clone)]
enum Queued {
    Transition {
        actor: Actor,
        run: RunId,
        change: RunTransition,
    },
    ProjectSetting {
        actor: Actor,
        project: ProjectId,
        key: String,
        value: JsonValue,
    },
    Setting {
        actor: Actor,
        key: String,
        value: JsonValue,
    },
    WorkspaceState {
        actor: Actor,
        workspace: WorkspaceId,
        change: WorkspaceChange,
    },
    WorkspaceOutcome {
        actor: Actor,
        workspace: WorkspaceId,
        outcome: WorkspaceOutcome,
    },
    RunProcess {
        run: RunId,
        process: RunProcess,
    },
    SessionRef {
        run: RunId,
        session_ref: String,
    },
    LogState {
        run: RunId,
        state: LogState,
    },
    Event {
        actor: Actor,
        actor_ref: Option<String>,
        run: RunId,
        event: AdapterEvent,
    },
}

impl Queued {
    const fn name(&self) -> &'static str {
        match self {
            Self::Transition { .. } => "transition_run",
            Self::ProjectSetting { .. } => "set_project_setting",
            Self::Setting { .. } => "set_setting",
            Self::WorkspaceState { .. } => "set_workspace_state",
            Self::WorkspaceOutcome { .. } => "set_workspace_outcome",
            Self::RunProcess { .. } => "set_run_process",
            Self::SessionRef { .. } => "set_agent_session_ref",
            Self::LogState { .. } => "set_log_state",
            Self::Event { .. } => "append_event",
        }
    }
}

/// A failure that queueing can wait out, as opposed to a refusal (a
/// constraint, a bad transition, a missing row) that would fail again.
const fn is_storage_failure(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Full | StoreError::Io(_) | StoreError::Unavailable
    )
}

#[derive(Default)]
struct State {
    /// `Some` while degraded.
    degraded: Option<String>,
    queue: VecDeque<Queued>,
    /// `append_event` commands dropped past the cap since degrading.
    dropped: u64,
    /// Runs as they are now: live runs seen on the ok path (so a transition
    /// can be checked even when reads fail too), plus every run a queued
    /// command changed.
    runs: HashMap<RunId, Run>,
    /// Workspaces a queued command changed.
    workspaces: HashMap<WorkspaceId, Workspace>,
}

impl State {
    fn remember(&mut self, run: &Run) {
        if run.status.is_terminal() {
            self.runs.remove(&run.id);
        } else {
            self.runs.insert(run.id.clone(), run.clone());
        }
    }
}

/// The [`Store`] the engine uses: the real store plus the degraded write queue.
pub struct QueuedStore {
    inner: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    /// Held across every write, so writes keep their order through the queue.
    state: Mutex<State>,
    health: watch::Sender<WriteHealth>,
    notices: mpsc::UnboundedSender<StorageNotice>,
}

impl QueuedStore {
    /// Wrap `inner`. Health changes and unjournaled run status changes go to `notices`.
    pub fn new(
        inner: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
        notices: mpsc::UnboundedSender<StorageNotice>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            clock,
            state: Mutex::new(State::default()),
            health: watch::Sender::new(WriteHealth::Ok),
            notices,
        })
    }

    /// Write health now.
    pub fn health(&self) -> WriteHealth {
        self.health.borrow().clone()
    }

    /// Commands waiting in the queue.
    pub async fn queued(&self) -> usize {
        self.state.lock().await.queue.len()
    }

    fn degrade(&self, st: &mut State, what: &str, err: &dyn std::fmt::Display) {
        if st.degraded.is_some() {
            return;
        }
        let message = format!(
            "{what} failed ({err}); output still streams, and saving is retried every {} seconds",
            RETRY_INTERVAL.as_secs()
        );
        tracing::error!(%message, "storage is now degraded_writes");
        st.degraded = Some(message.clone());
        let health = WriteHealth::Degraded { message };
        self.health.send_replace(health.clone());
        let _ = self.notices.send(StorageNotice::Health(health));
    }

    fn enqueue(&self, st: &mut State, command: Queued) {
        if st.queue.len() >= QUEUE_CAP && matches!(command, Queued::Event { .. }) {
            st.dropped += 1;
            if st.dropped == 1 || st.dropped.is_multiple_of(1000) {
                tracing::warn!(
                    dropped = st.dropped,
                    "write queue full, dropping journal events"
                );
            }
            return;
        }
        st.queue.push_back(command);
    }

    /// A run log append failed (AC-13): storage becomes degraded, and the run
    /// is marked `incomplete` (queued like any other `set_*`).
    pub async fn log_failed(&self, run: &RunId, err: &RunLogError) {
        let mut st = self.state.lock().await;
        self.degrade(&mut st, "a run log append", err);
        if let Err(err) = self.log_state_in(&mut st, run, LogState::Incomplete).await {
            tracing::warn!(%run, %err, "could not mark the run log incomplete");
        }
    }

    /// Replay the queue in order, then probe. Returns true when storage is
    /// (now, or already) back to ok. A replayed command the store refuses
    /// for a reason other than storage (it would never succeed) is dropped.
    pub async fn retry(&self) -> bool {
        let mut st = self.state.lock().await;
        if st.degraded.is_none() {
            return true;
        }
        while let Some(command) = st.queue.front().cloned() {
            match self.replay(command.clone()).await {
                Ok(()) => {}
                Err(err) if is_storage_failure(&err) => return false,
                Err(err) => tracing::error!(
                    command = command.name(),
                    %err,
                    "a queued change was refused when replayed, dropping it"
                ),
            }
            st.queue.pop_front();
        }
        let now = JsonValue::Int(self.clock.now_ms());
        if let Err(err) = self
            .inner
            .set_setting(Actor::Engine, STORAGE_PROBE_KEY, now)
            .await
        {
            tracing::debug!(%err, "storage probe failed");
            return false;
        }
        tracing::info!(dropped_events = st.dropped, "storage writes recovered");
        st.degraded = None;
        st.dropped = 0;
        st.workspaces.clear();
        st.runs.retain(|_, run| !run.status.is_terminal());
        self.health.send_replace(WriteHealth::Ok);
        let _ = self.notices.send(StorageNotice::Health(WriteHealth::Ok));
        true
    }

    async fn replay(&self, command: Queued) -> Result<(), StoreError> {
        let inner = self.inner.as_ref();
        match command {
            Queued::Transition { actor, run, change } => {
                inner.transition_run(actor, &run, change).await.map(drop)
            }
            Queued::ProjectSetting {
                actor,
                project,
                key,
                value,
            } => inner
                .set_project_setting(actor, &project, &key, value)
                .await
                .map(drop),
            Queued::Setting { actor, key, value } => {
                inner.set_setting(actor, &key, value).await.map(drop)
            }
            Queued::WorkspaceState {
                actor,
                workspace,
                change,
            } => inner
                .set_workspace_state(actor, &workspace, change)
                .await
                .map(drop),
            Queued::WorkspaceOutcome {
                actor,
                workspace,
                outcome,
            } => inner
                .set_workspace_outcome(actor, &workspace, outcome)
                .await
                .map(drop),
            Queued::RunProcess { run, process } => {
                inner.set_run_process(&run, process).await.map(drop)
            }
            Queued::SessionRef { run, session_ref } => inner
                .set_agent_session_ref(&run, &session_ref)
                .await
                .map(drop),
            Queued::LogState { run, state } => inner.set_log_state(&run, state).await.map(drop),
            Queued::Event {
                actor,
                actor_ref,
                run,
                event,
            } => inner
                .append_event(actor, actor_ref, &run, event)
                .await
                .map(drop),
        }
    }

    /// The run as it is now: a queued change's result, a live run seen
    /// earlier, or the database's row.
    async fn run_now(&self, st: &State, run: &RunId) -> Result<Run, StoreError> {
        match st.runs.get(run) {
            Some(known) => Ok(known.clone()),
            None => self.inner.get_run(run).await,
        }
    }

    async fn workspace_now(
        &self,
        st: &State,
        workspace: &WorkspaceId,
    ) -> Result<Workspace, StoreError> {
        match st.workspaces.get(workspace) {
            Some(known) => Ok(known.clone()),
            None => self.inner.get_workspace(workspace).await,
        }
    }

    /// Guard for a write that is never queued: refused while degraded, and a
    /// storage failure degrades.
    async fn unqueued<T, F>(&self, name: &str, write: F) -> Result<T, StoreError>
    where
        F: Future<Output = Result<T, StoreError>>,
    {
        let mut st = self.state.lock().await;
        if st.degraded.is_some() {
            return Err(StoreError::Full);
        }
        let result = write.await;
        if let Err(err) = &result
            && is_storage_failure(err)
        {
            self.degrade(&mut st, name, err);
        }
        result
    }

    /// Run a queueable write directly while ok. `None` means queue it: either
    /// storage was already degraded, or this write just failed and degraded it.
    async fn direct<T, F>(
        &self,
        st: &mut MutexGuard<'_, State>,
        name: &str,
        write: F,
    ) -> Option<Result<T, StoreError>>
    where
        F: Future<Output = Result<T, StoreError>>,
    {
        if st.degraded.is_some() {
            return None;
        }
        match write.await {
            Err(err) if is_storage_failure(&err) => {
                self.degrade(st, name, &err);
                None
            }
            other => Some(other),
        }
    }

    async fn log_state_in(
        &self,
        st: &mut MutexGuard<'_, State>,
        run: &RunId,
        state: LogState,
    ) -> Result<Run, StoreError> {
        if let Some(result) = self
            .direct(st, "set_log_state", self.inner.set_log_state(run, state))
            .await
        {
            return result;
        }
        let mut next = self.run_now(st, run).await?;
        next.log_state = state;
        st.runs.insert(run.clone(), next.clone());
        self.enqueue(
            st,
            Queued::LogState {
                run: run.clone(),
                state,
            },
        );
        Ok(next)
    }
}

#[async_trait::async_trait]
impl Store for QueuedStore {
    async fn create_project(
        &self,
        actor: Actor,
        new: NewProject,
    ) -> Result<Committed<Project>, StoreError> {
        self.unqueued("create_project", self.inner.create_project(actor, new))
            .await
    }

    async fn find_project_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<Project>, StoreError> {
        self.inner.find_project_by_repo_path(repo_path).await
    }

    async fn create_task(
        &self,
        actor: Actor,
        new: NewTask,
    ) -> Result<Committed<(Task, Vec<Workspace>)>, StoreError> {
        self.unqueued("create_task", self.inner.create_task(actor, new))
            .await
    }

    async fn add_workspace(
        &self,
        actor: Actor,
        task: &TaskId,
        new: NewWorkspace,
    ) -> Result<Committed<Workspace>, StoreError> {
        self.unqueued("add_workspace", self.inner.add_workspace(actor, task, new))
            .await
    }

    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Committed<Run>, StoreError> {
        let committed = self
            .unqueued("create_run", self.inner.create_run(actor, new))
            .await?;
        self.state.lock().await.remember(&committed.value);
        Ok(committed)
    }

    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError> {
        {
            let st = self.state.lock().await;
            if st.degraded.is_some()
                && let Some(known) = st.runs.get(run)
            {
                return Ok(known.clone());
            }
        }
        self.inner.get_run(run).await
    }

    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Committed<Run>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "transition_run",
                self.inner.transition_run(actor, run, change),
            )
            .await
        {
            if let Ok(committed) = &result {
                st.remember(&committed.value);
            }
            return result;
        }
        let current = self.run_now(&st, run).await?;
        let now = self.clock.now_ms();
        let Some(next) = current.transition(change, now)? else {
            return Ok(Committed {
                value: current,
                events: Vec::new(),
            });
        };
        st.runs.insert(run.clone(), next.clone());
        self.enqueue(
            &mut st,
            Queued::Transition {
                actor,
                run: run.clone(),
                change,
            },
        );
        let _ = self
            .notices
            .send(StorageNotice::Unjournaled(EventKind::RunStatusChanged {
                run_id: run.clone(),
                workspace_id: next.workspace_id.clone(),
                from: current.status,
                status: next.status,
                end_reason: next.end_reason,
                exit_code: next.exit_code,
                at: now,
            }));
        Ok(Committed {
            value: next,
            events: Vec::new(),
        })
    }

    async fn mark_live_runs_lost(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Run>>, StoreError> {
        self.unqueued(
            "mark_live_runs_lost",
            self.inner.mark_live_runs_lost(actor, at),
        )
        .await
    }

    async fn discard_preparing_workspaces(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Workspace>>, StoreError> {
        self.unqueued(
            "discard_preparing_workspaces",
            self.inner.discard_preparing_workspaces(actor, at),
        )
        .await
    }

    async fn reconcile_log_bytes(
        &self,
        files: Vec<(RunId, u64)>,
    ) -> Result<LogReconcile, StoreError> {
        self.unqueued("reconcile_log_bytes", self.inner.reconcile_log_bytes(files))
            .await
    }

    async fn events_since(
        &self,
        cursor: i64,
        limit: u32,
        filter: EventFilter,
    ) -> Result<EventPage, StoreError> {
        self.inner.events_since(cursor, limit, filter).await
    }

    async fn get_project(&self, project: &ProjectId) -> Result<Project, StoreError> {
        self.inner.get_project(project).await
    }

    async fn list_projects(
        &self,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Project>, StoreError> {
        self.inner.list_projects(include_archived, query).await
    }

    async fn archive_project(
        &self,
        actor: Actor,
        project: &ProjectId,
    ) -> Result<Committed<Project>, StoreError> {
        self.unqueued(
            "archive_project",
            self.inner.archive_project(actor, project),
        )
        .await
    }

    async fn delete_project(&self, project: &ProjectId) -> Result<Vec<RunId>, StoreError> {
        self.unqueued("delete_project", self.inner.delete_project(project))
            .await
    }

    async fn set_project_setting(
        &self,
        actor: Actor,
        project: &ProjectId,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<i64>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_project_setting",
                self.inner
                    .set_project_setting(actor, project, key, value.clone()),
            )
            .await
        {
            return result;
        }
        // The revision it will reach once the queue lands (each queued change bumps it).
        let base = self.inner.project_settings(project).await?.revision;
        let pending = st
            .queue
            .iter()
            .filter(|q| matches!(q, Queued::ProjectSetting { project: p, .. } if p == project))
            .count();
        let revision = base + i64::try_from(pending).unwrap_or(i64::MAX) + 1;
        self.enqueue(
            &mut st,
            Queued::ProjectSetting {
                actor,
                project: project.clone(),
                key: key.to_owned(),
                value,
            },
        );
        Ok(Committed {
            value: revision,
            events: Vec::new(),
        })
    }

    async fn project_settings(&self, project: &ProjectId) -> Result<ProjectSettings, StoreError> {
        self.inner.project_settings(project).await
    }

    async fn set_setting(
        &self,
        actor: Actor,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<()>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_setting",
                self.inner.set_setting(actor, key, value.clone()),
            )
            .await
        {
            return result;
        }
        self.enqueue(
            &mut st,
            Queued::Setting {
                actor,
                key: key.to_owned(),
                value,
            },
        );
        Ok(Committed {
            value: (),
            events: Vec::new(),
        })
    }

    async fn get_setting(&self, key: &str) -> Result<Option<JsonValue>, StoreError> {
        self.inner.get_setting(key).await
    }

    async fn get_task(&self, task: &TaskId) -> Result<Task, StoreError> {
        self.inner.get_task(task).await
    }

    async fn list_tasks(
        &self,
        project: &ProjectId,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Task>, StoreError> {
        self.inner
            .list_tasks(project, include_archived, query)
            .await
    }

    async fn archive_task(
        &self,
        actor: Actor,
        task: &TaskId,
    ) -> Result<Committed<Task>, StoreError> {
        self.unqueued("archive_task", self.inner.archive_task(actor, task))
            .await
    }

    async fn delete_task(&self, task: &TaskId) -> Result<Vec<RunId>, StoreError> {
        self.unqueued("delete_task", self.inner.delete_task(task))
            .await
    }

    async fn get_workspace(&self, workspace: &WorkspaceId) -> Result<Workspace, StoreError> {
        {
            let st = self.state.lock().await;
            if let Some(known) = st.workspaces.get(workspace) {
                return Ok(known.clone());
            }
        }
        self.inner.get_workspace(workspace).await
    }

    async fn list_workspaces(
        &self,
        task: &TaskId,
        query: ListQuery,
    ) -> Result<Page<Workspace>, StoreError> {
        self.inner.list_workspaces(task, query).await
    }

    async fn set_workspace_state(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        change: WorkspaceChange,
    ) -> Result<Committed<Workspace>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_workspace_state",
                self.inner
                    .set_workspace_state(actor, workspace, change.clone()),
            )
            .await
        {
            return result;
        }
        let current = self.workspace_now(&st, workspace).await?;
        let now = self.clock.now_ms();
        let Some(next) = current.change_state(change.clone(), now)? else {
            return Ok(Committed {
                value: current,
                events: Vec::new(),
            });
        };
        st.workspaces.insert(workspace.clone(), next.clone());
        self.enqueue(
            &mut st,
            Queued::WorkspaceState {
                actor,
                workspace: workspace.clone(),
                change,
            },
        );
        Ok(Committed {
            value: next,
            events: Vec::new(),
        })
    }

    async fn set_workspace_outcome(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        outcome: WorkspaceOutcome,
    ) -> Result<Committed<Workspace>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_workspace_outcome",
                self.inner.set_workspace_outcome(actor, workspace, outcome),
            )
            .await
        {
            return result;
        }
        let current = self.workspace_now(&st, workspace).await?;
        let siblings = self
            .inner
            .list_workspaces(
                &current.task_id,
                ListQuery {
                    after: None,
                    limit: 2,
                },
            )
            .await?
            .items
            .len();
        let Some(next) = current.with_outcome(outcome, siblings)? else {
            return Ok(Committed {
                value: current,
                events: Vec::new(),
            });
        };
        st.workspaces.insert(workspace.clone(), next.clone());
        self.enqueue(
            &mut st,
            Queued::WorkspaceOutcome {
                actor,
                workspace: workspace.clone(),
                outcome,
            },
        );
        Ok(Committed {
            value: next,
            events: Vec::new(),
        })
    }

    async fn list_runs(
        &self,
        filter: RunFilter,
        query: ListQuery,
    ) -> Result<Page<Run>, StoreError> {
        self.inner.list_runs(filter, query).await
    }

    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_run_process",
                self.inner.set_run_process(run, process),
            )
            .await
        {
            if let Ok(row) = &result {
                st.remember(row);
            }
            return result;
        }
        let mut next = self.run_now(&st, run).await?;
        next.pid = Some(process.pid);
        next.pgid = Some(process.pgid);
        next.pid_started_at = Some(process.pid_started_at);
        st.runs.insert(run.clone(), next.clone());
        self.enqueue(
            &mut st,
            Queued::RunProcess {
                run: run.clone(),
                process,
            },
        );
        Ok(next)
    }

    async fn set_agent_session_ref(
        &self,
        run: &RunId,
        session_ref: &str,
    ) -> Result<Run, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "set_agent_session_ref",
                self.inner.set_agent_session_ref(run, session_ref),
            )
            .await
        {
            if let Ok(row) = &result {
                st.remember(row);
            }
            return result;
        }
        let mut next = self.run_now(&st, run).await?;
        next.agent_session_ref = Some(session_ref.to_owned());
        st.runs.insert(run.clone(), next.clone());
        self.enqueue(
            &mut st,
            Queued::SessionRef {
                run: run.clone(),
                session_ref: session_ref.to_owned(),
            },
        );
        Ok(next)
    }

    async fn set_log_state(&self, run: &RunId, state: LogState) -> Result<Run, StoreError> {
        let mut st = self.state.lock().await;
        self.log_state_in(&mut st, run, state).await
    }

    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<Committed<()>, StoreError> {
        let mut st = self.state.lock().await;
        if let Some(result) = self
            .direct(
                &mut st,
                "append_event",
                self.inner
                    .append_event(actor, actor_ref.clone(), run, event.clone()),
            )
            .await
        {
            return result;
        }
        self.enqueue(
            &mut st,
            Queued::Event {
                actor,
                actor_ref,
                run: run.clone(),
                event,
            },
        );
        Ok(Committed {
            value: (),
            events: Vec::new(),
        })
    }

    async fn log_total_ended(&self) -> Result<u64, StoreError> {
        self.inner.log_total_ended().await
    }

    async fn log_trim_candidates(&self) -> Result<Vec<TrimCandidate>, StoreError> {
        self.inner.log_trim_candidates().await
    }

    async fn mark_log_trimmed(
        &self,
        actor: Actor,
        run: &RunId,
    ) -> Result<Committed<u64>, StoreError> {
        self.unqueued("mark_log_trimmed", self.inner.mark_log_trimmed(actor, run))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InvalidTransition, RunStatus};

    #[test]
    fn only_storage_failures_should_be_waited_out_and_refusals_should_pass_through() {
        // covers: AC-13 (disk full, an I/O error or the database thread gone degrade; a refusal never does)
        let waits = [
            StoreError::Full,
            StoreError::Io("disk I/O error".into()),
            StoreError::Unavailable,
        ];
        let refusals = [
            StoreError::NotFound,
            StoreError::ProjectExists,
            StoreError::RunAlreadyLive,
            StoreError::BranchInUse,
            StoreError::WinnerExists,
            StoreError::WorktreePathInUse,
            StoreError::NotArchived,
            StoreError::HasLiveRuns,
            StoreError::InvalidTransition(InvalidTransition {
                from: RunStatus::Stopped,
                to: RunStatus::Running,
            }),
        ];

        assert!(waits.iter().all(is_storage_failure));
        assert!(!refusals.iter().any(is_storage_failure));
    }
}
