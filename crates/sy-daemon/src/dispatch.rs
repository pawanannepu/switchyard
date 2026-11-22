//! Turns one request into one response.

use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sy_core::ports::{Store, StoreError};
use sy_core::run_logs::{LogReadError, read_run_log};
use sy_core::runs::{RunControl, RunError, RunId};
use sy_protocol::{
    Empty, EngineInfo, ErrorCode, EventSinceParams, EventSinceResult, HelloParams, HelloResult,
    MAX_INPUT_BYTES, MAX_LOG_READ_BYTES, Message, PROTOCOL, RpcError, RunAttachParams,
    RunAttachResult, RunInputParams, RunLogReadParams, RunLogReadResult, RunResizeParams,
    StorageRecoverParams, StorageRecoverResult, envelope::ParseFailure, methods, response_line,
};

use crate::ENGINE_VERSION;
use crate::engine::{ClientSink, ClientSinkHandle, Engine};
use crate::lifecycle::ExitReason;
use crate::storage::RecoverError;
use crate::wire;

/// Where a connection stands with `session.hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    NoHello,
    Compatible,
    Incompatible,
}

pub struct ConnectionCtx {
    pub id: u64,
    pub session: Session,
    pub sink: Arc<ClientSink>,
}

/// Handle one line. Returns the response line, if the message wants one.
pub async fn line(engine: &Arc<Engine>, ctx: &mut ConnectionCtx, text: &str) -> Option<String> {
    match Message::parse(text) {
        Err(ParseFailure::Json(msg)) => Some(response_line(
            &Value::Null,
            &Err(RpcError::new(ErrorCode::ParseError, msg)),
        )),
        Err(ParseFailure::Shape { id, reason }) => Some(response_line(
            &id,
            &Err(RpcError::new(ErrorCode::InvalidRequest, reason)),
        )),
        Ok(Message::Request { id, method, params }) => Some(response_line(
            &id,
            &request(engine, ctx, &method, params).await,
        )),
        Ok(Message::Notification { method, .. }) => {
            tracing::debug!(%method, "ignoring client notification");
            None
        }
        Ok(Message::Response { .. }) => None,
    }
}

fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(ErrorCode::InvalidParams, e.to_string()))
}

fn ok<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::new(ErrorCode::InternalError, e.to_string()))
}

fn run_error(err: RunError, run: &RunId) -> RpcError {
    match err {
        RunError::NotFound | RunError::Exists => {
            RpcError::new(ErrorCode::RunNotFound, format!("no live run {run}"))
        }
        RunError::Busy => RpcError::new(
            ErrorCode::InputBusy,
            format!("run {run} is not taking input right now"),
        ),
    }
}

fn storage_unavailable() -> RpcError {
    RpcError::new(
        ErrorCode::StorageUnavailable,
        "storage is unavailable; see engine.info storage, and storage.recover",
    )
}

/// A store failure on a read. Feature 8 gives the write side's variants their own codes.
fn store_error(err: &StoreError) -> RpcError {
    match err {
        StoreError::Unavailable => storage_unavailable(),
        other => RpcError::new(ErrorCode::InternalError, other.to_string()),
    }
}

/// The store, or `StorageUnavailable` in degraded storage mode.
pub fn store(engine: &Engine) -> Result<Arc<dyn Store>, RpcError> {
    engine.store().ok_or_else(storage_unavailable)
}

/// `storage.recover` (AC-4).
async fn storage_recover(engine: &Arc<Engine>, p: StorageRecoverParams) -> Result<Value, RpcError> {
    if !p.confirm {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            "storage.recover sets your database aside; pass confirm: true",
        ));
    }
    let storage = engine
        .storage
        .recover(p.action)
        .await
        .map_err(|err| match err {
            RecoverError::NotNeeded => RpcError::new(
                ErrorCode::StorageRecoverNotNeeded,
                "storage is working; there is nothing to recover",
            ),
            RecoverError::BackupMissing => RpcError::new(
                ErrorCode::BackupMissing,
                "no backup passes its integrity check; nothing was changed",
            ),
            RecoverError::Io(message) => RpcError::new(ErrorCode::InternalError, message),
        })?;
    engine.broadcast_storage_status();
    ok(&StorageRecoverResult { storage })
}

