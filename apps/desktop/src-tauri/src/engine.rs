//! The host's one engine connection: find or start the engine, say hello,
//! relay frames for the webview, and report the connection state.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;
use sy_client::relay::{FrontendSlot, Relay};
use sy_client::{Connection, EnginePaths, HelloError};
use sy_protocol::{
    ClientKind, EngineInfo, ErrorCode, HostConnection, HostConnectionState, methods,
};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, State};

use crate::{launcher, tray};

/// How long to keep retrying after spawning the engine.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the menu bar item refreshes its counts.
const STATUS_POLL: Duration = Duration::from_secs(3);

struct LinkState {
    relay: Option<Relay>,
    status: HostConnection,
}

pub struct EngineLink {
    paths: EnginePaths,
    slot: Arc<FrontendSlot>,
    state: Mutex<LinkState>,
    connecting: AtomicBool,
    running_agents: AtomicU32,
}

impl EngineLink {
    pub fn new(paths: EnginePaths) -> Self {
        let status = HostConnection {
            state: HostConnectionState::Connecting,
            hello: None,
            message: None,
            logs_dir: Some(paths.logs_dir.display().to_string()),
        };
        Self {
            paths,
            slot: FrontendSlot::new(),
            state: Mutex::new(LinkState {
                relay: None,
                status,
            }),
            connecting: AtomicBool::new(false),
            running_agents: AtomicU32::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn relay(&self) -> Option<Relay> {
        self.lock().relay.clone()
    }

    pub fn status(&self) -> HostConnection {
        self.lock().status.clone()
    }

    pub fn running_agents(&self) -> u32 {
        self.running_agents.load(Ordering::Relaxed)
    }

    /// Ask the engine for the current count (falls back to the last one seen).
    pub async fn refresh_running_agents(&self) -> u32 {
        if let Some(relay) = self.relay()
            && let Ok(info) = relay
                .connection()
                .call::<_, EngineInfo>(methods::ENGINE_INFO, &json!({}))
                .await
        {
            self.running_agents
                .store(info.running_agents, Ordering::Relaxed);
        }
        self.running_agents()
    }

    fn set_status(&self, app: &AppHandle, state: HostConnectionState, message: Option<String>) {
        let status = {
            let mut s = self.lock();
            s.status = HostConnection {
                state,
                hello: s
                    .relay
                    .as_ref()
                    .map(|r| r.hello().clone())
                    .or_else(|| s.status.hello.clone()),
                message,
                logs_dir: Some(self.paths.logs_dir.display().to_string()),
            };
            if state != HostConnectionState::Connected && state != HostConnectionState::Incompatible
            {
                s.status.hello = None;
            }
            s.status.clone()
        };
        tracing::info!(state = ?status.state, message = ?status.message, "engine connection");
        self.slot.send_connection(&status);
        tray::show_status(app, &status, self.running_agents());
    }

    /// Connect to the engine, starting it if nothing answers. Ignored while an attempt is running.
    pub fn connect(self: &Arc<Self>, app: AppHandle) {
        if self.connecting.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            this.connect_or_launch(&app).await;
            this.connecting.store(false, Ordering::Release);
        });
    }

    async fn connect_or_launch(self: &Arc<Self>, app: &AppHandle) {
        self.set_status(app, HostConnectionState::Connecting, None);
        let mut spawned = false;
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            match self.try_hello().await {
                Ok(()) => {
                    self.on_connected(app);
                    return;
                }
                Err(HelloError::Incompatible { engine }) => {
                    let message = format!(
                        "The running engine speaks protocol {}.{}; this app needs {}.{}.",
                        engine.protocol.major,
                        engine.protocol.minor,
                        sy_protocol::PROTOCOL.major,
                        sy_protocol::PROTOCOL.minor
                    );
                    self.lock().status.hello = Some(engine);
                    self.set_status(app, HostConnectionState::Incompatible, Some(message));
                    return;
                }
                Err(err) => {
                    if !spawned {
                        spawned = true;
                        if let Err(spawn_err) = launcher::spawn(&self.paths) {
                            self.set_status(
                                app,
                                HostConnectionState::Failed,
                                Some(format!("Could not start the engine: {spawn_err}")),
                            );
                            return;
                        }
                    } else if started.elapsed() >= START_TIMEOUT {
                        self.set_status(
                            app,
                            HostConnectionState::Failed,
                            Some(format!("The engine didn't start: {err}")),
                        );
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
            }
        }
    }

    async fn try_hello(&self) -> Result<(), HelloError> {
        let (conn, notifications) = Connection::connect(&self.paths.socket)
            .await
            .map_err(|_| HelloError::Call(sy_client::CallError::Closed))?;
        let hello = conn
            .hello(ClientKind::Desktop, env!("CARGO_PKG_VERSION"))
            .await?;
        let relay = Relay::start(conn, hello, notifications, Arc::clone(&self.slot));
        self.lock().relay = Some(relay);
        Ok(())
    }

    fn on_connected(self: &Arc<Self>, app: &AppHandle) {
        let Some(relay) = self.relay() else { return };
        self.set_status(app, HostConnectionState::Connected, None);

        let (this, app2, conn) = (Arc::clone(self), app.clone(), relay.connection().clone());
        tauri::async_runtime::spawn(async move {
            conn.closed().await;
            let still_current = this
                .relay()
                .is_some_and(|r| r.hello().pid == relay.hello().pid);
            if still_current {
                this.lock().relay = None;
                this.running_agents.store(0, Ordering::Relaxed);
                this.set_status(
                    &app2,
                    HostConnectionState::Stopped,
                    Some("The engine stopped.".into()),
                );
            }
        });

        let (this, app3) = (Arc::clone(self), app.clone());
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(STATUS_POLL);
            loop {
                tick.tick().await;
                if this.relay().is_none() {
                    break;
                }
                let running = this.refresh_running_agents().await;
                tray::show_status(&app3, &this.status(), running);
            }
        });
    }
}

/// The webview subscribes once per page load; a reload replaces the old channel.
#[tauri::command]
pub async fn engine_subscribe(
    link: State<'_, Arc<EngineLink>>,
    on_message: Channel,
) -> Result<(), String> {
    link.slot.attach(Arc::new(move |frame: String| {
        let _ = on_message.send(InvokeResponseBody::Json(frame));
    }));
    link.slot.send_connection(&link.status());
    Ok(())
}

/// One JSON-RPC frame from the webview. The answer comes back on the channel.
#[tauri::command]
pub async fn engine_send(link: State<'_, Arc<EngineLink>>, frame: String) -> Result<(), String> {
    let generation = link.slot.generation();
    match link.relay() {
        Some(relay) => relay.handle_frame(generation, &frame),
        None => link.slot.reject(
            generation,
            &frame,
            ErrorCode::EngineUnavailable,
            "The engine is not connected.",
        ),
    }
    Ok(())
}

/// Start (or reconnect to) the engine after it stopped or failed to start.
#[tauri::command]
pub async fn engine_restart(
    app: AppHandle,
    link: State<'_, Arc<EngineLink>>,
) -> Result<(), String> {
    if link.relay().is_none() {
        link.inner().connect(app);
    }
    Ok(())
}
