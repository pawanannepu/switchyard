//! Params, results and notification payloads.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Protocol version. Compatible when the majors match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolVersion {
    /// Breaking changes bump this.
    pub major: u32,
    /// Additions only.
    pub minor: u32,
}

impl ProtocolVersion {
    /// True when both sides speak the same major.
    pub const fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }
}

/// Which kind of client is connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    /// The desktop app host (or the dev bridge acting for the UI).
    Desktop,
    /// A command line client.
    Cli,
    /// An automated test.
    Test,
    /// `sy-signal`, relaying one agent hook call. It may call only the stable
    /// methods and `signal.hook`, and it gets no notifications.
    Signal,
}

/// Params with no fields. Accepts `{}` or an absent `params`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

// ---- session ----

/// `session.hello` params. Always the first call on a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloParams {
    /// Which kind of client this is.
    pub client: ClientKind,
    /// The client's own version, for logs.
    pub client_version: String,
    /// The protocol version the client speaks.
    pub protocol: ProtocolVersion,
}

/// `session.hello` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    /// The engine's build version.
    pub engine_version: String,
    /// The engine's process id.
    pub pid: u32,
    /// The protocol version the engine speaks.
    pub protocol: ProtocolVersion,
}

// ---- engine ----

/// `engine.info` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EngineInfo {
    /// The engine's build version.
    pub engine_version: String,
    /// The engine's process id.
    pub pid: u32,
    /// The protocol version the engine speaks.
    pub protocol: ProtocolVersion,
    /// Agents (runs) currently running in this engine.
    pub running_agents: u32,
    /// Local storage health. Optional, so the frozen stable shape still holds
    /// for clients that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageInfo>,
    /// How the login environment (what agents run with) was found. Absent
    /// until the first capture finishes. Optional, like `storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_env: Option<LoginEnvInfo>,
}

/// Whether the login shell gave the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginEnvStatus {
    /// Your login shell answered.
    Ok,
    /// It didn't; agents run with `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`.
    Fallback,
}

/// The login environment's state, for `engine.info` and `env.refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginEnvInfo {
    /// Whether the login shell answered.
    pub status: LoginEnvStatus,
    /// Why it fell back, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `env.refresh` result. Detection reruns in the background after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnvRefreshResult {
    /// The new capture's state. Only later launches and probes use it.
    pub login_env: LoginEnvInfo,
}

// ---- storage ----

/// Where local storage stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageStatus {
    /// Open and saving normally.
    Ok,
    /// A write failed (disk full or an I/O error): output still streams live,
    /// changes wait in memory, and notifications carry `event_id: null`.
    DegradedWrites,
    /// The database was written by a newer engine, so this one won't touch it.
    NewerSchema,
    /// The database file is damaged.
    Corrupt,
    /// The database could not be opened, or a migration failed.
    Unavailable,
}

/// Local storage health, in `engine.info` and the `storage.status` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StorageInfo {
    /// Where storage stands.
    pub status: StorageStatus,
    /// A human readable explanation (empty when `ok`).
    pub message: String,
    /// The database's schema version (`PRAGMA user_version`); `null` when the file couldn't be opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    /// The newest backup's file name in `backups/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_backup: Option<String>,
}

/// How `storage.recover` gets back to a working database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoverAction {
    /// Put the newest backup that passes its integrity check in place, then migrate it.
    RestoreBackup,
    /// Start over with an empty database.
    StartFresh,
}

/// `storage.recover` params. Allowed only while storage is `newer_schema`,
/// `corrupt` or `unavailable`. The bad database, its `-wal` and `-shm` files
/// and `runs/` are renamed to `.bad-<unix ms>` first, never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StorageRecoverParams {
    /// What to do.
    pub action: RecoverAction,
    /// Must be `true`: this sets your current database aside.
    pub confirm: bool,
}

/// `storage.recover` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StorageRecoverResult {
    /// Storage health after the recovery.
    pub storage: StorageInfo,
}

// ---- journal ----