fn log_unavailable(run: &RunId) -> RpcError {
    RpcError::new(
        ErrorCode::RunLogUnavailable,
        format!("the log of run {run} is no longer available"),
    )
}

/// `run.log_read`: a page of a run's saved output (AC-12).
async fn log_read(engine: &Arc<Engine>, p: RunLogReadParams) -> Result<Value, RpcError> {
    let run = core_run_id(&p.run_id)?;
    let row = store(engine)?
        .get_run(&run)
        .await
        .map_err(|err| match err {
            StoreError::NotFound => RpcError::new(ErrorCode::RunNotFound, format!("no run {run}")),
            other => store_error(&other),
        })?;
    let max = match p.max_bytes {
        None | Some(0) => MAX_LOG_READ_BYTES,
        Some(n) => usize::try_from(n).map_or(MAX_LOG_READ_BYTES, |n| n.min(MAX_LOG_READ_BYTES)),
    };
    let hub = Arc::clone(&engine.hub);
    let log = Arc::clone(&engine.run_log);
    let chunk = tokio::task::spawn_blocking(move || {
        read_run_log(log.as_ref(), &row, hub.log_len(&row.id), p.offset, max)
    })
    .await
    .map_err(|e| RpcError::new(ErrorCode::InternalError, e.to_string()))?
    .map_err(|err| match err {
        LogReadError::Unavailable => log_unavailable(&run),
        LogReadError::Io(e) => RpcError::new(ErrorCode::InternalError, e.to_string()),
    })?;
    ok(&RunLogReadResult {
        offset: chunk.offset,
        data_b64: base64::engine::general_purpose::STANDARD.encode(&chunk.data),
        eof: chunk.eof,
        log_bytes: chunk.log_bytes,
    })
}

/// `event.since`: journal events after a cursor (AC-11).
async fn event_since(engine: &Arc<Engine>, p: EventSinceParams) -> Result<Value, RpcError> {
    if p.cursor < 0 {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            "cursor must be 0 or more",
        ));
    }
    let filter = wire::event_filter(&p)?;
    let page = store(engine)?
        .events_since(p.cursor, p.limit.unwrap_or(0), filter)
        .await
        .map_err(|e| store_error(&e))?;
    ok(&EventSinceResult {
        events: page.events.iter().map(wire::event).collect(),
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    })
}

/// A wire run id as a typed id. Text that isn't a run id names no run.
fn core_run_id(id: &sy_protocol::RunId) -> Result<RunId, RpcError> {
    RunId::parse(&id.0)
        .map_err(|_| RpcError::new(ErrorCode::RunNotFound, format!("no run {}", id.0)))
}

