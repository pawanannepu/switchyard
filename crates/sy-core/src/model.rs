//! The domain entities (spec 0002, *Data model* and *State transitions*).
//!
//! Only [`Run`] stores a status. Workspace and task status are derived by
//! [`workspace_display_status`] and [`task_display_status`], never stored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sy_adapter_api::serde_json;

use crate::ids::{ApprovalId, ProjectId, RunId, TaskId, WorkspaceId};

/// A JSON value: a setting's value, stored in a JSON column and carried in
/// setting events. `sy-core` takes no JSON library (spec 0001's crate table),
/// so this is the plain serde shape of one; any serde format round trips it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonValue {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer that fits `i64`.
    Int(i64),
    /// An integer above `i64::MAX`.
    UInt(u64),
    /// Any other number.
    Float(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<JsonValue>),
    /// An object, keys in sorted order.
    Object(BTreeMap<String, JsonValue>),
}

// JSON has no NaN, so equality is total for every value a JSON text can hold.
impl Eq for JsonValue {}

impl JsonValue {
    /// The value as a non negative integer, if it is one.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Int(n) => u64::try_from(*n).ok(),
            Self::UInt(n) => Some(*n),
            _ => None,
        }
    }
}

/// A project's settings and the revision they're at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectSettings {
    /// `project.settings_revision`, bumped on every change.
    pub revision: i64,
    /// Every setting, by key.
    pub values: BTreeMap<String, JsonValue>,
}

/// The global setting key the degraded write probe writes (AC-13). It is never journaled.
pub const STORAGE_PROBE_KEY: &str = "storage.probe";

/// Declares a closed set of text values (stored as TEXT with a `CHECK`, sent as snake case).
macro_rules! text_enum {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident = $text:literal,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vdoc])* $variant,)*
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant,)*];

            /// The stored and wire text.
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text,)* }
            }

            /// The value for its text, or `None` for unknown text.
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some($name::$variant),)* _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(
    /// A run's status, the only stored status in the model.
    RunStatus {
        /// Waiting for a slot (feature 26's queue).
        Queued = "queued",
        /// The process is being launched.
        Starting = "starting",
        /// The process is working.
        Running = "running",
        /// The agent asked for input.
        WaitingInput = "waiting_input",
        /// The process exited on its own.
        Finished = "finished",
        /// The process failed (or could not start).
        Failed = "failed",
        /// Stopped on request.
        Stopped = "stopped",
        /// The engine restarted while it was live, so its real end is unknown.
        Lost = "lost",
    }
);

impl RunStatus {
    /// True for `finished`, `failed`, `stopped` and `lost`: the status never changes again.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Finished | Self::Failed | Self::Stopped | Self::Lost
        )
    }

    /// True for `starting`, `running` and `waiting_input`, the states a restart turns into `lost`.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::WaitingInput)
    }

    /// True for `queued` plus the live states (the set `uq_run_live_agent` and `HasLiveRuns` use).
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued) || self.is_live()
    }

    /// Whether `transition_run` may move a run from `self` to `to`.
    /// `lost` is never allowed here; only startup recovery sets it.
    pub const fn can_transition_to(self, to: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, to),
            (Queued, Starting | Stopped)
                | (
                    Starting,
                    Running | WaitingInput | Finished | Failed | Stopped
                )
                | (Running, WaitingInput | Finished | Failed | Stopped)
                | (WaitingInput, Running | Finished | Failed | Stopped)
        )
    }
}

text_enum!(
    /// What a run is.
    RunKind {
        /// An agent launch, follow up or resume.
        Agent = "agent",
        /// A project setup script (feature 13).
        Setup = "setup",
        /// A project run script (feature 13).
        RunScript = "run_script",
    }
);

text_enum!(
    /// Whether a run's output log is still on disk.
    LogState {
        /// The file holds everything the run wrote (or the run wrote nothing yet).
        Present = "present",
        /// An append failed, so the file holds only a prefix of the output.
        Incomplete = "incomplete",
        /// Removed by the log cap (AC-10).
        Trimmed = "trimmed",
        /// The file vanished outside the engine.
        Deleted = "deleted",
    }
);

text_enum!(
    /// A workspace's lifecycle state.
    WorkspaceState {
        /// The worktree is being created or set up.
        Preparing = "preparing",
        /// Ready for runs.
        Ready = "ready",
        /// Merged into the base branch.
        Merged = "merged",
        /// Thrown away.
        Discarded = "discarded",
    }
);

text_enum!(
    /// A workspace's result in a comparison (a task with 2 or more workspaces).
    WorkspaceOutcome {
        /// The chosen workspace.
        Winner = "winner",
        /// Compared and not chosen.
        NotChosen = "not_chosen",
    }
);

text_enum!(
    /// Who caused an event.
    Actor {
        /// You, through a client.
        User = "user",
        /// The engine itself (startup recovery, trimming).
        Engine = "engine",
        /// An approval policy (feature 12).
        Policy = "policy",
        /// An agent acting through the engine (feature 33).
        Agent = "agent",
    }
);

text_enum!(
    /// Why a run ended. Later features add values; stored without a `CHECK`.
    EndReason {
        /// The process exited.
        Exited = "exited",
        /// You stopped it.
        UserStop = "user_stop",
        /// The engine restarted while it was live.
        EngineRestart = "engine_restart",
        /// The process could not be started.
        SpawnFailed = "spawn_failed",
        /// A time or output limit stopped it.
        LimitExceeded = "limit_exceeded",
    }
);

/// A git repository switchyard works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project id.
    pub id: ProjectId,
    /// Display name.
    pub name: String,
    /// The repository path, stored exactly as given (feature 8 canonicalises it).
    pub repo_path: String,
    /// The branch new tasks start from by default.
    pub base_branch: String,
    /// Bumped on every project setting change.
    pub settings_revision: i64,
    /// Creation time, Unix ms UTC.
    pub created_at: i64,
    /// Archive time, when archived.
    pub archived_at: Option<i64>,
}

/// Input for `Store::create_project`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// Display name.
    pub name: String,
    /// The repository path, stored as given.
    pub repo_path: String,
    /// The default base branch.
    pub base_branch: String,
}

