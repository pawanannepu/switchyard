//! Method and notification names, plus the table that drives schema and TS generation.

/// Handshake: the client names itself and both sides check protocol versions.
pub const SESSION_HELLO: &str = "session.hello";
/// The engine's version, pid and current load.
pub const ENGINE_INFO: &str = "engine.info";
/// Stop accepting new work and let running agents finish.
pub const ENGINE_DRAIN: &str = "engine.drain";
/// Stop the engine and every agent it owns.
pub const ENGINE_SHUTDOWN: &str = "engine.shutdown";
/// Start receiving a run's output, with a replay of what came before.
pub const RUN_ATTACH: &str = "run.attach";
/// Send keystrokes or pasted text to a run's terminal.
pub const RUN_INPUT: &str = "run.input";
/// Resize a run's terminal.
pub const RUN_RESIZE: &str = "run.resize";
/// Read a page of a run's saved output from its log file.
pub const RUN_LOG_READ: &str = "run.log_read";
/// Stop a run: its graceful input, then SIGHUP, then SIGKILL.
pub const RUN_STOP: &str = "run.stop";
/// Journal events after a cursor, for history and reconnect catch up.
pub const EVENT_SINCE: &str = "event.since";
/// Set a broken database aside and restore the newest backup or start fresh.
pub const STORAGE_RECOVER: &str = "storage.recover";
/// Engine notification: local storage health changed (to every connection after hello).
pub const STORAGE_STATUS: &str = "storage.status";
/// Engine notification: a chunk of a run's terminal output.
pub const RUN_OUTPUT: &str = "run.output";
/// Engine notification: a run changed status (to every connection after hello).
pub const RUN_STATUS: &str = "run.status";
/// Engine notification: output was dropped for a slow client, so attach again.
pub const RUN_RESYNC_NEEDED: &str = "run.resync_needed";
/// Every registered agent adapter, with its capabilities and last detection.
pub const AGENT_LIST: &str = "agent.list";
/// Run a detection pass (or join the running one) and answer when it ends.
pub const AGENT_DETECT: &str = "agent.detect";
/// Capture the login environment again, then rerun detection in the background.
pub const ENV_REFRESH: &str = "env.refresh";
/// Approvals by state and run, keyset paged.
pub const APPROVAL_LIST: &str = "approval.list";
/// Answer a pending approval: approve or deny, with an optional message.
pub const APPROVAL_RESPOND: &str = "approval.respond";
/// Engine notification: a run asked for your permission (to every connection after hello).
pub const APPROVAL_REQUESTED: &str = "approval.requested";
/// Engine notification: an approval left `pending` (to every connection after hello).
pub const APPROVAL_DECIDED: &str = "approval.decided";
/// `sy-signal` only: relay one agent hook call to its run, and get the reply.
pub const SIGNAL_HOOK: &str = "signal.hook";
/// Dev only: create fixture rows and launch runs of an agent (the fake agent, usually).
pub const DEV_AGENT_RUN: &str = "dev.agent_run";
/// Host notification: the state of the app host's (or dev bridge's) engine connection.
pub const HOST_CONNECTION: &str = "host.connection";

/// Methods that work before `session.hello` and after an incompatible one.
/// Frozen across every protocol version.
pub const STABLE: &[&str] = &[SESSION_HELLO, ENGINE_INFO, ENGINE_DRAIN, ENGINE_SHUTDOWN];

/// Whether `method` is one of the [`STABLE`] methods.
pub fn is_stable(method: &str) -> bool {
    STABLE.contains(&method)
}

/// Request (expects a response) or notification (fire and forget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Has an `id` and expects a response.
    Request,
    /// Has no `id` and gets no response.
    Notification,
}

/// Who sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A client (the app host, dev bridge, or a test) calls the engine.
    ClientToEngine,
    /// The engine pushes to its clients.
    EngineToClient,
    /// App host or dev bridge to the UI. Never on the engine socket.
    HostToClient,
}

