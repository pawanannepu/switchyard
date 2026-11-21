//! The run hub: each run's terminal output, a replay ring buffer, and fan out
//! to attached clients.
//!
//! Offsets count bytes in a run's whole output (per run, not per client).
//! `attach` returns the ring buffer plus `end_offset`, the total produced so
//! far; live chunks then arrive with their starting offset, and a client drops
//! any byte below `end_offset`. That gives no gap and no duplicate even when a
//! chunk straddles the attach point.
//!
//! The producer never waits on a client: `write` only appends under a short
//! lock, and delivery happens on the flusher. A sink that can't keep up
//! answers [`Delivery::Dropped`] and is unsubscribed from that run.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

/// Most output kept for replay, per run.
pub const REPLAY_BYTES: usize = 2 * 1024 * 1024;

/// How often pending output is batched out to subscribers.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// Control messages a run's producer can be sent.
const CONTROL_QUEUE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub String);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a sink did with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    /// The sink fell behind (or went away); unsubscribe it from this run.
    Dropped,
}

/// Receives live output. Must not block.
pub trait RunSink: Send + Sync {
    fn deliver(&self, run: &RunId, offset: u64, data: &[u8]) -> Delivery;
}

/// Identifies one subscriber (one client connection) across runs.
pub type SubscriberId = u64;

/// Sent to a run's producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunControl {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("run not found")]
    NotFound,
    /// The producer's control queue is full.
    #[error("run input queue is full")]
    Busy,
}

#[derive(Default)]
struct RunState {
    ring: VecDeque<u8>,
    total: u64,
    pending: Vec<u8>,
    pending_offset: u64,
    subscribers: Vec<(SubscriberId, Arc<dyn RunSink>)>,
}

struct RunEntry {
    id: RunId,
    state: Mutex<RunState>,
    control: Mutex<Option<mpsc::Sender<RunControl>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // No invariant spans a panic inside these short critical sections.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RunEntry {
    fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut s = lock(&self.state);
        let keep = data.len().min(REPLAY_BYTES);
        let overflow = (s.ring.len() + keep).saturating_sub(REPLAY_BYTES);
        s.ring.drain(..overflow);
        s.ring.extend(&data[data.len() - keep..]);
        s.pending.extend_from_slice(data);
        s.total += data.len() as u64;
    }

    fn flush(&self) {
        let (offset, data, subscribers) = {
            let mut s = lock(&self.state);
            if s.pending.is_empty() {
                return;
            }
            let data = std::mem::take(&mut s.pending);
            let offset = s.pending_offset;
            s.pending_offset = s.total;
            (offset, data, s.subscribers.clone())
        };
        let dropped: Vec<SubscriberId> = subscribers
            .iter()
            .filter(|(_, sink)| sink.deliver(&self.id, offset, &data) == Delivery::Dropped)
            .map(|(id, _)| *id)
            .collect();
        if !dropped.is_empty() {
            lock(&self.state)
                .subscribers
                .retain(|(id, _)| !dropped.contains(id));
        }
    }
}

/// Handed to a run's producer to write output.
pub struct RunWriter {
    entry: Arc<RunEntry>,
}

impl RunWriter {
    pub fn id(&self) -> &RunId {
        &self.entry.id
    }

    pub fn write(&self, data: &[u8]) {
        self.entry.write(data);
    }

    /// Total bytes written so far.
    pub fn total(&self) -> u64 {
        lock(&self.entry.state).total
    }
}

/// Every live run in the engine.
pub struct RunHub {
    runs: Mutex<HashMap<RunId, Arc<RunEntry>>>,
    next: AtomicU64,
    count: watch::Sender<usize>,
}

