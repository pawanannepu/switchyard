//! The run hub: each run's terminal output, saved to its run log, then put in
//! a replay ring buffer and fanned out to attached clients.
//!
//! **Persist before broadcast** (spec 0002, AC-5). A producer's `write` only
//! appends to the run's pending batch. The flusher takes the batch, appends it
//! to the [`RunLog`] at its exact offset, and only then puts it in the replay
//! ring and delivers it. So every byte a client receives, live or as replay,
//! is already in `runs/<run_id>.log` at the same offset. Each run has its own
//! flush lock, held across take, append, ring and deliver, so the flusher
//! thread and [`RunHub::remove`] can't interleave one run's chunks.
//!
//! Offsets count bytes in a run's whole output (per run, not per client).
//! `attach` returns the ring plus `end_offset`, the bytes flushed so far; live
//! chunks then arrive with their starting offset, and a client drops any byte
//! below `end_offset`. That gives no gap and no duplicate even when a chunk
//! straddles the attach point.
//!
//! The producer never waits on a client or on disk: `write` takes a short
//! lock, and the blocking append happens on the flusher's own OS thread. A
//! sink that can't keep up answers [`Delivery::Dropped`] and is unsubscribed
//! from that run.
//!
//! If an append fails (disk full), the run's log is cut back to its last good
//! byte and never appended to again, while its output keeps streaming from
//! memory (AC-13). [`RemovedRun::log_complete`] reports it, and so does
//! [`RunHub::log_failures`], at the moment it happens.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

pub use crate::ids::RunId;
use crate::ports::{RunLog, RunLogError};

/// Most output kept for replay, per run.
pub const REPLAY_BYTES: usize = 2 * 1024 * 1024;

/// How often pending output is saved and batched out to subscribers.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// Control messages a run's producer can be sent.
const CONTROL_QUEUE: usize = 256;

/// What a sink did with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The sink took the chunk.
    Accepted,
    /// The sink fell behind (or went away); unsubscribe it from this run.
    Dropped,
}

/// Receives live output. Must not block. Called from the flusher thread.
pub trait RunSink: Send + Sync {
    /// Hand over `data`, which starts at byte `offset` of `run`'s whole output.
    fn deliver(&self, run: &RunId, offset: u64, data: &[u8]) -> Delivery;
}

/// Identifies one subscriber (one client connection) across runs.
pub type SubscriberId = u64;

/// Sent to a run's producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunControl {
    /// Bytes to write to the run's terminal.
    Input(Vec<u8>),
    /// A new terminal size.
    Resize {
        /// Width, in character cells.
        cols: u16,
        /// Height, in character cells.
        rows: u16,
    },
}

/// Why an operation on a run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// No live run has that id.
    #[error("run not found")]
    NotFound,
    /// The producer's control queue is full.
    #[error("run input queue is full")]
    Busy,
    /// A live run already has that id.
    #[error("a run with this id is already live")]
    Exists,
}

/// What [`RunHub::remove`] reports about a run it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovedRun {
    /// Bytes in the run's log file after the final flush (its `log_bytes`).
    pub log_bytes: u64,
    /// False when an append failed, so the file holds only a prefix (`incomplete`).
    pub log_complete: bool,
}

#[derive(Default)]
struct RunState {
    ring: VecDeque<u8>,
    /// Bytes the producer wrote.
    total: u64,
    /// Bytes flushed (in the ring and delivered). The ring ends here.
    flushed: u64,
    pending: Vec<u8>,
    subscribers: Vec<(SubscriberId, Arc<dyn RunSink>)>,
}

struct FlushState {
    /// Bytes in the log file.
    log_len: u64,
    /// Cleared for good after the first failed append.
    log_ok: bool,
}

