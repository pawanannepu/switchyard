//! Shared engine state, the `run.status` and `storage.status` broadcaster,
//! the degraded write retry loop, and the per client stream sink with resync
//! throttling.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use sy_core::adapters::AdapterRegistry;
use sy_core::model::{Event, EventKind, RunStatus};
use sy_core::ports::{RunLog, Store};
use sy_core::retention;
use sy_core::runs::{Delivery, RunHub, RunId, RunSink};
use sy_core::write_queue::{self, StorageNotice, WriteHealth};
use sy_protocol::{RunOutput, RunResyncNeeded, RunStatusChanged, methods, notification_line};
use tokio::sync::{Notify, mpsc};

use crate::lifecycle::Lifecycle;
use crate::outbox::Outbox;
use crate::storage::Storage;
use crate::wire;

/// At most one `run.resync_needed` per run per client in this window.
pub const RESYNC_THROTTLE: Duration = Duration::from_secs(2);

pub struct Engine {
    pub hub: Arc<RunHub>,
    /// The database: open (maybe with degraded writes), or failed to open.
    pub storage: Arc<Storage>,
    /// The run output logs (the hub appends; `run.log_read` reads).
    pub run_log: Arc<dyn RunLog>,
    pub lifecycle: Arc<Lifecycle>,
    pub adapters: AdapterRegistry,
    pub pid: u32,
    /// The data dir (the dev fixture's paths hang off it).
    #[cfg_attr(
        not(feature = "devtools"),
        expect(dead_code, reason = "only the dev fixture reads it until feature 8")
    )]
    pub data_dir: PathBuf,
    /// Wakes the log cap task (AC-10); permits coalesce, so a burst is one check.
    log_cap: Arc<Notify>,
    connections: Mutex<HashMap<u64, Connection>>,
    next_connection: AtomicU64,
}

struct Connection {
    outbox: Arc<Outbox>,
    /// Completed a compatible `session.hello`, so it gets broadcast notifications.
    hello: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    pub fn new(
        adapters: AdapterRegistry,
        storage: Arc<Storage>,
        run_log: Arc<dyn RunLog>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub: RunHub::new(Arc::clone(&run_log)),
            storage,
            run_log,
            lifecycle: Lifecycle::new(),
            adapters,
            pid: std::process::id(),
            data_dir,
            log_cap: Arc::new(Notify::new()),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
        })
    }

    /// The store, when storage is open (`ok` or `degraded_writes`).
    pub fn store(&self) -> Option<Arc<dyn Store>> {
        self.storage.store()
    }

    /// Send `storage.status` (the full health now) to every connection after hello.
    pub fn broadcast_storage_status(&self) {
        self.broadcast(&notification_line(
            methods::STORAGE_STATUS,
            &self.storage.info(),
        ));
    }

    pub fn register_connection(&self, outbox: Arc<Outbox>) -> u64 {
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        lock(&self.connections).insert(
            id,
            Connection {
                outbox,
                hello: false,
            },
        );
        id
    }

    /// The connection completed a compatible `session.hello` (or no longer has one).
    pub fn set_hello(&self, id: u64, compatible: bool) {
        if let Some(conn) = lock(&self.connections).get_mut(&id) {
            conn.hello = compatible;
        }
    }

    /// Queue a notification for every connection after a compatible hello.
    /// Control messages always queue, so this never drops.
    pub fn broadcast(&self, line: &str) {
        for conn in lock(&self.connections).values().filter(|c| c.hello) {
            conn.outbox.push(line.to_owned());
        }
    }

    /// Stop every live run: final flush to its log and delivery, on a blocking
    /// thread. Returns how many there were. Their rows stay live until the
    /// next start marks them `lost` (feature 5 replaces this with a real stop).
    pub async fn remove_all_runs(&self) -> usize {
        let hub = Arc::clone(&self.hub);
        tokio::task::spawn_blocking(move || {
            hub.run_ids()
                .iter()
                .filter(|id| hub.remove(id).is_some())
                .count()
        })
        .await
        .unwrap_or(0)
    }

    pub fn unregister_connection(&self, id: u64) {
        lock(&self.connections).remove(&id);
        self.hub.detach_all(id);
    }

    /// Close every client's outbox; writers drain what is queued, then hang up.
    pub fn close_all_connections(&self) {
        for conn in lock(&self.connections).values() {
            conn.outbox.close();
        }
    }
}

/// The `run.status` notification for a run status change, if `kind` is one.
/// `run_status_changed` is the only journal kind with a live notification.
/// `event_id` is `None` for a change that was queued, not journaled (AC-13).
pub fn run_status_line(kind: &EventKind, event_id: Option<i64>) -> Option<String> {
    let EventKind::RunStatusChanged {
        run_id,
        workspace_id,
        from,
        status,
        end_reason,
        exit_code,
        at,
    } = kind
    else {
        return None;
    };
    Some(notification_line(
        methods::RUN_STATUS,
        &RunStatusChanged {
            run_id: wire::run_id(run_id),
            workspace_id: sy_protocol::WorkspaceId(workspace_id.to_string()),
            from: wire::status(*from),
            status: wire::status(*status),
            end_reason: end_reason.map(|r| r.as_str().to_owned()),
            exit_code: *exit_code,
            at: *at,
            event_id,
        },
    ))
}

/// Run status changes already announced unjournaled (`event_id: null`), per
/// run, in order. When the queue replays one, its commit comes back through
/// the committed channel; that copy is not announced again.
#[derive(Default)]
struct Announced(HashMap<RunId, VecDeque<RunStatus>>);

