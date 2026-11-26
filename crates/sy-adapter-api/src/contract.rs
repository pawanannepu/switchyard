//! The contract types and traits (spec 0003, *Contract*).

use serde::{Deserialize, Serialize};

/// An agent adapter: what an agent is, how to detect it, and how to launch it.
///
/// Every method is synchronous and does no I/O. The engine calls them and
/// does the work they describe.
pub trait AgentAdapter: Send + Sync + 'static {
    /// Stable machine id, for example `claude`, `fake`, or `gen_<uuidv7 hex>`
    /// for a configured generic adapter. Unique within one engine.
    fn id(&self) -> &str;

    /// Name shown to people, for example `Claude Code`.
    fn display_name(&self) -> &str;

    /// What this adapter can do.
    fn capabilities(&self) -> Capabilities;

    /// The only programs a probe or launch of this adapter may run. A bare
    /// name (`claude`) matches a plan `program` of the same bare name, which
    /// the engine resolves on the login `PATH`. An absolute path matches only
    /// that exact path, compared literally and never resolved.
    fn programs(&self) -> Vec<String>;

    /// The commands detection runs, in order. Each `program` must be in
    /// [`AgentAdapter::programs`].
    fn detection_probes(&self) -> Vec<Probe>;

    /// Turn the probe results (in probe order) into a detection result.
    fn interpret_detection(&self, results: &[ProbeResult]) -> Detection;

    /// Plan one run: the program, its arguments and files, plus the decoder
    /// that will read this run.
    fn launch(&self, req: &LaunchRequest) -> Result<Launch, AdapterError>;
}

/// What an adapter supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// It can launch runs (false for a detection only adapter).
    pub launch: bool,
    /// It relays permission requests as approvals.
    pub approvals: bool,
    /// It can resume an earlier session.
    pub resume: bool,
    /// It reports journal events (messages, commands, file changes).
    pub events: bool,
    /// Where its live status comes from.
    pub status_source: StatusSource,
}

/// Where an adapter's live status comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSource {
    /// The agent's own hooks, through `sy-signal`.
    Hooks,
    /// Output patterns and a quiet timer ([`crate::OutputScanner`]).
    Patterns,
    /// Only the exit: `running` until it ends.
    ExitOnly,
}

/// One detection command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The program, which must be in [`AgentAdapter::programs`].
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// How long it may run, in ms. The engine caps it at 5000.
    pub timeout_ms: u32,
}

/// What one probe did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    /// The probe that ran.
    pub probe: Probe,
    /// How it ended.
    pub outcome: ProbeOutcome,
    /// What it wrote to stdout, capped at 64 KiB.
    pub stdout: Vec<u8>,
    /// What it wrote to stderr, capped at 64 KiB.
    pub stderr: Vec<u8>,
}

/// How a probe ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// It exited (on its own or by a signal).
    Exited(ExitInfo),
    /// It ran past its timeout and was killed.
    TimedOut,
    /// The program isn't on the login `PATH`; nothing was spawned.
    NotFound,
    /// The program isn't in [`AgentAdapter::programs`]; nothing was spawned.
    Refused,
    /// The spawn itself failed.
    SpawnError(String),
}

/// What detection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The agent's CLI is installed.
    pub installed: bool,
    /// Whether it is signed in, as far as the CLI says without reading tokens.
    pub signed_in: SignedIn,
    /// Its version, when it reported one.
    pub version: Option<String>,
    /// A short hint for you, when something looks wrong.
    pub hint: Option<String>,
}

/// Whether an agent is signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedIn {
    /// Signed in.
    Yes,
    /// Not signed in.
    No,
    /// The CLI gives no way to tell without reading its files.
    Unknown,
}

/// Everything an adapter needs to plan one run.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchRequest {
    /// The run's id (`run_…`).
    pub run_id: String,
    /// The task prompt (or follow up input).
    pub prompt: String,
    /// The working directory: the workspace's worktree.
    pub cwd: String,
    /// `workspace.agent_options`, parsed.
    pub options: serde_json::Value,
    /// An `agent_session_ref` to resume (always `None` until feature 23).
    pub resume: Option<String>,
    /// `<data dir>/agent-runtime/<run_id>`, where [`LaunchPlan::files`] land.
    pub launch_dir: String,
    /// How the agent's hooks reach the engine.
    pub signal: SignalEndpoint,
    /// The terminal size at launch.
    pub size: PtySize,
}

