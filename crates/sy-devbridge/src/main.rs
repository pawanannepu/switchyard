//! `sy-devbridge`: dev and test only.
//!
//! Listens on a random `127.0.0.1` port and forwards each WebSocket (which
//! must carry `?token=<token>`) to the engine socket through the same relay
//! the app host uses, so the UI behaves the same in a plain browser. At
//! startup it prints one JSON line, `{"port": …, "token": …}`, which the
//! Playwright setup reads. It never starts the engine; start that yourself.
//! It is excluded from release bundles.

use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sy_client::relay::{FrontendSlot, Relay};
use sy_client::{Connection, EnginePaths};
use sy_protocol::{ClientKind, ErrorCode, HostConnection, HostConnectionState};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let paths = EnginePaths::resolve()?;
    let token = random_token()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    println!("{}", serde_json::json!({ "port": port, "token": token }));
    tracing::info!(port, socket = %paths.socket.display(), "dev bridge listening");

    let paths = Arc::new(paths);
    let token: Arc<str> = token.into();
    loop {
        let (tcp, peer) = listener.accept().await?;
        let (paths, token) = (Arc::clone(&paths), Arc::clone(&token));
        tokio::spawn(async move {
            if let Err(err) = serve(tcp, &paths, &token).await {
                tracing::warn!(%peer, %err, "bridge connection ended");
            }
        });
    }
}

fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn token_matches(request: &Request, token: &str) -> bool {
    request
        .uri()
        .query()
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.strip_prefix("token="))
        .any(|given| {
            given.len() == token.len()
                && given
                    .bytes()
                    .zip(token.bytes())
                    .fold(0, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
}

fn connection_state(state: HostConnectionState, paths: &EnginePaths) -> HostConnection {
    HostConnection {
        state,
        hello: None,
        message: None,
        logs_dir: Some(paths.logs_dir.display().to_string()),
    }
}

// tungstenite's handshake callback returns its large `ErrorResponse` by value.
#[expect(clippy::result_large_err)]
async fn serve(tcp: TcpStream, paths: &EnginePaths, token: &str) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_hdr_async(tcp, |request: &Request, response: Response| {
        if token_matches(request, token) {
            Ok(response)
        } else {
            let mut refused = ErrorResponse::new(Some("bad or missing token".into()));
            *refused.status_mut() = StatusCode::UNAUTHORIZED;
            Err(refused)
        }
    })
    .await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if ws_tx.send(Message::text(frame)).await.is_err() {
                break;
            }
        }
    });
    let slot = FrontendSlot::new();
    let generation = slot.attach(Arc::new(move |frame| {
        let _ = out_tx.send(frame);
    }));

    let relay = connect(&paths.socket, &slot, paths).await;
    while let Some(message) = ws_rx.next().await {
        match message? {
            Message::Text(text) => match &relay {
                Some(relay) => relay.handle_frame(generation, &text),
                None => slot.reject(
                    generation,
                    &text,
                    ErrorCode::EngineUnavailable,
                    "the engine is not running",
                ),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

async fn connect(socket: &Path, slot: &Arc<FrontendSlot>, paths: &EnginePaths) -> Option<Relay> {
    let attempt = async {
        let (conn, notifications) = Connection::connect(socket).await?;
        let hello = conn
            .hello(ClientKind::Test, env!("CARGO_PKG_VERSION"))
            .await?;
        anyhow::Ok((conn, hello, notifications))
    };
    match attempt.await {
        Ok((conn, hello, notifications)) => {
            slot.send_connection(&HostConnection {
                hello: Some(hello.clone()),
                ..connection_state(HostConnectionState::Connected, paths)
            });
            let relay = Relay::start(conn, hello, notifications, Arc::clone(slot));
            let (watch_slot, watch_conn) = (Arc::clone(slot), relay.connection().clone());
            let stopped = connection_state(HostConnectionState::Stopped, paths);
            tokio::spawn(async move {
                watch_conn.closed().await;
                watch_slot.send_connection(&stopped);
            });
            Some(relay)
        }
        Err(err) => {
            slot.send_connection(&HostConnection {
                message: Some(format!(
                    "cannot reach the engine at {}: {err}",
                    socket.display()
                )),
                ..connection_state(HostConnectionState::Stopped, paths)
            });
            None
        }
    }
}