/// The intent: a prompt against a base ref, worked on in one or more workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task id.
    pub id: TaskId,
    /// The project it belongs to.
    pub project_id: ProjectId,
    /// Short title.
    pub title: String,
    /// The prompt given to every workspace's agent.
    pub prompt: String,
    /// The git ref the workspaces branch from.
    pub base_ref: String,
    /// The task this one was handed back from (a CI failure, for example).
    pub parent_task_id: Option<TaskId>,
    /// Creation time, Unix ms UTC.
    pub created_at: i64,
    /// Archive time, when archived.
    pub archived_at: Option<i64>,
}

/// Input for `Store::create_task`: the task and its first workspaces, created together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    /// The project to create it in.
    pub project_id: ProjectId,
    /// Short title.
    pub title: String,
    /// The prompt.
    pub prompt: String,
    /// The git ref to branch from.
    pub base_ref: String,
    /// The task this one continues, if any.
    pub parent_task_id: Option<TaskId>,
    /// The first workspaces (zero or more).
    pub workspaces: Vec<NewWorkspace>,
}

/// One worktree, branch and agent working on a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The workspace id.
    pub id: WorkspaceId,
    /// The task it works on.
    pub task_id: TaskId,
    /// The task's project, copied from the task by the store.
    pub project_id: ProjectId,
    /// The agent adapter id (no foreign key, a snapshot).
    pub agent_id: String,
    /// The agent's options, as JSON text.
    pub agent_options: String,
    /// The git branch.
    pub branch: String,
    /// The worktree's path.
    pub worktree_path: String,
    /// The commit the branch started from.
    pub base_commit: String,
    /// Lifecycle state.
    pub state: WorkspaceState,
    /// Comparison result, set only on a task with 2 or more workspaces.
    pub outcome: Option<WorkspaceOutcome>,
    /// The merge commit, once merged.
    pub merged_commit: Option<String>,
    /// Creation time, Unix ms UTC.
    pub created_at: i64,
    /// Set on entering `merged` or `discarded`.
    pub closed_at: Option<i64>,
}

/// Input for a new workspace. The caller mints the id because the branch and
/// worktree path usually contain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspace {
    /// The new workspace's id.
    pub id: WorkspaceId,
    /// The agent adapter id.
    pub agent_id: String,
    /// The agent's options, as JSON text (the database checks it is valid JSON).
    pub agent_options: String,
    /// The git branch.
    pub branch: String,
    /// The worktree's path.
    pub worktree_path: String,
    /// The commit the branch starts from.
    pub base_commit: String,
    /// Its first state.
    pub state: InitialWorkspaceState,
}

/// The states a workspace may be created in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialWorkspaceState {
    /// The worktree still needs creating or setting up.
    Preparing,
    /// Ready for runs at once.
    Ready,
}

impl From<InitialWorkspaceState> for WorkspaceState {
    fn from(state: InitialWorkspaceState) -> Self {
        match state {
            InitialWorkspaceState::Preparing => Self::Preparing,
            InitialWorkspaceState::Ready => Self::Ready,
        }
    }
}

/// One process with its own output: an agent launch, follow up or resume, or a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The run id.
    pub id: RunId,
    /// The workspace it runs in.
    pub workspace_id: WorkspaceId,
    /// What it is.
    pub kind: RunKind,
    /// Follow up text, if any.
    pub input: Option<String>,
    /// The run this one resumes (feature 23).
    pub resumes_run_id: Option<RunId>,
    /// The agent's own session handle, for resume (feature 4).
    pub agent_session_ref: Option<String>,
    /// Current status.
    pub status: RunStatus,
    /// The exit code, when the process exited.
    pub exit_code: Option<i32>,
    /// Why it ended, once terminal.
    pub end_reason: Option<EndReason>,
    /// Process id, once launched.
    pub pid: Option<i64>,
    /// Process group id, once launched.
    pub pgid: Option<i64>,
    /// The process start time, to tell a live orphan from a reused pid.
    pub pid_started_at: Option<i64>,
    /// Creation time, Unix ms UTC.
    pub queued_at: i64,
    /// Set on entering `starting`.
    pub started_at: Option<i64>,
    /// Set on entering a terminal status.
    pub ended_at: Option<i64>,
    /// Bytes in the output log, written at the terminal transition and at startup.
    pub log_bytes: u64,
    /// Whether the output log is on disk.
    pub log_state: LogState,
}

/// Input for `Store::create_run`. The run starts `queued`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    /// The id the caller minted (the run hub uses the same id).
    pub id: RunId,
    /// The workspace to run in.
    pub workspace_id: WorkspaceId,
    /// What it is.
    pub kind: RunKind,
    /// Follow up text, if any.
    pub input: Option<String>,
    /// The run this one resumes, if any.
    pub resumes_run_id: Option<RunId>,
}

/// How a run ended: required on a transition into a terminal status, refused otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEnd {
    /// Why it ended.
    pub reason: EndReason,
    /// The exit code, when the process exited.
    pub exit_code: Option<i32>,
    /// The final length of the run's output log.
    pub log_bytes: u64,
}

/// Input for `Store::transition_run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTransition {
    /// The target status.
    pub to: RunStatus,
    /// How it ended, exactly when `to` is terminal.
    pub end: Option<RunEnd>,
}

impl RunTransition {
    /// A move to a non terminal status.
    pub const fn to(to: RunStatus) -> Self {
        Self { to, end: None }
    }

    /// A move to a terminal status.
    pub const fn end(to: RunStatus, end: RunEnd) -> Self {
        Self { to, end: Some(end) }
    }
}

/// A transition the status machine refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a run can't go from {from} to {to}")]
pub struct InvalidTransition {
    /// The run's current status.
    pub from: RunStatus,
    /// The refused target.
    pub to: RunStatus,
}