impl Announced {
    fn record(&mut self, kind: &EventKind) {
        if let EventKind::RunStatusChanged { run_id, status, .. } = kind {
            self.0.entry(run_id.clone()).or_default().push_back(*status);
        }
    }

    /// True when `kind` is the replay of a change already announced.
    fn is_replay(&mut self, kind: &EventKind) -> bool {
        let EventKind::RunStatusChanged { run_id, status, .. } = kind else {
            return false;
        };
        let Some(pending) = self.0.get_mut(run_id) else {
            return false;
        };
        if pending.front() != Some(status) {
            return false;
        }
        pending.pop_front();
        if pending.is_empty() {
            self.0.remove(run_id);
        }
        true
    }
}

/// Drain the store's ordered channel of committed events and the write
/// path's notices, sending `run.status` and `storage.status` to every
/// connection. Committed events come first whenever both are ready: a
/// notice is always sent after the commits before it, so this keeps
/// notifications in the order they happened, and committed ones in
/// `event_id` order.
pub fn spawn_broadcaster(
    engine: &Arc<Engine>,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut notices: mpsc::UnboundedReceiver<StorageNotice>,
) {
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        let mut announced = Announced::default();
        loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => {
                    let Some(engine) = engine.upgrade() else { break };
                    if !announced.is_replay(&event.kind)
                        && let Some(line) = run_status_line(&event.kind, Some(event.id))
                    {
                        engine.broadcast(&line);
                    }
                    if event.kind.triggers_log_cap() {
                        engine.log_cap.notify_one();
                    }
                }
                Some(notice) = notices.recv() => {
                    let Some(engine) = engine.upgrade() else { break };
                    match notice {
                        StorageNotice::Unjournaled(kind) => {
                            announced.record(&kind);
                            if let Some(line) = run_status_line(&kind, None) {
                                engine.broadcast(&line);
                            }
                        }
                        StorageNotice::Health(health) => {
                            if health == WriteHealth::Ok {
                                // Every replayed commit came through before this notice.
                                announced.0.clear();
                                engine.log_cap.notify_one();
                            }
                            engine.broadcast_storage_status();
                        }
                    }
                }
                else => break,
            }
        }
    });
}

/// While writes are degraded, retry the queue and probe every
/// [`write_queue::RETRY_INTERVAL`] (AC-13).
pub fn spawn_write_retry(engine: &Arc<Engine>) {
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(write_queue::RETRY_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(queued) = engine.upgrade().and_then(|e| e.storage.queued()) else {
                if engine.strong_count() == 0 {
                    break;
                }
                continue;
            };
            if queued.health() != WriteHealth::Ok {
                queued.retry().await;
            }
        }
    });
}

/// Each run's first failed log append makes storage degraded and marks the
/// run's log `incomplete` (AC-13).
pub fn spawn_log_failures(engine: &Arc<Engine>) {
    let mut failures = engine.hub.log_failures();
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        while let Some((run, err)) = failures.recv().await {
            let Some(queued) = engine.upgrade().and_then(|e| e.storage.queued()) else {
                continue;
            };
            queued.log_failed(&run, &err).await;
        }
    });
}

/// Run the log cap check (AC-10) once now, at startup, and again whenever the
/// broadcaster sees a run end or an archive. One check runs at a time.
pub fn spawn_log_cap(engine: &Arc<Engine>) {
    let wake = Arc::clone(&engine.log_cap);
    wake.notify_one();
    let engine = Arc::downgrade(engine);
    tokio::spawn(async move {
        loop {
            // Wait holding only the `Notify`, so shutdown can drop the engine.
            wake.notified().await;
            let Some(woken) = engine.upgrade() else {
                break;
            };
            // Trims are writes: none while storage is failed or degraded.
            // Writes coming back to ok wakes this again.
            let Some(queued) = woken.storage.queued() else {
                continue;
            };
            if queued.health() != WriteHealth::Ok {
                continue;
            }
            let store: Arc<dyn Store> = queued;
            let hub = Arc::clone(&woken.hub);
            let live = tokio::task::spawn_blocking(move || hub.live_log_bytes())
                .await
                .unwrap_or(0);
            match retention::enforce_log_cap(store.as_ref(), Arc::clone(&woken.run_log), live).await
            {
                Ok(check) if check.trimmed > 0 => tracing::info!(
                    trimmed = check.trimmed,
                    before = check.before,
                    after = check.after,
                    cap = check.cap,
                    "log cap check trimmed run logs"
                ),
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "log cap check failed"),
            }
        }
    });
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
    /// Delivery runs on the flusher's OS thread, so delayed resyncs go through this.
    runtime: tokio::runtime::Handle,
}

impl ClientSink {
    /// Must be called inside the runtime.
    pub fn new(outbox: Arc<Outbox>) -> Arc<Self> {
        Arc::new(Self {
            outbox,
            resync: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    fn resync_line(run: &RunId) -> String {
        notification_line(
            methods::RUN_RESYNC_NEEDED,
            &RunResyncNeeded {
                run_id: wire::run_id(run),
            },
        )
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
        self.runtime.spawn(async move {
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
            run_id: wire::run_id(run),
            offset,
            data_b64: base64::engine::general_purpose::STANDARD.encode(data),
        };
        if self
            .0
            .outbox
            .try_push(notification_line(methods::RUN_OUTPUT, &output))
        {
            Delivery::Accepted
        } else {
            tracing::debug!(%run, "client fell behind, dropping its live stream");
            self.0.request_resync(run);
            Delivery::Dropped
        }
    }
}