/// How a hook reaches the engine: run `helper` with this socket and token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalEndpoint {
    /// The absolute path of the `sy-signal` helper.
    pub helper: String,
    /// The engine socket's path.
    pub socket: String,
    /// This run's signal token (attribution, held only in memory).
    pub token: String,
}

/// A terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// Width, in character cells.
    pub cols: u16,
    /// Height, in character cells.
    pub rows: u16,
}

/// A planned run: what to spawn, and the decoder that reads it.
pub struct Launch {
    /// What the engine spawns.
    pub plan: LaunchPlan,
    /// Reads this run. Lives on the run's own task.
    pub decoder: Box<dyn RunDecoder>,
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("plan", &self.plan)
            .finish_non_exhaustive()
    }
}

/// What the engine spawns for a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The program, which must be in [`AgentAdapter::programs`].
    pub program: String,
    /// Its arguments (never a shell string).
    pub args: Vec<String>,
    /// Variables to set on top of the login environment.
    pub env_set: Vec<(String, String)>,
    /// Variables to remove from the login environment.
    pub env_remove: Vec<String>,
    /// Files written under [`LaunchRequest::launch_dir`] before the spawn, `0600`.
    pub files: Vec<LaunchFile>,
    /// Bytes typed into the terminal once, when the run is first `running`.
    pub initial_input: Option<Vec<u8>>,
}

/// A file for the launch dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFile {
    /// Its path inside the launch dir: relative, with no `..` component.
    pub rel_path: String,
    /// Its bytes.
    pub contents: Vec<u8>,
}

/// Reads one run. The engine calls every method on the run's own task, in
/// order and never in parallel, so a decoder needs no locking.
///
/// **Contract rules**, binding on every adapter:
/// - Calls should be cheap: parse, match, return.
/// - [`RunDecoder::on_hook`] must always answer. An unknown hook name, or a
///   payload it can't parse, gets the same fallback reply that hook's
///   [`crate::hook_command`] was given, with no reports.
/// - An [`ApprovalRequest::key`] is the adapter's own id for the request (for
///   example the agent's tool call id), unique among the run's pending
///   approvals. [`RunDecoder::hold_reply`], [`RunDecoder::input_reply`] and
///   [`Report::ApprovalSettled`] name the request by that key.
/// - The fallback for any approval hook is the agent's own prompt, never an allow.
pub trait RunDecoder: Send {
    /// A copy of a chunk of terminal output. Under overload some chunks are
    /// dropped for the decoder (never for the log), so output based status
    /// is best effort.
    fn on_output(&mut self, chunk: &[u8], now_ms: i64) -> Vec<Report>;

    /// A hook call from the agent, through `sy-signal`. Must always answer.
    fn on_hook(&mut self, call: &HookCall, now_ms: i64) -> HookOutcome;

    /// Called every 250 ms while the run is live, for quiet period timers.
    fn on_tick(&mut self, now_ms: i64) -> Vec<Report>;

    /// The reply for a held hook (`HookResponse::Hold`) once it is decided.
    fn hold_reply(&mut self, key: &str, decision: &Decision) -> HookReply;

    /// The bytes to type for a decision on a request reported with
    /// [`Report::ApprovalRequested`] (answered in the terminal).
    fn input_reply(&mut self, key: &str, decision: &Decision) -> Vec<u8>;

    /// How to stop this run gracefully, before the engine's signals.
    fn stop(&mut self) -> StopPlan;

    /// Classify the exit.
    fn on_exit(&mut self, exit: &ExitInfo) -> ExitOutcome;
}

/// One hook call, as `sy-signal` relayed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCall {
    /// The hook name given to [`crate::hook_command`].
    pub hook: String,
    /// The arguments the agent passed after `--`.
    pub args: Vec<String>,
    /// What the agent wrote to the helper's stdin (capped at 256 KiB).
    pub stdin: Vec<u8>,
}