/// Identifies a project (one git repository).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Who caused an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// You, through a client.
    User,
    /// The engine itself (startup recovery, trimming).
    Engine,
    /// An approval policy.
    Policy,
    /// An agent acting through the engine (see `actor_ref`).
    Agent,
}

/// What a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// An agent launch, follow up or resume.
    Agent,
    /// A project setup script.
    Setup,
    /// A project run script.
    RunScript,
}

/// A workspace's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// The worktree is being created or set up.
    Preparing,
    /// Ready for runs.
    Ready,
    /// Merged into the base branch.
    Merged,
    /// Thrown away.
    Discarded,
}

/// A workspace's result in a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOutcome {
    /// The chosen workspace.
    Winner,
    /// Compared and not chosen.
    NotChosen,
}

/// Who wrote a `message` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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

/// An event's kind and its payload. Payload fields are only ever added as
/// optional; a breaking shape becomes a new kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventPayload {
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
        /// The new value (any JSON).
        #[schemars(extend("tsType" = "unknown"))]
        value: serde_json::Value,
        /// The project's new settings revision.
        settings_revision: i64,
    },
    /// A global setting changed.
    SettingChanged {
        /// The setting key.
        key: String,
        /// The new value (any JSON).
        #[schemars(extend("tsType" = "unknown"))]
        value: serde_json::Value,
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
    /// A run changed status: the `run.status` notification's fields, without `event_id`.
    RunStatusChanged {
        /// The run.
        run_id: RunId,
        /// Its workspace.
        workspace_id: WorkspaceId,
        /// The old status.
        from: RunStatus,
        /// The new status.
        status: RunStatus,
        /// Why it ended, on a terminal status (`exited`, `user_stop`, `engine_restart`, `spawn_failed`, `limit_exceeded`, `supervisor_error`, ...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_reason: Option<String>,
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
    /// A message in the conversation, reported by the agent's adapter.
    Message {
        /// Who wrote it.
        role: MessageRole,
        /// The text.
        text: String,
    },
    /// A command the agent ran, reported by its adapter.
    CommandRun {
        /// The argument array.
        argv: Vec<String>,
        /// Its exit code, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// A file the agent changed, reported by its adapter.
    FileChanged {
        /// The file's path in the worktree.
        path: String,
        /// How it changed.
        change: FileChange,
        /// The old path, for a rename.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_path: Option<String>,
    },
    /// A run asked for your permission.
    ApprovalRequested {
        /// The approval.
        approval_id: ApprovalId,
        /// What it asks for.
        kind: ApprovalKind,
        /// One line for people.
        summary: String,
    },
    /// An approval left `pending`.
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
    /// A run reached a limit and is being stopped; it ends `failed` with `limit_exceeded`.
    RunLimitExceeded {
        /// The run.
        run_id: RunId,
        /// Which limit.
        limit: RunLimit,
        /// The limit's value (ms for `idle` and `time`, bytes for `output`).
        threshold: u64,
        /// What was observed when it was hit.
        observed: u64,
    },
}

/// A run limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunLimit {
    /// No output and no hook call for too long while `starting` or `running`.
    Idle,
    /// Too much active time (`waiting_input` does not count).
    Time,
    /// Too many terminal output bytes.
    Output,
}

/// One journal event, as `event.since` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    /// The journal cursor: ascending, never reused.
    pub id: i64,
    /// When it happened, Unix ms UTC.
    pub at: i64,
    /// Who caused it.
    pub actor: Actor,
    /// For `agent`: the acting workspace id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_ref: Option<String>,
    /// The project it is filed under; absent only for global events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// The task, when the event is about a task or something in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The workspace, when the event is about a workspace or a run in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The run, when the event is about a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// What happened: `kind` plus its `payload`.
    #[serde(flatten)]
    pub payload: EventPayload,
}

/// `event.since` params: journal events after a cursor. Set filters combine with AND.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSinceParams {
    /// Return events with `id` above this; 0 for the start. Negative is invalid.
    pub cursor: i64,
    /// Page size: absent or 0 means 200, and it is capped at 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only this project's events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Only this task's events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Only this workspace's events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Only this run's events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

