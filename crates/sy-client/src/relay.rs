//! Forwards UI frames to the engine and back.
//!
//! Used by the app host (the Tauri channel) and the dev bridge (a WebSocket).
//! The host holds one engine connection and does its `session.hello` itself.
//! UI requests go out under the host's own ids and come back with the UI's id;
//! engine notifications pass through unchanged. When the UI reattaches (a page
//! reload), every request still in flight for the old page gets a
//! `RequestAbandoned` error and its late response is dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use sy_protocol::{
    ErrorCode, HelloResult, HostConnection, Message, RpcError, envelope::ParseFailure, methods,
    notification_line, response_line,
};

use crate::connection::{CallError, Connection, Notifications};

/// Delivers one frame (a JSON text, no trailing newline) to the UI.
pub type Sink = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Default)]
struct SlotState {
    generation: u64,
    sink: Option<Sink>,
    next_key: u64,
    /// key -> (generation, UI request id)
    in_flight: HashMap<u64, (u64, Value)>,
}

/// The UI end of the relay. Survives engine reconnects; a new page replaces the sink.
#[derive(Default)]
pub struct FrontendSlot {
    state: Mutex<SlotState>,
}

impl FrontendSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Attach a new page. Returns its generation. Requests in flight for the old page are abandoned.
    pub fn attach(&self, sink: Sink) -> u64 {
        let (old_sink, abandoned) = {
            let mut s = self.lock();
            s.generation += 1;
            let abandoned: Vec<Value> = s.in_flight.drain().map(|(_, (_, id))| id).collect();
            (s.sink.replace(sink), abandoned)
        };
        if let Some(old) = old_sink {
            for id in abandoned {
                old(frame(&response_line(
                    &id,
                    &Err(abandoned_error("the page reloaded")),
                )));
            }
        }
        self.lock().generation
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Send a frame to the current page, if any.
    pub fn send(&self, text: String) {
        let sink = self.lock().sink.clone();
        if let Some(sink) = sink {
            sink(text);
        }
    }

    /// Send the `host.connection` state to the current page.
    pub fn send_connection(&self, state: &HostConnection) {
        self.send(frame(&notification_line(methods::HOST_CONNECTION, state)));
    }

    fn begin(&self, generation: u64, id: Value) -> Option<u64> {
        let mut s = self.lock();
        if s.generation != generation {
            return None;
        }
        let key = s.next_key;
        s.next_key += 1;
        s.in_flight.insert(key, (generation, id));
        Some(key)
    }

    fn finish(&self, key: u64, outcome: Result<Value, RpcError>) {
        let (entry, sink) = {
            let mut s = self.lock();
            (s.in_flight.remove(&key), s.sink.clone())
        };
        // An entry missing here was abandoned by a reload: drop the late response.
        if let (Some((_, id)), Some(sink)) = (entry, sink) {
            sink(frame(&response_line(&id, &outcome)));
        }
    }

    /// Answer a UI request with an error without involving the engine
    /// (for example while there is no engine connection).
    pub fn reject(&self, generation: u64, text: &str, code: ErrorCode, message: &str) {
        if self.generation() != generation {
            return;
        }
        match Message::parse(text) {
            Ok(Message::Request { id, .. }) => self.send(frame(&response_line(
                &id,
                &Err(RpcError::new(code, message)),
            ))),
            Ok(_) => {}
            Err(failure) => self.send(parse_failure_frame(failure)),
        }
    }
}

fn frame(line: &str) -> String {
    line.trim_end_matches('\n').to_owned()
}

fn abandoned_error(why: &str) -> RpcError {
    RpcError::new(
        ErrorCode::RequestAbandoned,
        format!("request abandoned: {why}"),
    )
}

fn parse_failure_frame(failure: ParseFailure) -> String {
    let (id, err) = match failure {
        ParseFailure::Json(msg) => (Value::Null, RpcError::new(ErrorCode::ParseError, msg)),
        ParseFailure::Shape { id, reason } => {
            (id, RpcError::new(ErrorCode::InvalidRequest, reason))
        }
    };
    frame(&response_line(&id, &Err(err)))
}

/// One engine connection bound to a [`FrontendSlot`].
#[derive(Clone)]
pub struct Relay {
    conn: Connection,
    hello: HelloResult,
    slot: Arc<FrontendSlot>,
}

impl Relay {
    /// Start relaying: engine notifications flow to the slot from now on.
    pub fn start(
        conn: Connection,
        hello: HelloResult,
        mut notifications: Notifications,
        slot: Arc<FrontendSlot>,
    ) -> Self {
        let forward_slot = Arc::clone(&slot);
        tokio::spawn(async move {
            while let Some(line) = notifications.recv().await {
                forward_slot.send(line);
            }
        });
        Self { conn, hello, slot }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    /// Handle one frame from the page of `generation`.
    pub fn handle_frame(&self, generation: u64, text: &str) {
        match Message::parse(text) {
            Err(failure) => self.slot.send(parse_failure_frame(failure)),
            Ok(Message::Request { id, method, params }) => {
                if method == methods::SESSION_HELLO {
                    // The host already said hello on this connection; answer from that.
                    let hello = serde_json::to_value(&self.hello).unwrap_or(Value::Null);
                    self.slot.send(frame(&response_line(&id, &Ok(hello))));
                    return;
                }
                let Some(key) = self.slot.begin(generation, id) else {
                    return;
                };
                let conn = self.conn.clone();
                let slot = Arc::clone(&self.slot);
                tokio::spawn(async move {
                    let outcome = match conn.call_value(&method, params).await {
                        Ok(value) => Ok(value),
                        Err(CallError::Rpc(err)) => Err(err),
                        Err(CallError::Closed) => {
                            Err(abandoned_error("the engine connection closed"))
                        }
                        Err(CallError::Decode(err)) => {
                            Err(RpcError::new(ErrorCode::InternalError, err.to_string()))
                        }
                    };
                    slot.finish(key, outcome);
                });
            }
            Ok(Message::Notification { method, params }) => {
                if let Err(err) = self.conn.notify(&method, &params) {
                    tracing::debug!(%method, %err, "dropping UI notification");
                }
            }
            Ok(Message::Response { .. }) => tracing::debug!("ignoring response frame from UI"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collecting_sink() -> (Sink, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        (Arc::new(move |f| seen2.lock().unwrap().push(f)), seen)
    }

    #[test]
    fn attach_should_abandon_in_flight_requests_of_the_old_page() {
        let slot = FrontendSlot::new();
        let (old, old_seen) = collecting_sink();
        let generation = slot.attach(old);
        slot.begin(generation, json!(42)).unwrap();
        let (new, _) = collecting_sink();
        slot.attach(new);
        let frames = old_seen.lock().unwrap().clone();
        assert!(
            frames.len() == 1 && frames[0].contains("-32091") && frames[0].contains("42"),
            "{frames:?}"
        );
    }

    #[test]
    fn late_response_after_reload_should_be_dropped() {
        let slot = FrontendSlot::new();
        let (old, _) = collecting_sink();
        let generation = slot.attach(old);
        let key = slot.begin(generation, json!(1)).unwrap();
        let (new, new_seen) = collecting_sink();
        slot.attach(new);
        slot.finish(key, Ok(json!({})));
        assert!(new_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn begin_should_refuse_a_stale_generation() {
        let slot = FrontendSlot::new();
        let (sink, _) = collecting_sink();
        let generation = slot.attach(sink);
        let (sink2, _) = collecting_sink();
        slot.attach(sink2);
        assert!(slot.begin(generation, json!(1)).is_none());
    }
}