impl Run {
    /// Apply a status change at time `now`.
    ///
    /// `Ok(None)` is a no op (the run already has that status), so nothing is
    /// written and no event is made. Refuses `lost`, any pair not in the
    /// transition table, a terminal target without [`RunEnd`], and a non
    /// terminal target with one.
    pub fn transition(
        &self,
        change: RunTransition,
        now: i64,
    ) -> Result<Option<Run>, InvalidTransition> {
        let refuse = InvalidTransition {
            from: self.status,
            to: change.to,
        };
        if change.to == RunStatus::Lost {
            return Err(refuse);
        }
        if change.to == self.status {
            return Ok(None);
        }
        if !self.status.can_transition_to(change.to)
            || change.to.is_terminal() != change.end.is_some()
        {
            return Err(refuse);
        }
        let mut next = self.clone();
        next.status = change.to;
        if change.to == RunStatus::Starting {
            next.started_at = Some(now);
        }
        if let Some(end) = change.end {
            next.ended_at = Some(now);
            next.end_reason = Some(end.reason);
            next.exit_code = end.exit_code;
            next.log_bytes = end.log_bytes;
        }
        Ok(Some(next))
    }

    /// Mark a live run `lost` because the engine restarted (AC-6), ended at
    /// `at` (the engine's start time). Only startup recovery calls this.
    /// Refuses a run that isn't live (`queued` stays `queued`, and terminal
    /// states never change).
    pub fn lose(&self, at: i64) -> Result<Run, InvalidTransition> {
        if !self.status.is_live() {
            return Err(InvalidTransition {
                from: self.status,
                to: RunStatus::Lost,
            });
        }
        let mut next = self.clone();
        next.status = RunStatus::Lost;
        next.ended_at = Some(at);
        next.end_reason = Some(EndReason::EngineRestart);
        next.exit_code = None;
        Ok(next)
    }
}

/// A workspace state change, for `Store::set_workspace_state`. A merge always
/// names its merge commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceChange {
    /// `preparing → ready`.
    Ready,
    /// `ready → merged`, with the merge commit.
    Merged {
        /// The merge commit.
        commit: String,
    },
    /// `preparing | ready → discarded`.
    Discarded,
}

impl WorkspaceChange {
    /// The target state.
    pub const fn to(&self) -> WorkspaceState {
        match self {
            Self::Ready => WorkspaceState::Ready,
            Self::Merged { .. } => WorkspaceState::Merged,
            Self::Discarded => WorkspaceState::Discarded,
        }
    }
}

/// A workspace state change the lifecycle refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a workspace can't go from {from} to {to}")]
pub struct InvalidWorkspaceTransition {
    /// The workspace's current state.
    pub from: WorkspaceState,
    /// The refused target.
    pub to: WorkspaceState,
}

/// An outcome set on a task with fewer than [`MIN_COMPARISON`] workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an outcome needs a task with at least {MIN_COMPARISON} workspaces")]
pub struct NotAComparison;

/// The fewest workspaces a task needs before any of them gets an outcome.
pub const MIN_COMPARISON: usize = 2;

impl Workspace {
    /// Apply a state change at time `now`. `Ok(None)` is a no op (already in
    /// that state). Entering `merged` or `discarded` sets `closed_at`, and
    /// `merged` records the merge commit.
    pub fn change_state(
        &self,
        change: WorkspaceChange,
        now: i64,
    ) -> Result<Option<Workspace>, InvalidWorkspaceTransition> {
        let to = change.to();
        if to == self.state {
            return Ok(None);
        }
        if !self.state.can_transition_to(to) {
            return Err(InvalidWorkspaceTransition {
                from: self.state,
                to,
            });
        }
        let mut next = self.clone();
        next.state = to;
        if to.is_closed() {
            next.closed_at = Some(now);
        }
        if let WorkspaceChange::Merged { commit } = change {
            next.merged_commit = Some(commit);
        }
        Ok(Some(next))
    }

    /// Set the comparison outcome, given how many workspaces the task has.
    /// `Ok(None)` is a no op (already that outcome).
    pub fn with_outcome(
        &self,
        outcome: WorkspaceOutcome,
        workspaces_in_task: usize,
    ) -> Result<Option<Workspace>, NotAComparison> {
        if workspaces_in_task < MIN_COMPARISON {
            return Err(NotAComparison);
        }
        if self.outcome == Some(outcome) {
            return Ok(None);
        }
        Ok(Some(Workspace {
            outcome: Some(outcome),
            ..self.clone()
        }))
    }
}

impl WorkspaceState {
    /// Whether a workspace may move from `self` to `to`: `preparing → ready |
    /// discarded`, `ready → merged | discarded`. `merged` and `discarded` are final.
    pub const fn can_transition_to(self, to: WorkspaceState) -> bool {
        use WorkspaceState::*;
        matches!(
            (self, to),
            (Preparing, Ready | Discarded) | (Ready, Merged | Discarded)
        )
    }

    /// True for `merged` and `discarded`, the states that set `closed_at`.
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Merged | Self::Discarded)
    }
}

/// What startup reconcile does to one run's log fields (AC-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCheck {
    /// The new `log_bytes`: the file's real length, 0 when there's no file
    /// (or when the file is about to be removed).
    pub log_bytes: u64,
    /// The new `log_state`.
    pub log_state: LogState,
    /// The file exists but belongs to a `trimmed` or `deleted` run, so remove it.
    pub remove_file: bool,
}

impl LogState {
    /// Reconcile a run's stored log fields with its file on disk (`file_len`
    /// is `None` when there's no file). The file's length wins over the stored
    /// value. A `present` or `incomplete` run that had bytes (`stored_bytes >
    /// 0`) and lost its file becomes `deleted`; one that never wrote output has
    /// no file and stays as it is.
    pub const fn reconcile(self, stored_bytes: u64, file_len: Option<u64>) -> LogCheck {
        match (self, file_len) {
            (Self::Trimmed | Self::Deleted, file) => LogCheck {
                log_bytes: 0,
                log_state: self,
                remove_file: file.is_some(),
            },
            (Self::Present | Self::Incomplete, Some(len)) => LogCheck {
                log_bytes: len,
                log_state: self,
                remove_file: false,
            },
            (Self::Present | Self::Incomplete, None) => LogCheck {
                log_bytes: 0,
                log_state: if stored_bytes > 0 {
                    Self::Deleted
                } else {
                    self
                },
                remove_file: false,
            },
        }
    }
}