impl RunHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            count: watch::Sender::new(0),
        })
    }

    /// Create a run. The producer writes through the [`RunWriter`] and reads
    /// [`RunControl`] messages until the receiver closes (the run was removed).
    pub fn create_run(&self) -> (RunWriter, mpsc::Receiver<RunControl>) {
        let id = RunId(format!("run-{}", self.next.fetch_add(1, Ordering::Relaxed)));
        let (tx, rx) = mpsc::channel(CONTROL_QUEUE);
        let entry = Arc::new(RunEntry {
            id: id.clone(),
            state: Mutex::default(),
            control: Mutex::new(Some(tx)),
        });
        let len = {
            let mut runs = lock(&self.runs);
            runs.insert(id, Arc::clone(&entry));
            runs.len()
        };
        self.count.send_replace(len);
        (RunWriter { entry }, rx)
    }

    fn entry(&self, run: &RunId) -> Result<Arc<RunEntry>, RunError> {
        lock(&self.runs).get(run).cloned().ok_or(RunError::NotFound)
    }

    /// Subscribe `sink` to a run. Returns the replay and `end_offset`.
    /// Attaching again with the same subscriber replaces the old subscription.
    pub fn attach(
        &self,
        run: &RunId,
        subscriber: SubscriberId,
        sink: Arc<dyn RunSink>,
    ) -> Result<(Vec<u8>, u64), RunError> {
        let entry = self.entry(run)?;
        let mut s = lock(&entry.state);
        s.subscribers.retain(|(id, _)| *id != subscriber);
        s.subscribers.push((subscriber, sink));
        let (a, b) = s.ring.as_slices();
        Ok(([a, b].concat(), s.total))
    }

    /// Remove a subscriber from every run (its connection closed).
    pub fn detach_all(&self, subscriber: SubscriberId) {
        let entries: Vec<_> = lock(&self.runs).values().cloned().collect();
        for entry in entries {
            lock(&entry.state)
                .subscribers
                .retain(|(id, _)| *id != subscriber);
        }
    }

    /// Send input or a resize to a run's producer. Never waits.
    pub fn control(&self, run: &RunId, message: RunControl) -> Result<(), RunError> {
        let entry = self.entry(run)?;
        let sender = lock(&entry.control).clone().ok_or(RunError::NotFound)?;
        sender.try_send(message).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => RunError::Busy,
            mpsc::error::TrySendError::Closed(_) => RunError::NotFound,
        })
    }

    /// Stop a run: flush what it wrote and close its producer's control channel.
    pub fn remove(&self, run: &RunId) -> bool {
        let (entry, len) = {
            let mut runs = lock(&self.runs);
            let entry = runs.remove(run);
            (entry, runs.len())
        };
        let Some(entry) = entry else { return false };
        entry.flush();
        lock(&entry.control).take();
        self.count.send_replace(len);
        true
    }

    /// Stop every run. Returns how many there were.
    pub fn remove_all(&self) -> usize {
        let ids: Vec<RunId> = lock(&self.runs).keys().cloned().collect();
        ids.iter().filter(|id| self.remove(id)).count()
    }

    pub fn len(&self) -> usize {
        lock(&self.runs).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Watch the number of running runs.
    pub fn watch_count(&self) -> watch::Receiver<usize> {
        self.count.subscribe()
    }

    /// Deliver every run's pending output once.
    pub fn flush(&self) {
        let entries: Vec<_> = lock(&self.runs).values().cloned().collect();
        for entry in entries {
            entry.flush();
        }
    }

    /// Flush every [`FLUSH_INTERVAL`] until the hub is dropped.
    pub fn spawn_flusher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(hub) = hub.upgrade() else { break };
                hub.flush();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect {
        chunks: Mutex<Vec<(u64, Vec<u8>)>>,
        refuse: bool,
    }

    impl RunSink for Collect {
        fn deliver(&self, _run: &RunId, offset: u64, data: &[u8]) -> Delivery {
            if self.refuse {
                return Delivery::Dropped;
            }
            lock(&self.chunks).push((offset, data.to_vec()));
            Delivery::Accepted
        }
    }

    /// What a client shows after attach plus live chunks, trimming below end_offset.
    fn reassemble(replay: &[u8], end_offset: u64, chunks: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut out = replay.to_vec();
        let mut cursor = end_offset;
        for (offset, data) in chunks {
            let end = offset + data.len() as u64;
            if end <= cursor {
                continue;
            }
            let skip = cursor.saturating_sub(*offset) as usize;
            out.extend_from_slice(&data[skip..]);
            cursor = end;
        }
        out
    }

    #[test]
    fn attach_mid_batch_should_show_no_gap_and_no_duplicate() {
        let hub = RunHub::new();
        let (writer, _rx) = hub.create_run();
        writer.write(b"hello ");
        hub.flush();
        writer.write(b"wor");
        let sink = Arc::new(Collect::default());
        let (replay, end) = hub.attach(writer.id(), 1, sink.clone()).unwrap();
        writer.write(b"ld");
        hub.flush();
        let chunks = lock(&sink.chunks).clone();
        assert_eq!(reassemble(&replay, end, &chunks), b"hello world");
    }

    #[test]
    fn ring_should_keep_only_the_latest_replay_bytes() {
        let hub = RunHub::new();
        let (writer, _rx) = hub.create_run();
        writer.write(&vec![b'a'; REPLAY_BYTES]);
        writer.write(b"tail");
        let (replay, end) = hub
            .attach(writer.id(), 1, Arc::new(Collect::default()))
            .unwrap();
        assert!(
            replay.len() == REPLAY_BYTES
                && replay.ends_with(b"tail")
                && end == REPLAY_BYTES as u64 + 4
        );
    }

    #[test]
    fn dropped_sink_should_be_unsubscribed() {
        let hub = RunHub::new();
        let (writer, _rx) = hub.create_run();
        let slow = Arc::new(Collect {
            refuse: true,
            ..Default::default()
        });
        hub.attach(writer.id(), 1, slow).unwrap();
        writer.write(b"x");
        hub.flush();
        let entry = hub.entry(writer.id()).unwrap();
        assert!(lock(&entry.state).subscribers.is_empty());
    }

    #[test]
    fn remove_should_close_the_producer_control_channel() {
        let hub = RunHub::new();
        let (writer, mut rx) = hub.create_run();
        hub.remove(writer.id());
        assert!(rx.try_recv().is_err() && rx.is_closed());
    }

    #[test]
    fn count_should_follow_create_and_remove() {
        let hub = RunHub::new();
        let watch = hub.watch_count();
        let (a, _ra) = hub.create_run();
        let (_b, _rb) = hub.create_run();
        hub.remove(a.id());
        assert_eq!(*watch.borrow(), 1);
    }
}