struct RunEntry {
    id: RunId,
    state: Mutex<RunState>,
    flush: Mutex<FlushState>,
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
        s.pending.extend_from_slice(data);
        s.total += data.len() as u64;
    }

    fn flush(&self, log: &dyn RunLog, failures: &FailureSink) -> RemovedRun {
        let mut f = lock(&self.flush);
        let (offset, data) = {
            let mut s = lock(&self.state);
            (s.flushed, std::mem::take(&mut s.pending))
        };
        if !data.is_empty() {
            self.persist(&mut f, log, failures, offset, &data);
            self.deliver(offset, &data);
        }
        RemovedRun {
            log_bytes: f.log_len,
            log_complete: f.log_ok,
        }
    }

    fn persist(
        &self,
        f: &mut FlushState,
        log: &dyn RunLog,
        failures: &FailureSink,
        offset: u64,
        data: &[u8],
    ) {
        if !f.log_ok {
            return;
        }
        match log.append(&self.id, offset, data) {
            Ok(()) => f.log_len = offset + data.len() as u64,
            Err(err) => {
                f.log_ok = false;
                tracing::error!(run = %self.id, %err, "run log append failed; output now streams from memory only");
                if let Err(err) = log.truncate(&self.id, f.log_len) {
                    tracing::error!(run = %self.id, %err, "could not cut the run log back to its last good byte");
                }
                if let Some(sink) = lock(failures).as_ref() {
                    let _ = sink.send((self.id.clone(), err));
                }
            }
        }
    }

    fn deliver(&self, offset: u64, data: &[u8]) {
        let subscribers = {
            let mut s = lock(&self.state);
            let keep = data.len().min(REPLAY_BYTES);
            let overflow = (s.ring.len() + keep).saturating_sub(REPLAY_BYTES);
            s.ring.drain(..overflow);
            s.ring.extend(&data[data.len() - keep..]);
            s.flushed = offset + data.len() as u64;
            s.subscribers.clone()
        };
        let dropped: Vec<SubscriberId> = subscribers
            .iter()
            .filter(|(_, sink)| sink.deliver(&self.id, offset, data) == Delivery::Dropped)
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
    /// The run this writer belongs to.
    pub fn id(&self) -> &RunId {
        &self.entry.id
    }

    /// Append output. Never waits on a client or on disk.
    pub fn write(&self, data: &[u8]) {
        self.entry.write(data);
    }

    /// Total bytes written so far.
    pub fn total(&self) -> u64 {
        lock(&self.entry.state).total
    }
}

/// Where a run's first failed append is reported.
type FailureSink = Mutex<Option<mpsc::UnboundedSender<(RunId, RunLogError)>>>;

/// Every live run in the engine.
pub struct RunHub {
    runs: Mutex<HashMap<RunId, Arc<RunEntry>>>,
    log: Arc<dyn RunLog>,
    count: watch::Sender<usize>,
    failures: FailureSink,
}