/// The ids an event is filed under. Every event fills in every ancestor it knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventScope {
    /// `None` only for global events.
    pub project_id: Option<ProjectId>,
    /// The task, when the event is about a task or something in it.
    pub task_id: Option<TaskId>,
    /// The workspace, when the event is about a workspace or a run in it.
    pub workspace_id: Option<WorkspaceId>,
    /// The run, when the event is about a run.
    pub run_id: Option<RunId>,
}

impl EventScope {
    /// The scope of a project event.
    pub fn project(project: &ProjectId) -> Self {
        Self {
            project_id: Some(project.clone()),
            ..Self::default()
        }
    }

    /// The scope of a task event.
    pub fn task(task: &Task) -> Self {
        Self {
            task_id: Some(task.id.clone()),
            ..Self::project(&task.project_id)
        }
    }

    /// The scope of a workspace event.
    pub fn workspace(ws: &Workspace) -> Self {
        Self {
            project_id: Some(ws.project_id.clone()),
            task_id: Some(ws.task_id.clone()),
            workspace_id: Some(ws.id.clone()),
            run_id: None,
        }
    }

    /// The scope of a run event in workspace `ws`.
    pub fn run(ws: &Workspace, run: &RunId) -> Self {
        Self {
            run_id: Some(run.clone()),
            ..Self::workspace(ws)
        }
    }
}

/// Who wrote a `message` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// You.
    User,
    /// The agent.
    Agent,
    /// The engine or the agent's system output.
    System,
}

/// How a file changed in a `file_changed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// A new file.
    Added,
    /// An edited file.
    Modified,
    /// A removed file.
    Deleted,
    /// A moved file (see `from_path`).
    Renamed,
}

/// Every journal event kind and its payload (spec 0002, *Journal event kinds*).
///
/// Serialised as `{ "kind": "...", "payload": { ... } }`. Payload fields may
/// only ever be added as optional; a breaking shape becomes a new kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventKind {
    /// A project was created.
    ProjectCreated {
        /// Display name.
        name: String,
        /// Repository path.
        repo_path: String,
        /// Default base branch.
        base_branch: String,
    },
    /// A project was archived.
    ProjectArchived {},
    /// A project setting changed.
    ProjectSettingChanged {
        /// The setting key.
        key: String,
        /// The new value.
        value: JsonValue,
        /// The project's new `settings_revision`.
        settings_revision: i64,
    },
    /// A global setting changed (never written for [`STORAGE_PROBE_KEY`]).
    SettingChanged {
        /// The setting key.
        key: String,
        /// The new value.
        value: JsonValue,
    },
    /// A task was created.
    TaskCreated {
        /// Short title.
        title: String,
        /// The ref it branches from.
        base_ref: String,
        /// The task it continues.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<TaskId>,
    },
    /// A task was archived.
    TaskArchived {},
    /// A workspace was created.
    WorkspaceCreated {
        /// The agent adapter id.
        agent_id: String,
        /// The git branch.
        branch: String,
        /// The worktree path.
        worktree_path: String,
        /// Its first state.
        state: WorkspaceState,
    },
    /// A workspace changed state.
    WorkspaceStateChanged {
        /// The old state.
        from: WorkspaceState,
        /// The new state.
        to: WorkspaceState,
    },
    /// A workspace's comparison outcome was set.
    WorkspaceOutcomeSet {
        /// The outcome.
        outcome: WorkspaceOutcome,
    },
    /// A run was created (always `queued`).
    RunCreated {
        /// What it is.
        kind: RunKind,
        /// Always `queued`.
        status: RunStatus,
        /// The run it resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumes_run_id: Option<RunId>,
    },
    /// A run changed status. The same fields as the `run.status` notification, without `event_id`.
    RunStatusChanged {
        /// The run.
        run_id: RunId,
        /// Its workspace.
        workspace_id: WorkspaceId,
        /// The old status.
        from: RunStatus,
        /// The new status.
        status: RunStatus,
        /// Why it ended, on a terminal status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_reason: Option<EndReason>,
        /// The exit code, when the process exited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// When it changed, Unix ms UTC.
        at: i64,
    },
    /// A run's log was trimmed by the log cap.
    RunLogTrimmed {
        /// Bytes removed.
        bytes: u64,
    },
    /// A message in the conversation (adapter reported, feature 4).
    Message {
        /// Who wrote it.
        role: MessageRole,
        /// The text.
        text: String,
    },
    /// A command the agent ran (adapter reported, feature 4).
    CommandRun {
        /// The argument array.
        argv: Vec<String>,
        /// Its exit code, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// A file the agent changed (adapter reported, feature 4).
    FileChanged {
        /// The file's path in the worktree.
        path: String,
        /// How it changed.
        change: FileChange,
        /// The old path, for a rename.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_path: Option<String>,
    },
    /// A run asked for your permission (spec 0003).
    ApprovalRequested {
        /// The approval.
        approval_id: ApprovalId,
        /// What it asks for.
        kind: ApprovalKind,
        /// One line for people.
        summary: String,
    },
    /// An approval left `pending` (spec 0003).
    ApprovalDecided {
        /// The approval.
        approval_id: ApprovalId,
        /// Its end state.
        status: ApprovalStatus,
        /// Who decided, for `approved` and `denied`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decided_by: Option<DecidedBy>,
        /// Where they decided (`inbox`, `terminal`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decided_by_ref: Option<String>,
        /// The decision's message, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl EventKind {
    /// Whether this event may change the log cap's outcome, so the engine runs
    /// the check (AC-10): a run ended, or a task or project was archived.
    pub const fn triggers_log_cap(&self) -> bool {
        match self {
            Self::RunStatusChanged { status, .. } => status.is_terminal(),
            Self::TaskArchived {} | Self::ProjectArchived {} => true,
            _ => false,
        }
    }
}

/// The journal kinds an adapter reports, the only ones `Store::append_event` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    /// A message in the conversation.
    Message {
        /// Who wrote it.
        role: MessageRole,
        /// The text.
        text: String,
    },
    /// A command the agent ran.
    CommandRun {
        /// The argument array.
        argv: Vec<String>,
        /// Its exit code, when known.
        exit_code: Option<i32>,
    },
    /// A file the agent changed.
    FileChanged {
        /// The file's path in the worktree.
        path: String,
        /// How it changed.
        change: FileChange,
        /// The old path, for a rename.
        from_path: Option<String>,
    },
}

