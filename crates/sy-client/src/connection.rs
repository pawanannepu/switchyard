//! A client connection to the engine: sends requests, matches responses by id,
//! and hands notifications out as raw lines (so a relay can forward them unchanged).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sy_protocol::{
    ClientKind, HelloParams, HelloResult, Message, PROTOCOL, RpcError, methods, notification_line, request_line,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::transport;

/// How many notifications may queue before the reader stops reading the socket.
/// A full queue pushes back on the engine, which then drops our live stream and
/// asks us to resync, instead of this process buffering without limit.
const NOTIFICATION_QUEUE: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("engine error: {0}")]
    Rpc(RpcError),
    #[error("engine connection closed")]
    Closed,
    #[error("unexpected response shape: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum HelloError {
    #[error(transparent)]
    Call(#[from] CallError),
    #[error("engine speaks protocol {}.{}, this client speaks {}.{}", .engine.protocol.major, .engine.protocol.minor, PROTOCOL.major, PROTOCOL.minor)]
    Incompatible { engine: HelloResult },
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;

struct Inner {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    closed: watch::Receiver<bool>,
}

/// A cheap to clone handle to one engine connection.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

/// Raw notification lines from the engine, in order, without the trailing newline.
pub type Notifications = mpsc::Receiver<String>;

impl Connection {
    /// Connect to the engine socket. Does not say hello; call [`Connection::hello`] next.
    pub async fn connect(socket: &Path) -> std::io::Result<(Connection, Notifications)> {
        let stream = transport::connect(socket).await?;
        let (read, mut write) = stream.into_split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (notif_tx, notif_rx) = mpsc::channel(NOTIFICATION_QUEUE);
        let (closed_tx, closed_rx) = watch::channel(false);

        let inner = Arc::new(Inner { out: out_tx, pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1), closed: closed_rx });

        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = transport::line_reader(read);
            while let Some(Ok(line)) = lines.next().await {
                match Message::parse(&line) {
                    Ok(Message::Response { id, outcome }) => {
                        let waiter = id.as_u64().and_then(|id| reader_inner.pending_lock().remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(outcome);
                        }
                    }
                    Ok(Message::Notification { .. }) => {
                        if notif_tx.send(line).await.is_err() {
                            // Nobody listens for notifications; keep serving responses.
                        }
                    }
                    Ok(Message::Request { method, .. }) => tracing::debug!(%method, "ignoring request from engine"),
                    Err(err) => tracing::warn!(?err, "unparseable line from engine"),
                }
            }
            let _ = closed_tx.send(true);
            // Dropping the waiters wakes every pending call with `Closed`.
            reader_inner.pending_lock().clear();
        });

        Ok((Connection { inner }, notif_rx))
    }

    /// `session.hello`, failing if the protocol majors differ.
    pub async fn hello(&self, client: ClientKind, client_version: &str) -> Result<HelloResult, HelloError> {
        let params = HelloParams { client, client_version: client_version.to_owned(), protocol: PROTOCOL };
        let hello: HelloResult = self.call(methods::SESSION_HELLO, &params).await?;
        if hello.protocol.is_compatible_with(PROTOCOL) {
            Ok(hello)
        } else {
            Err(HelloError::Incompatible { engine: hello })
        }
    }

    /// A typed request.
    pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P) -> Result<R, CallError> {
        let params = serde_json::to_value(params)?;
        let result = self.call_value(method, params).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// An untyped request, as the relay uses it.
    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, CallError> {
        if self.is_closed() {
            return Err(CallError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending_lock().insert(id, tx);
        if self.inner.out.send(request_line(&Value::from(id), method, &params)).is_err() {
            self.inner.pending_lock().remove(&id);
            return Err(CallError::Closed);
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(CallError::Rpc(err)),
            Err(_) => Err(CallError::Closed),
        }
    }

    /// Send a notification (no response expected).
    pub fn notify(&self, method: &str, params: &Value) -> Result<(), CallError> {
        self.inner.out.send(notification_line(method, params)).map_err(|_| CallError::Closed)
    }

    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves once the engine side of the connection is gone.
    pub async fn closed(&self) {
        let mut closed = self.inner.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }
}

impl Inner {
    fn pending_lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>> {
        // A panic while holding this lock cannot leave the map inconsistent.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
