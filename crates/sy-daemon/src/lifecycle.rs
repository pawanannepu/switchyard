//! When the engine exits: shutdown, drain, idle, or a signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sy_core::runs::RunHub;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown,
    Drained,
    Idle,
    Signal(&'static str),
}

pub struct Lifecycle {
    clients: watch::Sender<usize>,
    draining: AtomicBool,
    exit: watch::Sender<Option<ExitReason>>,
}

impl Lifecycle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            clients: watch::Sender::new(0),
            draining: AtomicBool::new(false),
            exit: watch::Sender::new(None),
        })
    }

    pub fn client_connected(&self) {
        self.clients.send_modify(|n| *n += 1);
    }

    pub fn client_disconnected(&self) {
        self.clients.send_modify(|n| *n = n.saturating_sub(1));
    }

    #[cfg_attr(
        not(feature = "devtools"),
        expect(
            dead_code,
            reason = "only dev.synthetic_run starts runs until feature 8"
        )
    )]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// The first reason wins.
    pub fn request_exit(&self, reason: ExitReason) {
        self.exit.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }

    pub fn watch_exit(&self) -> watch::Receiver<Option<ExitReason>> {
        self.exit.subscribe()
    }

    pub fn is_exiting(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// `engine.drain`: refuse new work, exit once no agent is running, whatever clients are connected.
    pub fn drain(self: &Arc<Self>, hub: &Arc<RunHub>) {
        if self.draining.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::info!(running = hub.len(), "draining");
        let this = Arc::clone(self);
        let mut runs = hub.watch_count();
        tokio::spawn(async move {
            if runs.wait_for(|n| *n == 0).await.is_ok() {
                this.request_exit(ExitReason::Drained);
            }
        });
    }

    /// Exit after `idle` with no agent running and no client connected.
    pub fn spawn_idle_monitor(self: &Arc<Self>, hub: &Arc<RunHub>, idle: Duration) {
        let this = Arc::clone(self);
        let mut clients = self.clients.subscribe();
        let mut runs = hub.watch_count();
        tokio::spawn(async move {
            loop {
                let idle_now = *clients.borrow_and_update() == 0 && *runs.borrow_and_update() == 0;
                let changed = async {
                    tokio::select! {
                        r = clients.changed() => r.is_ok(),
                        r = runs.changed() => r.is_ok(),
                    }
                };
                if idle_now {
                    tokio::select! {
                        () = tokio::time::sleep(idle) => {
                            this.request_exit(ExitReason::Idle);
                            return;
                        }
                        alive = changed => if !alive { return },
                    }
                } else if !changed.await {
                    return;
                }
            }
        });
    }
}