impl From<AdapterEvent> for EventKind {
    fn from(event: AdapterEvent) -> Self {
        match event {
            AdapterEvent::Message { role, text } => Self::Message { role, text },
            AdapterEvent::CommandRun { argv, exit_code } => Self::CommandRun { argv, exit_code },
            AdapterEvent::FileChanged {
                path,
                change,
                from_path,
            } => Self::FileChanged {
                path,
                change,
                from_path,
            },
        }
    }
}

// ---- approvals (spec 0003) ----

text_enum!(
    /// What an approval asks for (the `ApprovalDetail` variant).
    ApprovalKind {
        /// Run a command.
        Command = "command",
        /// Edit a file.
        FileEdit = "file_edit",
        /// Fetch a URL.
        Fetch = "fetch",
        /// Call an MCP tool.
        McpTool = "mcp_tool",
        /// Anything else.
        Other = "other",
    }
);

text_enum!(
    /// An approval's state. Every state but `pending` is final.
    ApprovalStatus {
        /// Waiting for you.
        Pending = "pending",
        /// Allowed.
        Approved = "approved",
        /// Refused.
        Denied = "denied",
        /// The agent stopped waiting (its hook closed), or the engine restarted.
        Expired = "expired",
        /// Its run ended or was stopped first.
        Cancelled = "cancelled",
    }
);

text_enum!(
    /// Who decided an `approved` or `denied` approval.
    DecidedBy {
        /// You, in the inbox or the terminal.
        User = "user",
        /// An approval policy (feature 12).
        Policy = "policy",
        /// The agent itself.
        Agent = "agent",
    }
);

/// Where you decided: in switchyard's inbox (`approval.respond`).
pub const DECIDED_IN_INBOX: &str = "inbox";
/// Where you decided: in the agent's own terminal.
pub const DECIDED_IN_TERMINAL: &str = "terminal";

/// Most characters of an approval's summary that are kept.
pub const APPROVAL_SUMMARY_CHARS: usize = 200;
/// Most bytes of an approval's detail (its JSON text) before `raw` is cut.
pub const APPROVAL_DETAIL_BYTES: usize = 64 * 1024;
/// Most bytes of a decision's message.
pub const APPROVAL_MESSAGE_BYTES: usize = 4 * 1024;

/// A permission request from a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// Its id (`apr_`).
    pub id: ApprovalId,
    /// The run that asked.
    pub run_id: RunId,
    /// That run's workspace (joined from the run).
    pub workspace_id: WorkspaceId,
    /// What it asks for.
    pub kind: ApprovalKind,
    /// One line for people, at most [`APPROVAL_SUMMARY_CHARS`].
    pub summary: String,
    /// The kind's typed fields plus `raw` (and `raw_truncated` when cut).
    pub detail: serde_json::Value,
    /// Its state.
    pub status: ApprovalStatus,
    /// When it was asked, Unix ms UTC.
    pub requested_at: i64,
    /// When it left `pending`.
    pub decided_at: Option<i64>,
    /// Who decided (`approved` and `denied` only).
    pub decided_by: Option<DecidedBy>,
    /// Where they decided (`approved` and `denied` only).
    pub decided_by_ref: Option<String>,
    /// The decision's message (`response.message`).
    pub message: Option<String>,
}

/// An approval to create (`pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval {
    /// Its id.
    pub id: ApprovalId,
    /// The run that asks.
    pub run_id: RunId,
    /// What it asks for.
    pub kind: ApprovalKind,
    /// One line for people (cut to [`APPROVAL_SUMMARY_CHARS`] on create).
    pub summary: String,
    /// The kind's typed fields plus `raw`, already cut to fit.
    pub detail: serde_json::Value,
}

/// How an approval leaves `pending`. Built only by its constructors, so it
/// always names an end state with the who and where that state allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    status: ApprovalStatus,
    decided_by: Option<DecidedBy>,
    decided_by_ref: Option<&'static str>,
    message: Option<String>,
}

impl ApprovalDecision {
    /// You approved (or denied) it, `via` [`DECIDED_IN_INBOX`] or [`DECIDED_IN_TERMINAL`].
    pub fn by_user(approve: bool, via: &'static str, message: Option<String>) -> Self {
        Self {
            status: if approve {
                ApprovalStatus::Approved
            } else {
                ApprovalStatus::Denied
            },
            decided_by: Some(DecidedBy::User),
            decided_by_ref: Some(via),
            message,
        }
    }

    /// The agent stopped waiting, or the engine restarted.
    pub const fn expired() -> Self {
        Self {
            status: ApprovalStatus::Expired,
            decided_by: None,
            decided_by_ref: None,
            message: None,
        }
    }

    /// Its run ended or was stopped first, or an answer in the terminal
    /// couldn't be read.
    pub const fn cancelled(message: Option<String>) -> Self {
        Self {
            status: ApprovalStatus::Cancelled,
            decided_by: None,
            decided_by_ref: None,
            message,
        }
    }

    /// The end state.
    pub const fn status(&self) -> ApprovalStatus {
        self.status
    }

    /// Who decided.
    pub const fn decided_by(&self) -> Option<DecidedBy> {
        self.decided_by
    }

    /// Where they decided.
    pub const fn decided_by_ref(&self) -> Option<&'static str> {
        self.decided_by_ref
    }

    /// The message.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

/// The approval already left `pending`; end states never change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("approval {id} is already {status}")]
pub struct ApprovalNotPending {
    /// The approval.
    pub id: ApprovalId,
    /// Its end state.
    pub status: ApprovalStatus,
}