/// `event.since` result. Advance your cursor only from here, never from a
/// notification's `event_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSinceResult {
    /// Matching events, ascending by `id`.
    pub events: Vec<Event>,
    /// The id of the last event returned, or the input cursor when none.
    pub next_cursor: i64,
    /// True when more events match after this page.
    pub has_more: bool,
}

// ---- run ----

/// Identifies one run (one agent process and its terminal).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct RunId(pub String);

/// Identifies a task (the intent: a prompt worked on in one or more workspaces).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct TaskId(pub String);

/// Identifies a workspace (one worktree, branch and agent working on a task).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

/// A run's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Waiting for a slot.
    Queued,
    /// The process is being launched.
    Starting,
    /// The process is working.
    Running,
    /// The agent asked for input.
    WaitingInput,
    /// The process exited on its own.
    Finished,
    /// The process failed, or could not start.
    Failed,
    /// Stopped on request.
    Stopped,
    /// The engine restarted while it was live, so its real end is unknown.
    Lost,
}

/// `run.status` notification, sent to every connection after a compatible
/// `session.hello`, attached or not, in `event_id` order. Sent only after the
/// change and its journal event committed, and after the run's last output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunStatusChanged {
    /// The run.
    pub run_id: RunId,
    /// Its workspace.
    pub workspace_id: WorkspaceId,
    /// The status before the change.
    pub from: RunStatus,
    /// The new status.
    pub status: RunStatus,
    /// Why it ended, on a terminal status (`exited`, `user_stop`, `engine_restart`, `spawn_failed`, `limit_exceeded`, `supervisor_error`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// The exit code, when the process exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// When it changed, Unix ms UTC.
    pub at: i64,
    /// The journal event id. `null` while storage is degraded. Advance a cursor
    /// only from `event.since` results, never from this.
    pub event_id: Option<i64>,
}

/// `run.attach` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunAttachParams {
    /// The run to attach to.
    pub run_id: RunId,
}

/// `run.attach` result: the replay of the run's ring buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunAttachResult {
    /// The latest output, up to 2 MiB, base64.
    pub replay_b64: String,
    /// Total bytes the run has produced at the end of the replay. Drop any live byte below this.
    pub end_offset: u64,
}

/// `run.output` notification: one batch of live terminal bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunOutput {
    /// The run that produced it.
    pub run_id: RunId,
    /// Byte position of this chunk's first byte in the run's whole output.
    pub offset: u64,
    /// The bytes, base64.
    pub data_b64: String,
}

/// `run.input` params. The decoded payload is capped at 64 KiB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunInputParams {
    /// The run to type into.
    pub run_id: RunId,
    /// The bytes to write to its terminal, base64.
    pub data_b64: String,
}

/// `run.resize` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResizeParams {
    /// The run to resize.
    pub run_id: RunId,
    /// New width, in character cells.
    pub cols: u16,
    /// New height, in character cells.
    pub rows: u16,
}

/// `run.log_read` params: a page of a run's saved output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunLogReadParams {
    /// The run to read.
    pub run_id: RunId,
    /// Byte position to read from (the same offsets as `run.output`).
    pub offset: u64,
    /// Most bytes to return: absent or 0 means 262144, and it is capped at 262144.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
}

/// `run.log_read` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunLogReadResult {
    /// Where `data_b64` starts (the requested offset).
    pub offset: u64,
    /// The raw bytes, base64.
    pub data_b64: String,
    /// True when this read reaches `log_bytes`. For a live run that only means
    /// "the end of the output so far".
    pub eof: bool,
    /// Bytes in the log file now (for a live run, without its unsaved batch).
    pub log_bytes: u64,
}

/// `run.stop` params. Returns `{}` once the stop has started; the run's end
/// arrives as `run.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunStopParams {
    /// The run to stop.
    pub run_id: RunId,
}

/// `run.resync_needed` notification: this client fell behind; attach again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResyncNeeded {
    /// The run whose output was dropped for this client.
    pub run_id: RunId,
}

// ---- agent (spec 0003) ----