impl RunHub {
    /// An empty hub that saves output through `log`.
    pub fn new(log: Arc<dyn RunLog>) -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(HashMap::new()),
            log,
            count: watch::Sender::new(0),
            failures: Mutex::new(None),
        })
    }

    /// Receive each run's first failed append (its log is `incomplete` from
    /// then on), as it happens. A later call replaces the earlier receiver.
    pub fn log_failures(&self) -> mpsc::UnboundedReceiver<(RunId, RunLogError)> {
        let (tx, rx) = mpsc::unbounded_channel();
        *lock(&self.failures) = Some(tx);
        rx
    }

    /// Add a live run with the id the store created it with. The producer
    /// writes through the [`RunWriter`] and reads [`RunControl`] messages until
    /// the receiver closes (the run was removed).
    pub fn create_run(
        &self,
        id: RunId,
    ) -> Result<(RunWriter, mpsc::Receiver<RunControl>), RunError> {
        let (tx, rx) = mpsc::channel(CONTROL_QUEUE);
        let entry = Arc::new(RunEntry {
            id: id.clone(),
            state: Mutex::default(),
            flush: Mutex::new(FlushState {
                log_len: 0,
                log_ok: true,
            }),
            control: Mutex::new(Some(tx)),
        });
        let len = {
            let mut runs = lock(&self.runs);
            if runs.contains_key(&id) {
                return Err(RunError::Exists);
            }
            runs.insert(id, Arc::clone(&entry));
            runs.len()
        };
        self.count.send_replace(len);
        Ok((RunWriter { entry }, rx))
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
        Ok(([a, b].concat(), s.flushed))
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

    /// Stop a run: save and deliver what it wrote (the final flush), then close
    /// its producer's control channel. Blocks on the run log, so async callers
    /// use `spawn_blocking`. `None` when no live run has that id.
    pub fn remove(&self, run: &RunId) -> Option<RemovedRun> {
        let (entry, len) = {
            let mut runs = lock(&self.runs);
            let entry = runs.remove(run);
            (entry, runs.len())
        };
        let entry = entry?;
        let removed = entry.flush(self.log.as_ref(), &self.failures);
        lock(&entry.control).take();
        self.count.send_replace(len);
        Some(removed)
    }

    /// Bytes appended to a live run's log file so far (its last good byte
    /// after a failed append), or `None` when the run isn't live. Waits for an
    /// append in progress on that run, so async callers use `spawn_blocking`.
    pub fn log_len(&self, run: &RunId) -> Option<u64> {
        let entry = self.entry(run).ok()?;
        let len = lock(&entry.flush).log_len;
        Some(len)
    }

    /// The bytes appended so far across every live run's log, the live part
    /// of the log cap's total (AC-10). Blocks like [`RunHub::log_len`].
    pub fn live_log_bytes(&self) -> u64 {
        self.run_ids()
            .iter()
            .filter_map(|run| self.log_len(run))
            .fold(0, u64::saturating_add)
    }

    /// The ids of every live run.
    pub fn run_ids(&self) -> Vec<RunId> {
        lock(&self.runs).keys().cloned().collect()
    }

    /// How many runs are live.
    pub fn len(&self) -> usize {
        lock(&self.runs).len()
    }

    /// True when no run is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Watch the number of live runs.
    pub fn watch_count(&self) -> watch::Receiver<usize> {
        self.count.subscribe()
    }

    /// Save and deliver every run's pending output once. Blocks on the run log.
    pub fn flush(&self) {
        let entries: Vec<_> = lock(&self.runs).values().cloned().collect();
        for entry in entries {
            entry.flush(self.log.as_ref(), &self.failures);
        }
    }

    /// Flush every [`FLUSH_INTERVAL`] on a dedicated OS thread (appends are
    /// blocking writes) until the hub is dropped.
    pub fn spawn_flusher(self: &Arc<Self>) -> std::io::Result<std::thread::JoinHandle<()>> {
        let hub = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("run-flusher".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(FLUSH_INTERVAL);
                    let Some(hub) = hub.upgrade() else { break };
                    hub.flush();
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run log in memory that records every append and can be told to fail.
    #[derive(Default)]
    struct MemLog {
        files: Mutex<HashMap<RunId, Vec<u8>>>,
        appends: Mutex<Vec<(u64, usize)>>,
        fail_after: Mutex<Option<usize>>,
    }

    impl MemLog {
        fn len_of(&self, run: &RunId) -> u64 {
            lock(&self.files).get(run).map_or(0, |f| f.len() as u64)
        }
    }

    impl RunLog for MemLog {
        fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
            {
                let mut fail = lock(&self.fail_after);
                if let Some(n) = fail.as_mut() {
                    if *n == 0 {
                        // A partial write, which the hub must cut back.
                        lock(&self.files).entry(run.clone()).or_default().push(b'!');
                        return Err(RunLogError::Full);
                    }
                    *n -= 1;
                }
            }
            lock(&self.appends).push((offset, bytes.len()));
            let mut files = lock(&self.files);
            let file = files.entry(run.clone()).or_default();
            let start = usize::try_from(offset).unwrap();
            file.resize(start.max(file.len()), 0);
            file.splice(
                start..(start + bytes.len()).min(file.len()),
                bytes.iter().copied(),
            );
            Ok(())
        }
        fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError> {
            if let Some(f) = lock(&self.files).get_mut(run) {
                f.truncate(usize::try_from(len).unwrap());
            }
            Ok(())
        }
        fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError> {
            let files = lock(&self.files);
            let f = files.get(run).ok_or(RunLogError::NotFound)?;
            let start = usize::try_from(offset).unwrap().min(f.len());
            Ok(f[start..(start + max).min(f.len())].to_vec())
        }
        fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
            Ok(self.len_of(run))
        }
        fn delete(&self, run: &RunId) -> Result<(), RunLogError> {
            lock(&self.files).remove(run);
            Ok(())
        }
        fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
            Ok(lock(&self.files)
                .iter()
                .map(|(k, v)| (k.clone(), v.len() as u64))
                .collect())
        }
    }

    /// Records chunks and checks each was already in the log when delivered.
    struct Collect {
        log: Arc<MemLog>,
        chunks: Mutex<Vec<(u64, Vec<u8>)>>,
        early: Mutex<usize>,
        refuse: bool,
    }

    impl Collect {
        fn new(log: &Arc<MemLog>) -> Arc<Self> {
            Arc::new(Self {
                log: Arc::clone(log),
                chunks: Mutex::default(),
                early: Mutex::new(0),
                refuse: false,
            })
        }
    }

    impl RunSink for Collect {
        fn deliver(&self, run: &RunId, offset: u64, data: &[u8]) -> Delivery {
            if self.refuse {
                return Delivery::Dropped;
            }
            if self.log.len_of(run) < offset + data.len() as u64 {
                *lock(&self.early) += 1;
            }
            lock(&self.chunks).push((offset, data.to_vec()));
            Delivery::Accepted
        }
    }

    fn hub() -> (Arc<RunHub>, Arc<MemLog>) {
        let log = Arc::new(MemLog::default());
        (RunHub::new(log.clone()), log)
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
            let skip = usize::try_from(cursor.saturating_sub(*offset)).unwrap();
            out.extend_from_slice(&data[skip..]);
            cursor = end;
        }
        out
    }

    #[test]
    fn attach_mid_batch_should_show_no_gap_and_no_duplicate() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        writer.write(b"hello ");
        hub.flush();
        writer.write(b"wor");
        let sink = Collect::new(&log);
        let (replay, end) = hub.attach(writer.id(), 1, sink.clone()).unwrap();
        writer.write(b"ld");
        hub.flush();
        let chunks = lock(&sink.chunks).clone();
        assert_eq!(reassemble(&replay, end, &chunks), b"hello world");
    }

    #[test]
    fn attach_should_never_replay_unsaved_bytes() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        writer.write(b"saved");
        hub.flush();
        writer.write(b" pending");
        let (replay, end) = hub.attach(writer.id(), 1, Collect::new(&log)).unwrap();
        assert_eq!(
            (replay.as_slice(), end, log.len_of(writer.id())),
            (&b"saved"[..], 5, 5)
        );
    }

    #[test]
    fn every_delivered_byte_should_be_in_the_log_at_its_offset() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        let sink = Collect::new(&log);
        hub.attach(writer.id(), 1, sink.clone()).unwrap();
        for i in 0..50_u8 {
            writer.write(&[i; 100]);
            hub.flush();
        }
        let file = lock(&log.files).get(writer.id()).cloned().unwrap();
        let chunks = lock(&sink.chunks).clone();
        let all_match = chunks.iter().all(|(offset, data)| {
            let start = usize::try_from(*offset).unwrap();
            &file[start..start + data.len()] == data.as_slice()
        });
        assert!(all_match && *lock(&sink.early) == 0);
    }

    #[test]
    fn remove_racing_the_flusher_should_append_in_offset_order_and_never_deliver_early() {
        for _ in 0..20 {
            let (hub, log) = hub();
            let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
            let id = writer.id().clone();
            let sink = Collect::new(&log);
            hub.attach(&id, 1, sink.clone()).unwrap();
            let flusher = {
                let hub = Arc::clone(&hub);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        hub.flush();
                    }
                })
            };
            for i in 0..200_u32 {
                writer.write(&i.to_le_bytes());
            }
            let removed = hub.remove(&id).unwrap();
            flusher.join().unwrap();

            let appends = lock(&log.appends).clone();
            let contiguous = appends.windows(2).all(|w| w[0].0 + w[0].1 as u64 == w[1].0);
            assert!(contiguous, "appends out of order: {appends:?}");
            assert_eq!((removed.log_bytes, log.len_of(&id)), (800, 800));
            assert_eq!(*lock(&sink.early), 0);
        }
    }

    #[test]
    fn failed_append_should_cut_the_log_back_and_keep_streaming_from_memory() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        let sink = Collect::new(&log);
        hub.attach(writer.id(), 1, sink.clone()).unwrap();
        *lock(&log.fail_after) = Some(1);
        writer.write(b"good");
        hub.flush();
        writer.write(b"lost");
        hub.flush();
        writer.write(b"more");
        let removed = hub.remove(writer.id()).unwrap();
        let shown = reassemble(&[], 0, &lock(&sink.chunks));
        assert_eq!(
            (removed, log.len_of(writer.id()), shown.as_slice()),
            (
                RemovedRun {
                    log_bytes: 4,
                    log_complete: false
                },
                4,
                &b"goodlostmore"[..]
            )
        );
    }

    #[test]
    fn first_failed_append_should_be_reported_once() {
        let (hub, log) = hub();
        let mut failures = hub.log_failures();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        *lock(&log.fail_after) = Some(0);
        for _ in 0..3 {
            writer.write(b"lost");
            hub.flush();
        }
        let first = failures.try_recv().unwrap();
        assert_eq!(
            (first, failures.try_recv().is_err()),
            ((writer.id().clone(), RunLogError::Full), true)
        );
    }

    #[test]
    fn log_len_should_count_appended_bytes_only_and_stop_at_the_last_good_byte() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        writer.write(b"saved");
        let before_flush = hub.log_len(writer.id());
        hub.flush();
        let after_flush = hub.log_len(writer.id());
        *lock(&log.fail_after) = Some(0);
        writer.write(b"lost");
        hub.flush();
        let after_failure = hub.log_len(writer.id());
        hub.remove(writer.id());
        assert_eq!(
            (
                before_flush,
                after_flush,
                after_failure,
                hub.log_len(writer.id())
            ),
            (Some(0), Some(5), Some(5), None)
        );
    }

    #[test]
    fn ring_should_keep_only_the_latest_replay_bytes() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        writer.write(&vec![b'a'; REPLAY_BYTES]);
        writer.write(b"tail");
        hub.flush();
        let (replay, end) = hub.attach(writer.id(), 1, Collect::new(&log)).unwrap();
        assert!(
            replay.len() == REPLAY_BYTES
                && replay.ends_with(b"tail")
                && end == REPLAY_BYTES as u64 + 4
        );
    }

    #[test]
    fn dropped_sink_should_be_unsubscribed() {
        let (hub, log) = hub();
        let (writer, _rx) = hub.create_run(RunId::new()).unwrap();
        let slow = Arc::new(Collect {
            refuse: true,
            ..Arc::into_inner(Collect::new(&log)).unwrap()
        });
        hub.attach(writer.id(), 1, slow).unwrap();
        writer.write(b"x");
        hub.flush();
        let entry = hub.entry(writer.id()).unwrap();
        assert!(lock(&entry.state).subscribers.is_empty());
    }

    #[test]
    fn remove_should_close_the_producer_control_channel() {
        let (hub, _log) = hub();
        let (writer, mut rx) = hub.create_run(RunId::new()).unwrap();
        hub.remove(writer.id());
        assert!(rx.try_recv().is_err() && rx.is_closed());
    }

    #[test]
    fn create_run_should_refuse_a_live_id() {
        let (hub, _log) = hub();
        let id = RunId::new();
        let _first = hub.create_run(id.clone()).unwrap();
        assert!(matches!(hub.create_run(id), Err(RunError::Exists)));
    }

    #[test]
    fn count_should_follow_create_and_remove() {
        let (hub, _log) = hub();
        let watch = hub.watch_count();
        let (a, _ra) = hub.create_run(RunId::new()).unwrap();
        let (_b, _rb) = hub.create_run(RunId::new()).unwrap();
        hub.remove(a.id());
        assert_eq!(*watch.borrow(), 1);
    }
}
