//! The storage ports (spec 0002, *Ports*): implemented by `sy-infra`
//! (`SqliteStore`, `FileRunLog`, `SystemClock`) and by test doubles.

use crate::ids::{ProjectId, RunId, TaskId, WorkspaceId};
use crate::model::{
    Actor, AdapterEvent, Event, InvalidTransition, InvalidWorkspaceTransition, JsonValue, LogState,
    NewProject, NewRun, NewTask, NewWorkspace, NotAComparison, Project, ProjectSettings, Run,
    RunStatus, RunTransition, Task, Workspace, WorkspaceChange, WorkspaceOutcome,
};

/// The time source for every `*_at` value.
pub trait Clock: Send + Sync {
    /// Now, as Unix milliseconds UTC.
    fn now_ms(&self) -> i64;
}

/// Why a store command failed. Constraint violations map to their own variant,
/// never to a raw SQLite error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The row the command names does not exist.
    #[error("not found")]
    NotFound,
    /// Another project already uses this repository path (`uq_project_repo_path`).
    #[error("a project already uses this repository path")]
    ProjectExists,
    /// The workspace already has a live agent run (`uq_run_live_agent`).
    #[error("this workspace already has a live agent run")]
    RunAlreadyLive,
    /// The branch is taken by a workspace in this project that isn't discarded (`uq_workspace_branch`).
    #[error("this branch is already used in the project")]
    BranchInUse,
    /// The task already has a winner (`uq_workspace_winner`).
    #[error("this task already has a winner")]
    WinnerExists,
    /// Another workspace uses this worktree path (`uq_workspace_worktree_path`).
    #[error("another workspace uses this worktree path")]
    WorktreePathInUse,
    /// The run status machine refused the change.
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
    /// The workspace lifecycle refused the change.
    #[error(transparent)]
    InvalidWorkspaceTransition(#[from] InvalidWorkspaceTransition),
    /// An outcome was set on a task that isn't a comparison.
    #[error(transparent)]
    NotAComparison(#[from] NotAComparison),
    /// Permanent delete needs the row archived first.
    #[error("archive it before deleting it")]
    NotArchived,
    /// The command needs every run to have ended first.
    #[error("it still has a live run")]
    HasLiveRuns,
    /// Storage is not open (degraded mode), or its thread has stopped.
    #[error("storage is unavailable")]
    Unavailable,
    /// The disk is full.
    #[error("the disk is full")]
    Full,
    /// Any other database or I/O failure. The text never holds row data.
    #[error("storage error: {0}")]
    Io(String),
}

/// A command's result plus the events it committed (empty for a no op), with
/// their journal ids, for the caller to broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed<T> {
    /// What the command returns.
    pub value: T,
    /// The events committed with it, in id order.
    pub events: Vec<Event>,
}

/// A page of journal events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPage {
    /// Events with `id > cursor`, ascending.
    pub events: Vec<Event>,
    /// The id of the last event returned, or the input cursor when none.
    pub next_cursor: i64,
    /// True when more events match after this page.
    pub has_more: bool,
}

/// Filters for [`Store::events_since`]; set filters combine with AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    /// Only this project's events.
    pub project_id: Option<ProjectId>,
    /// Only this task's events.
    pub task_id: Option<TaskId>,
    /// Only this workspace's events.
    pub workspace_id: Option<WorkspaceId>,
    /// Only this run's events.
    pub run_id: Option<RunId>,
}

/// What [`Store::reconcile_log_bytes`] changed and what it leaves for the
/// caller to remove from `runs/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogReconcile {
    /// Runs newly flagged `deleted`: they had output, and their file is gone.
    pub deleted: Vec<RunId>,
    /// Log files to remove: files with no run row, and files of `trimmed` or
    /// `deleted` runs.
    pub remove: Vec<RunId>,
}

/// Default page size for list and journal reads.
pub const DEFAULT_PAGE: u32 = 200;
/// Largest page size for list and journal reads.
pub const MAX_PAGE: u32 = 500;

/// The effective page size for a requested `limit`: 0 means [`DEFAULT_PAGE`],
/// and it is capped at [`MAX_PAGE`].
pub const fn page_size(limit: u32) -> u32 {
    match limit {
        0 => DEFAULT_PAGE,
        n if n > MAX_PAGE => MAX_PAGE,
        n => n,
    }
}

/// A keyset position: the sort time (`created_at`, or `queued_at` for runs)
/// and id of the last row a page returned. The next page starts after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyset {
    /// The last row's sort time.
    pub at: i64,
    /// The last row's id text.
    pub id: String,
}

/// Where a list read starts and how much it returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Start after this row; `None` for the first page.
    pub after: Option<Keyset>,
    /// Page size, by [`page_size`].
    pub limit: u32,
}

/// One page of a list read, in ascending (sort time, id) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The rows.
    pub items: Vec<T>,
    /// Pass as [`ListQuery::after`] for the next page; `None` on the last page.
    pub next: Option<Keyset>,
}

