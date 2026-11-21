//! Method and notification names, plus the table that drives schema and TS generation.

pub const SESSION_HELLO: &str = "session.hello";
pub const ENGINE_INFO: &str = "engine.info";
pub const ENGINE_DRAIN: &str = "engine.drain";
pub const ENGINE_SHUTDOWN: &str = "engine.shutdown";
pub const RUN_ATTACH: &str = "run.attach";
pub const RUN_INPUT: &str = "run.input";
pub const RUN_RESIZE: &str = "run.resize";
pub const RUN_OUTPUT: &str = "run.output";
pub const RUN_RESYNC_NEEDED: &str = "run.resync_needed";
pub const DEV_SYNTHETIC_RUN: &str = "dev.synthetic_run";
pub const DEV_STOP_RUNS: &str = "dev.stop_runs";
pub const HOST_CONNECTION: &str = "host.connection";

/// Methods that work before `session.hello` and after an incompatible one.
/// Frozen across every protocol version.
pub const STABLE: &[&str] = &[SESSION_HELLO, ENGINE_INFO, ENGINE_DRAIN, ENGINE_SHUTDOWN];

pub fn is_stable(method: &str) -> bool {
    STABLE.contains(&method)
}

/// Request (expects a response) or notification (fire and forget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
}

/// Who sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToEngine,
    EngineToClient,
    /// App host or dev bridge to the UI. Never on the engine socket.
    HostToClient,
}

/// One row of the protocol table. Type names refer to `$defs` in the schema.
#[derive(Debug, Clone, Copy)]
pub struct MethodSpec {
    pub name: &'static str,
    pub kind: Kind,
    pub direction: Direction,
    pub params: &'static str,
    pub result: Option<&'static str>,
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
    event(RUN_OUTPUT, "RunOutput", Direction::EngineToClient),
    event(
        RUN_RESYNC_NEEDED,
        "RunResyncNeeded",
        Direction::EngineToClient,
    ),
    dev(req(
        DEV_SYNTHETIC_RUN,
        "SyntheticRunParams",
        "SyntheticRunResult",
    )),
    dev(req(DEV_STOP_RUNS, "Empty", "StopRunsResult")),
    event(HOST_CONNECTION, "HostConnection", Direction::HostToClient),
];