impl Approval {
    /// The approval after `decision` at `now`, or why not: an approval
    /// leaves `pending` exactly once (first change wins).
    pub fn decide(
        &self,
        decision: &ApprovalDecision,
        now: i64,
    ) -> Result<Self, ApprovalNotPending> {
        if self.status != ApprovalStatus::Pending {
            return Err(ApprovalNotPending {
                id: self.id.clone(),
                status: self.status,
            });
        }
        Ok(Self {
            status: decision.status,
            decided_at: Some(now),
            decided_by: decision.decided_by,
            decided_by_ref: decision.decided_by_ref.map(str::to_owned),
            message: decision.message.clone(),
            ..self.clone()
        })
    }

    /// The `approval_decided` event kind for this (decided) approval.
    pub fn decided_event(&self) -> EventKind {
        EventKind::ApprovalDecided {
            approval_id: self.id.clone(),
            status: self.status,
            decided_by: self.decided_by,
            decided_by_ref: self.decided_by_ref.clone(),
            message: self.message.clone(),
        }
    }
}

/// `summary` cut to [`APPROVAL_SUMMARY_CHARS`] characters.
pub fn cut_summary(summary: &str) -> String {
    summary.chars().take(APPROVAL_SUMMARY_CHARS).collect()
}

/// An event about to be journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// When it happened, Unix ms UTC.
    pub at: i64,
    /// Who caused it.
    pub actor: Actor,
    /// For `agent`: the acting workspace id (feature 33).
    pub actor_ref: Option<String>,
    /// The ids it is filed under.
    pub scope: EventScope,
    /// What happened.
    pub kind: EventKind,
}

/// A committed journal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The journal cursor: ascending, never reused.
    pub id: i64,
    /// When it happened, Unix ms UTC.
    pub at: i64,
    /// Who caused it.
    pub actor: Actor,
    /// For `agent`: the acting workspace id.
    pub actor_ref: Option<String>,
    /// The ids it is filed under.
    pub scope: EventScope,
    /// What happened.
    pub kind: EventKind,
}

impl Event {
    /// The committed form of `event`, with its journal id.
    pub fn committed(id: i64, event: NewEvent) -> Self {
        Self {
            id,
            at: event.at,
            actor: event.actor,
            actor_ref: event.actor_ref,
            scope: event.scope,
            kind: event.kind,
        }
    }
}

/// The status shown for a workspace or task. Declaration order is urgency, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisplayStatus {
    /// An agent is waiting for you.
    WaitingInput,
    /// A run failed.
    Failed,
    /// A run is working.
    Running,
    /// A run is starting.
    Starting,
    /// A run is queued.
    Queued,
    /// The workspace is being prepared.
    Preparing,
    /// A run was lost to an engine restart.
    Lost,
    /// The newest run finished.
    Finished,
    /// The newest run was stopped.
    Stopped,
    /// Ready, with no agent run yet.
    Idle,
    /// Merged.
    Merged,
    /// Discarded.
    Discarded,
}

impl From<RunStatus> for DisplayStatus {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Queued => Self::Queued,
            RunStatus::Starting => Self::Starting,
            RunStatus::Running => Self::Running,
            RunStatus::WaitingInput => Self::WaitingInput,
            RunStatus::Finished => Self::Finished,
            RunStatus::Failed => Self::Failed,
            RunStatus::Stopped => Self::Stopped,
            RunStatus::Lost => Self::Lost,
        }
    }
}

/// A workspace's shown status: its state unless `ready`, else the status of
/// its newest `agent` run (by `queued_at`, then id), else `idle`.
/// `runs` may hold any of the workspace's runs, in any order.
pub fn workspace_display_status(state: WorkspaceState, runs: &[Run]) -> DisplayStatus {
    match state {
        WorkspaceState::Preparing => DisplayStatus::Preparing,
        WorkspaceState::Merged => DisplayStatus::Merged,
        WorkspaceState::Discarded => DisplayStatus::Discarded,
        WorkspaceState::Ready => runs
            .iter()
            .filter(|r| r.kind == RunKind::Agent)
            .max_by(|a, b| (a.queued_at, &a.id).cmp(&(b.queued_at, &b.id)))
            .map_or(DisplayStatus::Idle, |r| r.status.into()),
    }
}