/// Where an adapter's live status comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    /// The agent's own hooks.
    Hooks,
    /// Patterns in its terminal output.
    Patterns,
    /// Only whether it exited.
    ExitOnly,
}

/// What an adapter can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCapabilities {
    /// It can launch runs (false for a detection only adapter).
    pub launch: bool,
    /// It relays permission requests.
    pub approvals: bool,
    /// It can resume a session.
    pub resume: bool,
    /// It reports journal events.
    pub events: bool,
    /// Where its live status comes from.
    pub status_source: StatusSource,
}

/// Whether an agent's CLI is signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignedIn {
    /// Yes.
    Yes,
    /// No.
    No,
    /// The CLI can't say without switchyard reading its files, which it never does.
    Unknown,
}

/// An agent's last detection: asked of its CLI, never read from its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentDetection {
    /// Its CLI is on the login `PATH` (or at its absolute path).
    pub installed: bool,
    /// Whether it is signed in.
    pub signed_in: SignedIn,
    /// Its version, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Where its CLI was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A hint for people, when something looks off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// When the detection pass ended, Unix ms UTC.
    pub checked_at: i64,
}

/// A registered agent adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentInfo {
    /// Its id (`claude`, ...).
    pub id: String,
    /// Its name for people.
    pub display_name: String,
    /// What it can do.
    pub capabilities: AgentCapabilities,
    /// Its last detection; absent until the first pass ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<AgentDetection>,
}

/// `agent.list` and `agent.detect` result, in registration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentListResult {
    /// The adapters.
    pub agents: Vec<AgentInfo>,
}

/// `agent.detect` params. Runs a detection pass (or joins the one running)
/// and answers when it ends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDetectParams {
    /// Answer with only this agent; every agent when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

// ---- approval (spec 0003) ----

/// Identifies an approval (`apr_` plus 32 hex digits).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ApprovalId(pub String);

/// What an approval asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Run a command.
    Command,
    /// Edit a file.
    FileEdit,
    /// Fetch a URL.
    Fetch,
    /// Call an MCP tool.
    McpTool,
    /// Anything else.
    Other,
}

/// An approval's state. Every state but `pending` is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for you.
    Pending,
    /// Allowed.
    Approved,
    /// Refused.
    Denied,
    /// The agent stopped waiting (its own hook timed out), or the engine restarted.
    Expired,
    /// Its run ended or was stopped first.
    Cancelled,
}

/// Who decided an `approved` or `denied` approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    /// You, in the inbox or the terminal.
    User,
    /// An approval policy.
    Policy,
    /// The agent itself.
    Agent,
}

/// A permission request from a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Approval {
    /// Its id.
    pub id: ApprovalId,
    /// The run that asked.
    pub run_id: RunId,
    /// That run's workspace.
    pub workspace_id: WorkspaceId,
    /// What it asks for.
    pub kind: ApprovalKind,
    /// One line for people, at most 200 characters.
    pub summary: String,
    /// The kind's fields (`command` and `cwd`, `path` and `diff`, `url`,
    /// `server`, `tool` and `input`), plus the agent's `raw` payload and
    /// `raw_truncated: true` when it was cut to fit 64 KiB. May hold secrets
    /// the agent was about to use; mask it on display.
    pub detail: serde_json::Value,
    /// Its state.
    pub status: ApprovalStatus,
    /// When it was asked, Unix ms UTC.
    pub requested_at: i64,
    /// When it left `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<i64>,
    /// Who decided (`approved` and `denied` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<DecidedBy>,
    /// Where they decided: `inbox` (`approval.respond`) or `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by_ref: Option<String>,
    /// The decision's message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A position in `approval.list`: the last approval a page returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCursor {
    /// Its `requested_at`.
    pub requested_at: i64,
    /// Its id.
    pub id: ApprovalId,
}

/// `approval.list` params. Filters combine with `after` (AND).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListParams {
    /// Only approvals in this state. Default `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ApprovalStatus>,
    /// Only this run's approvals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// Start after this approval (a page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<ApprovalCursor>,
    /// Page size: absent or 0 means 100, capped at 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `approval.list` result, ordered by (`requested_at`, `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalListResult {
    /// The page.
    pub approvals: Vec<Approval>,
    /// The last approval of this page, for `after`; absent on an empty page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ApprovalCursor>,
    /// True when more approvals match after this page.
    pub has_more: bool,
}