async fn request(
    engine: &Arc<Engine>,
    ctx: &mut ConnectionCtx,
    method: &str,
    raw: Value,
) -> Result<Value, RpcError> {
    if engine.lifecycle.is_exiting() && method != methods::ENGINE_INFO {
        return Err(RpcError::new(
            ErrorCode::EngineShuttingDown,
            "the engine is shutting down",
        ));
    }
    if !methods::is_stable(method) {
        match ctx.session {
            Session::Compatible => {}
            Session::NoHello => {
                return Err(RpcError::new(
                    ErrorCode::HelloRequired,
                    "call session.hello first",
                ));
            }
            Session::Incompatible => {
                return Err(RpcError::new(
                    ErrorCode::IncompatibleProtocol,
                    "protocol major differs; only stable methods work",
                ));
            }
        }
        // Degraded storage mode (AC-3): only the stable methods and the way out.
        if method != methods::STORAGE_RECOVER && engine.storage.is_failed() {
            return Err(storage_unavailable());
        }
    }

    match method {
        methods::SESSION_HELLO => {
            let hello: HelloParams = params(raw)?;
            let compatible = hello.protocol.is_compatible_with(PROTOCOL);
            ctx.session = if compatible {
                Session::Compatible
            } else {
                Session::Incompatible
            };
            engine.set_hello(ctx.id, compatible);
            tracing::info!(
                connection = ctx.id,
                client = ?hello.client,
                client_version = %hello.client_version,
                protocol = ?hello.protocol,
                compatible,
                "hello"
            );
            ok(&HelloResult {
                engine_version: ENGINE_VERSION.into(),
                pid: engine.pid,
                protocol: PROTOCOL,
            })
        }
        methods::ENGINE_INFO => {
            let _: Empty = params(raw)?;
            ok(&EngineInfo {
                engine_version: ENGINE_VERSION.into(),
                pid: engine.pid,
                protocol: PROTOCOL,
                running_agents: u32::try_from(engine.hub.len()).unwrap_or(u32::MAX),
                storage: Some(engine.storage.info()),
            })
        }
        methods::ENGINE_DRAIN => {
            let _: Empty = params(raw)?;
            engine.lifecycle.drain(&engine.hub);
            ok(&Empty {})
        }
        methods::ENGINE_SHUTDOWN => {
            let _: Empty = params(raw)?;
            tracing::info!(connection = ctx.id, "shutdown requested");
            engine.lifecycle.request_exit(ExitReason::Shutdown);
            ok(&Empty {})
        }
        methods::RUN_ATTACH => {
            let p: RunAttachParams = params(raw)?;
            let run = core_run_id(&p.run_id)?;
            let sink = Arc::new(ClientSinkHandle(Arc::clone(&ctx.sink)));
            let (replay, end_offset) = engine
                .hub
                .attach(&run, ctx.id, sink)
                .map_err(|e| run_error(e, &run))?;
            ok(&RunAttachResult {
                replay_b64: base64::engine::general_purpose::STANDARD.encode(replay),
                end_offset,
            })
        }
        methods::RUN_INPUT => {
            let p: RunInputParams = params(raw)?;
            let run = core_run_id(&p.run_id)?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(p.data_b64.as_bytes())
                .map_err(|e| RpcError::new(ErrorCode::InvalidParams, format!("data_b64: {e}")))?;
            if data.len() > MAX_INPUT_BYTES {
                return Err(RpcError::new(
                    ErrorCode::InputTooLarge,
                    format!(
                        "{} bytes is over the {MAX_INPUT_BYTES} byte limit; split it",
                        data.len()
                    ),
                ));
            }
            engine
                .hub
                .control(&run, RunControl::Input(data))
                .map_err(|e| run_error(e, &run))?;
            ok(&Empty {})
        }
        methods::RUN_RESIZE => {
            let p: RunResizeParams = params(raw)?;
            let run = core_run_id(&p.run_id)?;
            engine
                .hub
                .control(
                    &run,
                    RunControl::Resize {
                        cols: p.cols,
                        rows: p.rows,
                    },
                )
                .map_err(|e| run_error(e, &run))?;
            ok(&Empty {})
        }
        methods::RUN_LOG_READ => log_read(engine, params(raw)?).await,
        methods::EVENT_SINCE => event_since(engine, params(raw)?).await,
        methods::STORAGE_RECOVER => storage_recover(engine, params(raw)?).await,
        #[cfg(feature = "devtools")]
        methods::DEV_SYNTHETIC_RUN => {
            if engine.lifecycle.is_draining() {
                return Err(RpcError::new(
                    ErrorCode::EngineDraining,
                    "the engine is draining and takes no new runs",
                ));
            }
            let p: sy_protocol::SyntheticRunParams = params(raw)?;
            ok(&crate::dev::synthetic_run(engine, &p).await?)
        }
        #[cfg(feature = "devtools")]
        methods::DEV_STOP_RUNS => {
            let _: Empty = params(raw)?;
            ok(&crate::dev::stop_runs(engine).await?)
        }
        other => Err(RpcError::new(
            ErrorCode::MethodNotFound,
            format!("no method {other}"),
        )),
    }
}
