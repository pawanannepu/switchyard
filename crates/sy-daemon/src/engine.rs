//! Shared engine state, and the per client stream sink with resync throttling.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use sy_core::adapters::AdapterRegistry;
use sy_core::runs::{Delivery, RunHub, RunId, RunSink};
use sy_protocol::{RunOutput, RunResyncNeeded, methods, notification_line};

use crate::lifecycle::Lifecycle;
use crate::outbox::Outbox;

/// At most one `run.resync_needed` per run per client in this window.
pub const RESYNC_THROTTLE: Duration = Duration::from_secs(2);

pub struct Engine {
    pub hub: Arc<RunHub>,
    pub lifecycle: Arc<Lifecycle>,
    pub adapters: AdapterRegistry,
    pub pid: u32,
    connections: Mutex<HashMap<u64, Arc<Outbox>>>,
    next_connection: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    pub fn new(adapters: AdapterRegistry) -> Arc<Self> {
        Arc::new(Self {
            hub: RunHub::new(),
            lifecycle: Lifecycle::new(),
            adapters,
            pid: std::process::id(),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
        })
    }

    pub fn register_connection(&self, outbox: Arc<Outbox>) -> u64 {
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        lock(&self.connections).insert(id, outbox);
        id
    }

    pub fn unregister_connection(&self, id: u64) {
        lock(&self.connections).remove(&id);
        self.hub.detach_all(id);
    }

    /// Close every client's outbox; writers drain what is queued, then hang up.
    pub fn close_all_connections(&self) {
        for outbox in lock(&self.connections).values() {
            outbox.close();
        }
    }
}

pub fn protocol_run_id(id: &RunId) -> sy_protocol::RunId {
    sy_protocol::RunId(id.0.clone())
}

#[derive(Default)]
struct ResyncState {
    last_sent: Option<Instant>,
    scheduled: bool,
}

/// One client's subscription end for every run it attached.
pub struct ClientSink {
    outbox: Arc<Outbox>,
    resync: Mutex<HashMap<RunId, ResyncState>>,
}

impl ClientSink {
    pub fn new(outbox: Arc<Outbox>) -> Arc<Self> {
        Arc::new(Self { outbox, resync: Mutex::new(HashMap::new()) })
    }

    fn resync_line(run: &RunId) -> String {
        notification_line(methods::RUN_RESYNC_NEEDED, &RunResyncNeeded { run_id: protocol_run_id(run) })
    }

    /// Tell the client to attach again, at most once per [`RESYNC_THROTTLE`] per run.
    fn request_resync(self: &Arc<Self>, run: &RunId) {
        let now = Instant::now();
        let delay = {
            let mut map = lock(&self.resync);
            let state = map.entry(run.clone()).or_default();
            if state.scheduled {
                return;
            }
            match state.last_sent {
                Some(last) if now.duration_since(last) < RESYNC_THROTTLE => {
                    state.scheduled = true;
                    RESYNC_THROTTLE - now.duration_since(last)
                }
                _ => {
                    state.last_sent = Some(now);
                    Duration::ZERO
                }
            }
        };
        if delay.is_zero() {
            self.outbox.push(Self::resync_line(run));
            return;
        }
        let this = Arc::clone(self);
        let run = run.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut map = lock(&this.resync);
                let state = map.entry(run.clone()).or_default();
                state.scheduled = false;
                state.last_sent = Some(Instant::now());
            }
            this.outbox.push(Self::resync_line(&run));
        });
    }
}

/// Wraps the sink so `deliver` can hand an `Arc` to the resync task.
pub struct ClientSinkHandle(pub Arc<ClientSink>);

impl RunSink for ClientSinkHandle {
    fn deliver(&self, run: &RunId, offset: u64, data: &[u8]) -> Delivery {
        if self.0.outbox.is_closed() {
            return Delivery::Dropped;
        }
        let output = RunOutput {
            run_id: protocol_run_id(run),
            offset,
            data_b64: base64::engine::general_purpose::STANDARD.encode(data),
        };
        if self.0.outbox.try_push(notification_line(methods::RUN_OUTPUT, &output)) {
            Delivery::Accepted
        } else {
            tracing::debug!(%run, "client fell behind, dropping its live stream");
            self.0.request_resync(run);
            Delivery::Dropped
        }
    }
}