/// A task's shown status: the most urgent of its workspaces' statuses, or `idle` with none.
pub fn task_display_status(workspaces: impl IntoIterator<Item = DisplayStatus>) -> DisplayStatus {
    workspaces.into_iter().min().unwrap_or(DisplayStatus::Idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: RunStatus) -> Run {
        Run {
            id: RunId::new(),
            workspace_id: WorkspaceId::new(),
            kind: RunKind::Agent,
            input: None,
            resumes_run_id: None,
            agent_session_ref: None,
            status,
            exit_code: None,
            end_reason: None,
            pid: None,
            pgid: None,
            pid_started_at: None,
            queued_at: 1,
            started_at: None,
            ended_at: None,
            log_bytes: 0,
            log_state: LogState::Present,
        }
    }

    const END: RunEnd = RunEnd {
        reason: EndReason::Exited,
        exit_code: Some(0),
        log_bytes: 42,
    };

    fn change(to: RunStatus) -> RunTransition {
        if to.is_terminal() {
            RunTransition::end(to, END)
        } else {
            RunTransition::to(to)
        }
    }

    /// The transition table from spec 0002, written out pair by pair.
    const ALLOWED: &[(RunStatus, RunStatus)] = {
        use RunStatus::*;
        &[
            (Queued, Starting),
            (Queued, Stopped),
            (Starting, Running),
            (Starting, WaitingInput),
            (Starting, Finished),
            (Starting, Failed),
            (Starting, Stopped),
            (Running, WaitingInput),
            (Running, Finished),
            (Running, Failed),
            (Running, Stopped),
            (WaitingInput, Running),
            (WaitingInput, Finished),
            (WaitingInput, Failed),
            (WaitingInput, Stopped),
        ]
    };

    #[test]
    fn every_listed_transition_should_be_accepted() {
        let refused: Vec<_> = ALLOWED
            .iter()
            .filter(|(from, to)| !matches!(run(*from).transition(change(*to), 5), Ok(Some(_))))
            .collect();
        assert!(refused.is_empty(), "{refused:?}");
    }

    #[test]
    fn every_unlisted_transition_should_be_refused() {
        let mut accepted = Vec::new();
        for &from in RunStatus::ALL {
            for &to in RunStatus::ALL {
                if from == to || ALLOWED.contains(&(from, to)) {
                    continue;
                }
                if run(from).transition(change(to), 5) != Err(InvalidTransition { from, to }) {
                    accepted.push((from, to));
                }
            }
        }
        assert!(accepted.is_empty(), "{accepted:?}");
    }

    #[test]
    fn same_status_report_should_be_a_no_op() {
        let all_no_op = RunStatus::ALL
            .iter()
            .filter(|s| **s != RunStatus::Lost)
            .all(|&s| run(s).transition(change(s), 5) == Ok(None));
        assert!(all_no_op);
    }

    #[test]
    fn transition_should_refuse_lost_even_from_a_live_status() {
        let refused = [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
        ]
        .iter()
        .all(|&s| {
            run(s)
                .transition(RunTransition::end(RunStatus::Lost, END), 5)
                .is_err()
        });
        assert!(refused);
    }

    #[test]
    fn terminal_target_should_need_an_end_and_others_should_refuse_one() {
        let no_end = run(RunStatus::Running).transition(RunTransition::to(RunStatus::Finished), 5);
        let stray_end =
            run(RunStatus::Queued).transition(RunTransition::end(RunStatus::Starting, END), 5);
        assert!(no_end.is_err() && stray_end.is_err());
    }

    #[test]
    fn entering_starting_should_set_started_at() {
        let next = run(RunStatus::Queued)
            .transition(RunTransition::to(RunStatus::Starting), 77)
            .unwrap()
            .unwrap();
        assert_eq!(next.started_at, Some(77));
    }

    #[test]
    fn entering_a_terminal_status_should_set_its_end_fields() {
        let next = run(RunStatus::Running)
            .transition(RunTransition::end(RunStatus::Finished, END), 99)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                next.ended_at,
                next.end_reason,
                next.exit_code,
                next.log_bytes
            ),
            (Some(99), Some(EndReason::Exited), Some(0), 42)
        );
    }

    #[test]
    fn lose_should_end_a_live_run_as_lost_at_the_start_time() {
        for status in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
        ] {
            let lost = run(status).lose(500).unwrap();
            assert_eq!(
                (lost.status, lost.ended_at, lost.end_reason, lost.exit_code),
                (
                    RunStatus::Lost,
                    Some(500),
                    Some(EndReason::EngineRestart),
                    None
                ),
                "{status}"
            );
        }
    }

    #[test]
    fn lose_should_refuse_queued_and_terminal_runs() {
        let refused: Vec<RunStatus> = RunStatus::ALL
            .iter()
            .copied()
            .filter(|s| run(*s).lose(1).is_err())
            .collect();
        assert_eq!(
            refused,
            vec![
                RunStatus::Queued,
                RunStatus::Finished,
                RunStatus::Failed,
                RunStatus::Stopped,
                RunStatus::Lost
            ]
        );
    }

    #[test]
    fn workspace_lifecycle_should_match_the_spec() {
        use WorkspaceState::*;
        let allowed: Vec<(WorkspaceState, WorkspaceState)> = WorkspaceState::ALL
            .iter()
            .flat_map(|from| WorkspaceState::ALL.iter().map(move |to| (*from, *to)))
            .filter(|(from, to)| from.can_transition_to(*to))
            .collect();
        assert_eq!(
            allowed,
            vec![
                (Preparing, Ready),
                (Preparing, Discarded),
                (Ready, Merged),
                (Ready, Discarded)
            ]
        );
    }

    #[test]
    fn reconcile_should_take_the_file_length_over_the_stored_value() {
        let check = LogState::Present.reconcile(10, Some(64));
        assert_eq!(
            check,
            LogCheck {
                log_bytes: 64,
                log_state: LogState::Present,
                remove_file: false
            }
        );
    }

    #[test]
    fn reconcile_should_flag_a_vanished_file_deleted_only_when_bytes_were_written() {
        assert_eq!(
            (
                LogState::Present.reconcile(5, None).log_state,
                LogState::Incomplete.reconcile(5, None).log_state,
                LogState::Present.reconcile(0, None),
            ),
            (
                LogState::Deleted,
                LogState::Deleted,
                LogCheck {
                    log_bytes: 0,
                    log_state: LogState::Present,
                    remove_file: false
                },
            )
        );
    }

    #[test]
    fn reconcile_should_remove_a_trimmed_or_deleted_runs_file() {
        assert_eq!(
            (
                LogState::Trimmed.reconcile(9, Some(9)),
                LogState::Deleted.reconcile(0, None).remove_file,
            ),
            (
                LogCheck {
                    log_bytes: 0,
                    log_state: LogState::Trimmed,
                    remove_file: true
                },
                false,
            )
        );
    }

    #[test]
    fn text_enums_should_round_trip_their_text() {
        let ok = RunStatus::ALL
            .iter()
            .all(|s| RunStatus::parse(s.as_str()) == Some(*s))
            && EndReason::ALL
                .iter()
                .all(|s| EndReason::parse(s.as_str()) == Some(*s))
            && WorkspaceState::ALL
                .iter()
                .all(|s| WorkspaceState::parse(s.as_str()) == Some(*s));
        assert!(ok);
    }

    #[test]
    fn ready_workspace_should_show_its_newest_agent_run() {
        let mut old = run(RunStatus::Failed);
        old.queued_at = 1;
        let mut new = run(RunStatus::Running);
        new.queued_at = 2;
        let mut script = run(RunStatus::WaitingInput);
        script.kind = RunKind::Setup;
        script.queued_at = 3;
        assert_eq!(
            workspace_display_status(WorkspaceState::Ready, &[new, script, old]),
            DisplayStatus::Running
        );
    }

    #[test]
    fn ready_workspace_with_no_agent_run_should_be_idle() {
        assert_eq!(
            workspace_display_status(WorkspaceState::Ready, &[]),
            DisplayStatus::Idle
        );
    }

    #[test]
    fn workspace_not_ready_should_show_its_state() {
        let runs = [run(RunStatus::Running)];
        let shown = [
            WorkspaceState::Preparing,
            WorkspaceState::Merged,
            WorkspaceState::Discarded,
        ]
        .map(|s| workspace_display_status(s, &runs));
        assert_eq!(
            shown,
            [
                DisplayStatus::Preparing,
                DisplayStatus::Merged,
                DisplayStatus::Discarded
            ]
        );
    }

    #[test]
    fn task_should_show_its_most_urgent_workspace() {
        let shown = task_display_status([
            DisplayStatus::Finished,
            DisplayStatus::WaitingInput,
            DisplayStatus::Running,
        ]);
        assert_eq!(shown, DisplayStatus::WaitingInput);
    }

    #[test]
    fn event_kind_should_serialise_as_kind_plus_payload() {
        let kind = EventKind::RunCreated {
            kind: RunKind::Agent,
            status: RunStatus::Queued,
            resumes_run_id: None,
        };
        assert_eq!(
            serde_json::to_value(&kind).unwrap(),
            serde_json::json!({ "kind": "run_created", "payload": { "kind": "agent", "status": "queued" } })
        );
    }

    #[test]
    fn empty_payload_kinds_should_serialise_an_empty_object() {
        let value = serde_json::to_value(EventKind::TaskArchived {}).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "kind": "task_archived", "payload": {} })
        );
    }

    fn workspace(state: WorkspaceState) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            task_id: TaskId::new(),
            project_id: ProjectId::new(),
            agent_id: "synthetic".into(),
            agent_options: "{}".into(),
            branch: "b".into(),
            worktree_path: "/w".into(),
            base_commit: String::new(),
            state,
            outcome: None,
            merged_commit: None,
            created_at: 1,
            closed_at: None,
        }
    }

    fn change_for(to: WorkspaceState) -> Option<WorkspaceChange> {
        match to {
            WorkspaceState::Preparing => None,
            WorkspaceState::Ready => Some(WorkspaceChange::Ready),
            WorkspaceState::Merged => Some(WorkspaceChange::Merged {
                commit: "abc".into(),
            }),
            WorkspaceState::Discarded => Some(WorkspaceChange::Discarded),
        }
    }

    #[test]
    fn workspace_lifecycle_should_accept_only_the_listed_moves() {
        for &from in WorkspaceState::ALL {
            for &to in WorkspaceState::ALL {
                let Some(change) = change_for(to) else {
                    continue;
                };
                let result = workspace(from).change_state(change, 9);
                match result {
                    Ok(None) => assert_eq!(from, to),
                    Ok(Some(next)) => {
                        assert!(from.can_transition_to(to), "{from} → {to}");
                        assert_eq!(next.state, to);
                        assert_eq!(next.closed_at, to.is_closed().then_some(9));
                        let merged = (to == WorkspaceState::Merged).then(|| "abc".to_owned());
                        assert_eq!(next.merged_commit, merged);
                    }
                    Err(err) => {
                        assert!(!from.can_transition_to(to) && from != to);
                        assert_eq!(err, InvalidWorkspaceTransition { from, to });
                    }
                }
            }
        }
    }

    #[test]
    fn outcome_should_need_a_comparison_and_repeat_as_a_no_op() {
        let ws = workspace(WorkspaceState::Ready);
        assert_eq!(
            ws.with_outcome(WorkspaceOutcome::Winner, 1),
            Err(NotAComparison)
        );
        let won = ws
            .with_outcome(WorkspaceOutcome::Winner, 2)
            .unwrap()
            .unwrap();
        assert_eq!(won.outcome, Some(WorkspaceOutcome::Winner));
        assert_eq!(won.with_outcome(WorkspaceOutcome::Winner, 2), Ok(None));
        let changed = won.with_outcome(WorkspaceOutcome::NotChosen, 3).unwrap();
        assert_eq!(changed.unwrap().outcome, Some(WorkspaceOutcome::NotChosen));
    }

    #[test]
    fn json_value_should_round_trip_every_json_shape() {
        let text = r#"{"a":null,"b":true,"c":-3,"d":18446744073709551615,"e":1.5,"f":"x","g":[1,{"h":[]}]}"#;
        let value: JsonValue = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), text);
        assert_eq!(
            serde_json::from_str::<JsonValue>("5368709120")
                .unwrap()
                .as_u64(),
            Some(5_368_709_120)
        );
        assert_eq!(JsonValue::Int(-1).as_u64(), None);
        assert_eq!(JsonValue::String("1".into()).as_u64(), None);
    }

    #[test]
    fn setting_events_should_carry_the_value_as_json_not_text() {
        let value = serde_json::to_value(EventKind::SettingChanged {
            key: "storage.log_cap_bytes".into(),
            value: JsonValue::Int(10),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "setting_changed",
                "payload": { "key": "storage.log_cap_bytes", "value": 10 }
            })
        );
    }

    #[test]
    fn log_cap_should_be_triggered_by_run_ends_and_archives_only() {
        let status = |status| EventKind::RunStatusChanged {
            run_id: RunId::new(),
            workspace_id: WorkspaceId::new(),
            from: RunStatus::Running,
            status,
            end_reason: None,
            exit_code: None,
            at: 1,
        };
        for &s in RunStatus::ALL {
            assert_eq!(status(s).triggers_log_cap(), s.is_terminal(), "{s}");
        }
        assert!(EventKind::TaskArchived {}.triggers_log_cap());
        assert!(EventKind::ProjectArchived {}.triggers_log_cap());
        assert!(!EventKind::RunLogTrimmed { bytes: 1 }.triggers_log_cap());
    }

    #[test]
    fn urgency_order_should_match_the_spec() {
        use DisplayStatus::*;
        let spec = [
            WaitingInput,
            Failed,
            Running,
            Starting,
            Queued,
            Preparing,
            Lost,
            Finished,
            Stopped,
            Idle,
            Merged,
            Discarded,
        ];
        assert!(spec.windows(2).all(|w| w[0] < w[1]));
    }
}