/// Your answer to an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalVerdict {
    /// Allow it.
    Approve,
    /// Refuse it.
    Deny,
}

/// `approval.respond` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondParams {
    /// The approval to answer (it must be `pending`).
    pub approval_id: ApprovalId,
    /// Allow or refuse.
    pub decision: ApprovalVerdict,
    /// A message for the agent, at most 4096 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `approval.respond` result: the decided approval, even if the agent could
/// no longer take the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRespondResult {
    /// The approval, now `approved` or `denied`.
    pub approval: Approval,
}

/// `approval.requested` and `approval.decided` notifications (to every
/// connection after hello, except `signal` ones).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalNotification {
    /// The approval as it is now.
    pub approval: Approval,
    /// Its journal event id; `null` only while storage is degraded.
    pub event_id: Option<i64>,
}

// ---- signal (from `sy-signal`, the agent hook helper) ----

/// `signal.hook` params: one agent hook call, relayed by `sy-signal` on a
/// `signal` connection. The reply may wait until an approval is decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalHookParams {
    /// The run's signal token (from the helper's own arguments).
    pub token: String,
    /// The hook's name, as the adapter configured it.
    pub hook: String,
    /// The arguments the agent passed to the hook.
    #[serde(default)]
    pub args: Vec<String>,
    /// What the agent wrote to the hook's stdin, base64. With `args`, at most
    /// 262144 bytes once decoded.
    #[serde(default)]
    pub stdin_b64: String,
}

/// `signal.hook` result: what `sy-signal` writes to stdout and exits with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SignalHookResult {
    /// The helper's stdout bytes, base64.
    pub stdout_b64: String,
    /// The helper's exit code.
    pub exit_code: i32,
}

// ---- dev (only in engines built with the `devtools` feature) ----

/// `dev.agent_run` params: fixture rows plus runs of an agent, usually the
/// fake agent (`agent_id: "fake"` with `options: { script: [...] }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunParams {
    /// The agent adapter to launch.
    pub agent_id: String,
    /// The prompt. Default `""`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The workspace's `agent_options`. Default `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Value>,
    /// How many runs to start, one workspace each: 1 to 32, default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Terminal width. Default 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// Terminal height. Default 40.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Run limits for these runs, above every setting. `0` turns one off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AgentRunLimits>,
}

/// `dev.agent_run` limits: each an integer from 0 to 2^53, `0` meaning off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunLimits {
    /// The idle limit, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// The active time limit, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_ms: Option<u64>,
    /// The output limit, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
}

/// `dev.agent_run` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunResult {
    /// The task this call created in the dev fixture project.
    pub task_id: TaskId,
    /// One workspace per run, in the same order as `run_ids`.
    pub workspace_ids: Vec<WorkspaceId>,
    /// The new runs, ready for `run.attach`. A run that could not launch is
    /// already `failed`, with its reason as the first line of its log.
    pub run_ids: Vec<RunId>,
}

// ---- host (app host or dev bridge to UI; never sent by the engine) ----

/// State of the host's engine connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostConnectionState {
    /// Looking for the engine, or starting it.
    Connecting,
    /// Connected and `session.hello` succeeded.
    Connected,
    /// The engine connection dropped (the engine exited or crashed).
    Stopped,
    /// The engine did not start within the timeout.
    Failed,
    /// The engine answered, but speaks an incompatible protocol major.
    Incompatible,
}

/// `host.connection` notification, sent by the app host (or dev bridge) to the UI
/// whenever the engine connection changes, and once on subscribe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostConnection {
    /// Where the connection stands.
    pub state: HostConnectionState,
    /// The engine's `session.hello` reply, when connected (or incompatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<HelloResult>,
    /// A human readable reason, for the failed and stopped states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Where the engine logs live, so the UI can point at them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_dir: Option<String>,
}
