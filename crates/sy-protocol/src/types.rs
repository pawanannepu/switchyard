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
}

// ---- run ----

/// Identifies one run (one agent process and its terminal).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct RunId(pub String);

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

/// `run.resync_needed` notification: this client fell behind; attach again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResyncNeeded {
    /// The run whose output was dropped for this client.
    pub run_id: RunId,
}

// ---- dev (only in engines built with the `devtools` feature) ----

/// `dev.synthetic_run` params: fake runs that stream output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyntheticRunParams {
    /// How many runs to start.
    pub count: u32,
    /// Output rate per run.
    pub bytes_per_sec: u64,
    /// Write every `run.input` straight back as output.
    pub echo: bool,
}

/// `dev.synthetic_run` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyntheticRunResult {
    /// The new runs, ready for `run.attach`.
    pub run_ids: Vec<RunId>,
}

/// `dev.stop_runs` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StopRunsResult {
    /// How many runs were stopped.
    pub stopped: u32,
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
