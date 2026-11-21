//! Turns one request into one response.

use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sy_core::runs::{RunControl, RunError, RunId};
use sy_protocol::{
    Empty, EngineInfo, ErrorCode, HelloParams, HelloResult, MAX_INPUT_BYTES, Message, PROTOCOL,
    RpcError, RunAttachParams, RunAttachResult, RunInputParams, RunResizeParams,
    envelope::ParseFailure, methods, response_line,
};

use crate::ENGINE_VERSION;
use crate::engine::{ClientSink, ClientSinkHandle, Engine};
use crate::lifecycle::ExitReason;

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
pub fn line(engine: &Arc<Engine>, ctx: &mut ConnectionCtx, text: &str) -> Option<String> {
    match Message::parse(text) {
        Err(ParseFailure::Json(msg)) => Some(response_line(
            &Value::Null,
            &Err(RpcError::new(ErrorCode::ParseError, msg)),
        )),
        Err(ParseFailure::Shape { id, reason }) => Some(response_line(
            &id,
            &Err(RpcError::new(ErrorCode::InvalidRequest, reason)),
        )),
        Ok(Message::Request { id, method, params }) => {
            Some(response_line(&id, &request(engine, ctx, &method, params)))
        }
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
        RunError::NotFound => RpcError::new(ErrorCode::RunNotFound, format!("no run {run}")),
        RunError::Busy => RpcError::new(
            ErrorCode::InputBusy,
            format!("run {run} is not taking input right now"),
        ),
    }
}

fn core_run_id(id: sy_protocol::RunId) -> RunId {
    RunId(id.0)
}

fn request(
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
            let run = core_run_id(p.run_id);
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
            let run = core_run_id(p.run_id);
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
            let run = core_run_id(p.run_id);
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
        #[cfg(feature = "devtools")]
        methods::DEV_SYNTHETIC_RUN => {
            if engine.lifecycle.is_draining() {
                return Err(RpcError::new(
                    ErrorCode::EngineDraining,
                    "the engine is draining and takes no new runs",
                ));
            }
            let p: sy_protocol::SyntheticRunParams = params(raw)?;
            ok(&crate::dev::synthetic_run(&engine.hub, &p)?)
        }
        #[cfg(feature = "devtools")]
        methods::DEV_STOP_RUNS => {
            let _: Empty = params(raw)?;
            let stopped = u32::try_from(engine.hub.remove_all()).unwrap_or(u32::MAX);
            ok(&sy_protocol::StopRunsResult { stopped })
        }
        other => Err(RpcError::new(
            ErrorCode::MethodNotFound,
            format!("no method {other}"),
        )),
    }
}