/// A decoder's answer to a hook call.
#[derive(Debug, Clone, PartialEq)]
pub struct HookOutcome {
    /// Reports to apply.
    pub reports: Vec<Report>,
    /// What the helper gets back.
    pub response: HookResponse,
}

/// Reply now, or hold the call until an approval is decided.
#[derive(Debug, Clone, PartialEq)]
pub enum HookResponse {
    /// Answer at once.
    Reply(HookReply),
    /// Create an approval and answer with [`RunDecoder::hold_reply`] once it is decided.
    Hold(ApprovalRequest),
}

/// What `sy-signal` writes to stdout and exits with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReply {
    /// Its stdout bytes.
    pub stdout: Vec<u8>,
    /// Its exit code.
    pub exit_code: i32,
}

/// What a decoder tells the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    /// The agent's live status.
    Status(LiveStatus),
    /// A permission request answered by typing ([`RunDecoder::input_reply`]).
    ApprovalRequested(ApprovalRequest),
    /// A request was answered in the terminal.
    ApprovalSettled {
        /// The request's key.
        key: String,
        /// What the decoder saw.
        outcome: SettledOutcome,
    },
    /// A journal event.
    Event(AgentEvent),
    /// The agent's own session handle, for resume. The last one wins.
    SessionRef(String),
}

/// The statuses an adapter reports. The engine owns the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    /// The agent is working.
    Running,
    /// The agent wants you.
    WaitingInput,
}

/// How a request answered in the terminal ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledOutcome {
    /// Allowed.
    Approved,
    /// Refused.
    Denied,
    /// Answered, but the decoder can't tell how.
    Unknown,
}

/// A journal event an agent reported (spec 0002's shapes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
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
        /// Its path in the worktree.
        path: String,
        /// How it changed.
        change: FileChange,
        /// The old path, for a rename.
        from_path: Option<String>,
    },
}

/// Who wrote a message.
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

/// How a file changed.
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

/// A permission request.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// The adapter's own id for it, unique among the run's pending approvals.
    pub key: String,
    /// One line for people (the engine cuts it to 200 characters).
    pub summary: String,
    /// What is asked for; the variant is `approval.kind`.
    pub detail: ApprovalDetail,
    /// The agent's raw payload (the engine cuts it to fit 64 KiB).
    pub raw: serde_json::Value,
}

/// What a permission request asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDetail {
    /// Run a command.
    Command {
        /// The command line.
        command: String,
        /// Where it runs, when known.
        cwd: Option<String>,
    },
    /// Edit a file.
    FileEdit {
        /// The file.
        path: String,
        /// The change, when known.
        diff: Option<String>,
    },
    /// Fetch a URL.
    Fetch {
        /// The URL.
        url: String,
    },
    /// Call an MCP tool.
    McpTool {
        /// The MCP server.
        server: String,
        /// The tool.
        tool: String,
        /// Its input.
        input: serde_json::Value,
    },
    /// Anything else.
    Other {
        /// The tool's name.
        tool: String,
        /// Its input.
        input: serde_json::Value,
    },
}

impl ApprovalDetail {
    /// The `approval.kind` text for this variant.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::FileEdit { .. } => "file_edit",
            Self::Fetch { .. } => "fetch",
            Self::McpTool { .. } => "mcp_tool",
            Self::Other { .. } => "other",
        }
    }
}

/// A decision on a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Allow or refuse.
    pub verdict: Verdict,
    /// An optional message for the agent.
    pub message: Option<String>,
}

/// Allow or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Allow.
    Approve,
    /// Refuse.
    Deny,
}

/// How to stop a run gracefully.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopPlan {
    /// Bytes to type first (for example Ctrl C).
    pub input: Vec<u8>,
    /// How long to wait after typing before SIGHUP, in ms (clamped to 0 to 10000).
    pub grace_ms: u32,
}

/// How a process ended. Exactly one field is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// Its exit code, when it exited on its own.
    pub code: Option<i32>,
    /// The number of the signal that ended it.
    pub signal: Option<i32>,
}

/// How a run's exit is classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// It did its job.
    Finished,
    /// It failed.
    Failed,
}

/// Why an adapter can't plan a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// This adapter can't do that (yet).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// `agent_options` don't fit this adapter.
    #[error("invalid agent options: {0}")]
    InvalidOptions(String),
}