/// Which runs [`Store::list_runs`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFilter {
    /// The runs of one workspace.
    Workspace(WorkspaceId),
    /// Every run in one of these statuses (for reconcile).
    Status(Vec<RunStatus>),
}

/// One run log the log cap may trim (AC-10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimCandidate {
    /// The ended run.
    pub run_id: RunId,
    /// Its stored log size.
    pub log_bytes: u64,
}

/// The process a run launched, for `Store::set_run_process`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunProcess {
    /// Process id.
    pub pid: i64,
    /// Process group id.
    pub pgid: i64,
    /// The process start time, to tell a live orphan from a reused pid.
    pub pid_started_at: i64,
}

/// The relational state and the event journal.
///
/// Every command is one transaction that writes its rows and its events
/// together. Committed events are also pushed, in id order, to the ordered
/// channel the implementation was built with, which the broadcaster drains.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Create a project.
    async fn create_project(
        &self,
        actor: Actor,
        new: NewProject,
    ) -> Result<Committed<Project>, StoreError>;

    /// The project with this repository path, if any.
    async fn find_project_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<Project>, StoreError>;

    /// Create a task and its first workspaces, atomically. Each workspace's
    /// `project_id` is copied from the task inside the insert.
    async fn create_task(
        &self,
        actor: Actor,
        new: NewTask,
    ) -> Result<Committed<(Task, Vec<Workspace>)>, StoreError>;

    /// Add a workspace to a task. Its `project_id` is copied from the task.
    async fn add_workspace(
        &self,
        actor: Actor,
        task: &TaskId,
        new: NewWorkspace,
    ) -> Result<Committed<Workspace>, StoreError>;

    /// Create a run, `queued`, with the id the caller minted.
    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Committed<Run>, StoreError>;

    /// The run with this id.
    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError>;

    /// Change a run's status through the status machine. A report of the
    /// current status is a no op (no write, no event).
    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Committed<Run>, StoreError>;

    /// Startup recovery (AC-6): every run in `starting`, `running` or
    /// `waiting_input` becomes `lost`, with `end_reason = engine_restart` and
    /// `ended_at = at` (the engine's start time), and one `run_status_changed`
    /// event each. Returns the runs as they are now.
    async fn mark_live_runs_lost(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Run>>, StoreError>;

    /// Startup recovery (AC-6): every workspace still in `preparing` becomes
    /// `discarded`, with `closed_at = at` and one `workspace_state_changed`
    /// event each. Returns the workspaces as they are now.
    async fn discard_preparing_workspaces(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Workspace>>, StoreError>;

    /// Startup recovery (AC-6): reconcile every run's `log_bytes` and
    /// `log_state` with `files` (every log file in `runs/` with its size, from
    /// [`RunLog::list`]), by [`LogState::reconcile`](crate::model::LogState::reconcile).
    /// Writes no event.
    async fn reconcile_log_bytes(
        &self,
        files: Vec<(RunId, u64)>,
    ) -> Result<LogReconcile, StoreError>;

    /// Journal events with `id > cursor`, ascending. `limit` 0 means
    /// [`DEFAULT_PAGE`], and it is capped at [`MAX_PAGE`].
    async fn events_since(
        &self,
        cursor: i64,
        limit: u32,
        filter: EventFilter,
    ) -> Result<EventPage, StoreError>;

    // ---- projects ----

    /// The project with this id.
    async fn get_project(&self, project: &ProjectId) -> Result<Project, StoreError>;

    /// Projects by (`created_at`, `id`); archived ones only when asked.
    async fn list_projects(
        &self,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Project>, StoreError>;

    /// Archive a project: set `archived_at` and write one `project_archived`
    /// event; its rows and logs stay. Already archived is a no op. Fails with
    /// `HasLiveRuns` while any of its runs is queued or live.
    async fn archive_project(
        &self,
        actor: Actor,
        project: &ProjectId,
    ) -> Result<Committed<Project>, StoreError>;

    /// Permanently delete an archived project and everything under it, in one
    /// transaction, writing no event. Returns the removed run ids, whose log
    /// files the caller removes. `NotArchived` unless archived, `HasLiveRuns`
    /// while any run is queued or live.
    async fn delete_project(&self, project: &ProjectId) -> Result<Vec<RunId>, StoreError>;

    /// Set a project setting, bump `settings_revision`, and write one
    /// `project_setting_changed` event. The same value again is a no op.
    /// Returns the revision now.
    async fn set_project_setting(
        &self,
        actor: Actor,
        project: &ProjectId,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<i64>, StoreError>;

    /// A project's settings and revision.
    async fn project_settings(&self, project: &ProjectId) -> Result<ProjectSettings, StoreError>;

    // ---- global settings ----

    /// Set a global setting and write one `setting_changed` event (none for
    /// [`STORAGE_PROBE_KEY`](crate::model::STORAGE_PROBE_KEY), which always
    /// writes). The same value again is a no op.
    async fn set_setting(
        &self,
        actor: Actor,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<()>, StoreError>;

    /// A global setting, if set.
    async fn get_setting(&self, key: &str) -> Result<Option<JsonValue>, StoreError>;

    // ---- tasks ----

    /// The task with this id.
    async fn get_task(&self, task: &TaskId) -> Result<Task, StoreError>;

    /// A project's tasks by (`created_at`, `id`); archived ones only when asked.
    async fn list_tasks(
        &self,
        project: &ProjectId,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Task>, StoreError>;

    /// Archive a task: set `archived_at` and write one `task_archived` event;
    /// its rows and logs stay. Already archived is a no op. Fails with
    /// `HasLiveRuns` while any of its runs is queued or live.
    async fn archive_task(
        &self,
        actor: Actor,
        task: &TaskId,
    ) -> Result<Committed<Task>, StoreError>;

    /// Permanently delete a task that counts as archived (its own
    /// `archived_at`, or its project's) and everything under it, in one
    /// transaction, writing no event. Returns the removed run ids.
    async fn delete_task(&self, task: &TaskId) -> Result<Vec<RunId>, StoreError>;

    // ---- workspaces ----

    /// The workspace with this id.
    async fn get_workspace(&self, workspace: &WorkspaceId) -> Result<Workspace, StoreError>;

    /// A task's workspaces by (`created_at`, `id`).
    async fn list_workspaces(
        &self,
        task: &TaskId,
        query: ListQuery,
    ) -> Result<Page<Workspace>, StoreError>;

    /// Move a workspace through its lifecycle, writing one
    /// `workspace_state_changed` event. The current state is a no op.
    async fn set_workspace_state(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        change: WorkspaceChange,
    ) -> Result<Committed<Workspace>, StoreError>;

    /// Set a workspace's comparison outcome, writing one
    /// `workspace_outcome_set` event. `NotAComparison` on a task with fewer
    /// than two workspaces, `WinnerExists` for a second winner.
    async fn set_workspace_outcome(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        outcome: WorkspaceOutcome,
    ) -> Result<Committed<Workspace>, StoreError>;

    // ---- runs ----

    /// Runs by (`queued_at`, `id`).
    async fn list_runs(&self, filter: RunFilter, query: ListQuery)
    -> Result<Page<Run>, StoreError>;

    /// Record the process a run launched. Writes no event.
    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError>;

    /// Record the agent's own session handle, for resume. Writes no event.
    async fn set_agent_session_ref(
        &self,
        run: &RunId,
        session_ref: &str,
    ) -> Result<Run, StoreError>;

    /// Set a run's `log_state` (to `incomplete` after a failed append). Writes
    /// no event; trimming goes through [`Store::mark_log_trimmed`].
    async fn set_log_state(&self, run: &RunId, state: LogState) -> Result<Run, StoreError>;

    // ---- journal ----

    /// Journal an adapter reported event on a run, filed under the run and
    /// every ancestor. `actor_ref` names the acting workspace for `agent`.
    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<Committed<()>, StoreError>;

    // ---- log cap (AC-10) ----

    /// The SUM of `log_bytes` over ended runs whose `log_state` is `present`
    /// or `incomplete`.
    async fn log_total_ended(&self) -> Result<u64, StoreError>;

    /// Ended runs with saved output whose task counts as archived
    /// (`COALESCE(task.archived_at, project.archived_at)` set), oldest archive
    /// time first, then oldest `queued_at`, then id.
    async fn log_trim_candidates(&self) -> Result<Vec<TrimCandidate>, StoreError>;

    /// Commit `log_state = trimmed` (and `log_bytes = 0`) with one
    /// `run_log_trimmed` event, before the caller unlinks the file. Returns
    /// the bytes trimmed. A run that is no longer a candidate (live, not
    /// archived, or already trimmed) is a no op returning 0 and no event.
    async fn mark_log_trimmed(
        &self,
        actor: Actor,
        run: &RunId,
    ) -> Result<Committed<u64>, StoreError>;
}

/// Why a run log operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunLogError {
    /// The disk is full.
    #[error("the disk is full")]
    Full,
    /// The run has no log file.
    #[error("no log file for this run")]
    NotFound,
    /// Any other I/O failure.
    #[error("run log error: {0}")]
    Io(String),
}

/// Each run's raw terminal output, one append only file per run. The byte
/// position in the file equals the protocol's per run `offset`.
///
/// Synchronous: called from the run hub's flusher thread, and from
/// `spawn_blocking` elsewhere.
pub trait RunLog: Send + Sync {
    /// Write `bytes` at exactly `offset` (creating the file if needed), so a
    /// retried or reordered call can never shift bytes.
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError>;

    /// Cut the file back to `len` bytes (after a failed append).
    fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError>;

    /// Up to `max` bytes from `offset`. Empty at or past the end.
    fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError>;

    /// Bytes in the file now.
    fn len(&self, run: &RunId) -> Result<u64, RunLogError>;

    /// Remove the file. Removing a missing file is not an error.
    fn delete(&self, run: &RunId) -> Result<(), RunLogError>;

    /// Every log file whose name is a valid run id, with its size.
    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError>;
}
