//! A client's outgoing queue, capped in bytes.
//!
//! Responses and control notifications always queue. Live stream chunks are
//! refused once the queue holds more than the cap, which is how a slow client
//! loses its live stream (and gets `run.resync_needed`) without ever slowing
//! an agent or another client.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

/// Largest single write the writer task takes at once.
const MAX_BATCH_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct State {
    queue: VecDeque<String>,
    /// Bytes queued plus bytes taken by the writer but not yet written.
    bytes: usize,
    closed: bool,
}

pub struct Outbox {
    state: Mutex<State>,
    notify: Notify,
    cap: usize,
}

impl Outbox {
    pub fn new(cap: usize) -> Self {
        Self { state: Mutex::default(), notify: Notify::new(), cap }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a response or control message. False once closed.
    pub fn push(&self, line: String) -> bool {
        let mut s = self.lock();
        if s.closed {
            return false;
        }
        s.bytes += line.len();
        s.queue.push_back(line);
        drop(s);
        self.notify.notify_one();
        true
    }

    /// Queue a live stream chunk unless that would exceed the cap. False means dropped.
    pub fn try_push(&self, line: String) -> bool {
        let mut s = self.lock();
        if s.closed || s.bytes + line.len() > self.cap {
            return false;
        }
        s.bytes += line.len();
        s.queue.push_back(line);
        drop(s);
        self.notify.notify_one();
        true
    }

    /// Wait for queued lines and take up to one batch. `None` once closed and drained.
    pub async fn take_batch(&self) -> Option<String> {
        loop {
            {
                let mut s = self.lock();
                if let Some(first) = s.queue.pop_front() {
                    let mut batch = first;
                    while let Some(next) = s.queue.front() {
                        if batch.len() + next.len() > MAX_BATCH_BYTES {
                            break;
                        }
                        if let Some(next) = s.queue.pop_front() {
                            batch.push_str(&next);
                        }
                    }
                    return Some(batch);
                }
                if s.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// The writer finished writing `n` bytes to the socket.
    pub fn written(&self, n: usize) {
        let mut s = self.lock();
        s.bytes = s.bytes.saturating_sub(n);
    }

    /// No more lines. The writer still drains what is queued.
    pub fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_push_should_refuse_past_the_cap() {
        let outbox = Outbox::new(10);
        assert!(outbox.try_push("12345678".into()));
        assert!(!outbox.try_push("123".into()));
    }

    #[test]
    fn push_should_queue_control_messages_past_the_cap() {
        let outbox = Outbox::new(4);
        outbox.push("12345678".into());
        assert!(outbox.push("resync".into()));
    }

    #[tokio::test]
    async fn written_should_free_room_for_stream_chunks() {
        let outbox = Outbox::new(10);
        outbox.try_push("12345678".into());
        let batch = outbox.take_batch().await.unwrap();
        outbox.written(batch.len());
        assert!(outbox.try_push("1234567".into()));
    }

    #[tokio::test]
    async fn take_batch_should_drain_before_reporting_closed() {
        let outbox = Outbox::new(100);
        outbox.push("last\n".into());
        outbox.close();
        assert_eq!(outbox.take_batch().await.as_deref(), Some("last\n"));
    }
}