/// One row of the protocol table. Type names refer to `$defs` in the schema.
#[derive(Debug, Clone, Copy)]
pub struct MethodSpec {
    /// Wire name, such as `run.attach`.
    pub name: &'static str,
    /// Request or notification.
    pub kind: Kind,
    /// Who sends it.
    pub direction: Direction,
    /// Schema type name of the params.
    pub params: &'static str,
    /// Schema type name of the result; `None` for notifications.
    pub result: Option<&'static str>,
    /// Frozen across protocol versions (see [`STABLE`]).
    pub stable: bool,
    /// Only answered by engines built with the `devtools` feature.
    pub dev_only: bool,
}

const fn req(name: &'static str, params: &'static str, result: &'static str) -> MethodSpec {
    MethodSpec {
        name,
        kind: Kind::Request,
        direction: Direction::ClientToEngine,
        params,
        result: Some(result),
        stable: false,
        dev_only: false,
    }
}

const fn stable(spec: MethodSpec) -> MethodSpec {
    MethodSpec {
        stable: true,
        ..spec
    }
}

const fn dev(spec: MethodSpec) -> MethodSpec {
    MethodSpec {
        dev_only: true,
        ..spec
    }
}

const fn event(name: &'static str, params: &'static str, direction: Direction) -> MethodSpec {
    MethodSpec {
        name,
        kind: Kind::Notification,
        direction,
        params,
        result: None,
        stable: false,
        dev_only: false,
    }
}

/// Every method and notification in the protocol.
pub const ALL: &[MethodSpec] = &[
    stable(req(SESSION_HELLO, "HelloParams", "HelloResult")),
    stable(req(ENGINE_INFO, "Empty", "EngineInfo")),
    stable(req(ENGINE_DRAIN, "Empty", "Empty")),
    stable(req(ENGINE_SHUTDOWN, "Empty", "Empty")),
    req(RUN_ATTACH, "RunAttachParams", "RunAttachResult"),
    req(RUN_INPUT, "RunInputParams", "Empty"),
    req(RUN_RESIZE, "RunResizeParams", "Empty"),
    req(RUN_LOG_READ, "RunLogReadParams", "RunLogReadResult"),
    req(RUN_STOP, "RunStopParams", "Empty"),
    req(EVENT_SINCE, "EventSinceParams", "EventSinceResult"),
    req(
        STORAGE_RECOVER,
        "StorageRecoverParams",
        "StorageRecoverResult",
    ),
    event(RUN_OUTPUT, "RunOutput", Direction::EngineToClient),
    event(RUN_STATUS, "RunStatusChanged", Direction::EngineToClient),
    event(STORAGE_STATUS, "StorageInfo", Direction::EngineToClient),
    event(
        RUN_RESYNC_NEEDED,
        "RunResyncNeeded",
        Direction::EngineToClient,
    ),
    req(AGENT_LIST, "Empty", "AgentListResult"),
    req(AGENT_DETECT, "AgentDetectParams", "AgentListResult"),
    req(ENV_REFRESH, "Empty", "EnvRefreshResult"),
    req(APPROVAL_LIST, "ApprovalListParams", "ApprovalListResult"),
    req(
        APPROVAL_RESPOND,
        "ApprovalRespondParams",
        "ApprovalRespondResult",
    ),
    event(
        APPROVAL_REQUESTED,
        "ApprovalNotification",
        Direction::EngineToClient,
    ),
    event(
        APPROVAL_DECIDED,
        "ApprovalNotification",
        Direction::EngineToClient,
    ),
    req(SIGNAL_HOOK, "SignalHookParams", "SignalHookResult"),
    dev(req(DEV_AGENT_RUN, "AgentRunParams", "AgentRunResult")),
    event(HOST_CONNECTION, "HostConnection", Direction::HostToClient),
];
