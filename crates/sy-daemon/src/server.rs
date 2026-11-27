//! The socket server: accept, peer check, one reader and one writer per client,
//! and the shutdown sequence.

use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use sy_client::EnginePaths;
use sy_client::transport::LinesCodecError;
use sy_client::transport::{self, Stream};
use sy_core::adapters::AdapterRegistry;
use sy_core::agents::detect::{Detector, DetectorDeps};
use sy_core::agents::ports::LoginEnv;
use sy_core::agents::ports::ProcessTable;
use sy_core::agents::ports::RuntimeDir;
use sy_core::agents::{AgentRuns, AgentRunsDeps, Timings};
use sy_core::ports::Clock;
use sy_core::recovery;
use sy_core::runs::RunHub;
use sy_infra::agents::{
    FsRuntimeDir, InheritedEnv, PortablePty, RandomTokens, ShellLoginEnv, SysProcessTable,
    SystemProcessRunner,
};
use sy_infra::clock::SystemClock;
use sy_infra::storage::{FileRunLog, StoragePaths, sweep_backup_temps};
use sy_protocol::CLIENT_QUEUE_BYTES;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::dispatch::{self, ConnectionCtx, Session};
use crate::engine::{self, ClientSink, Engine};
use crate::faults;
use crate::lifecycle::ExitReason;
use crate::outbox::Outbox;
use crate::storage::Storage;

pub async fn run(
    paths: EnginePaths,
    config: Config,
    adapters: AdapterRegistry,
) -> anyhow::Result<()> {
    // We hold the lock, so we are the only process allowed to replace the socket.
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => tracing::info!(socket = %paths.socket.display(), "removed stale socket"),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("removing {}", paths.socket.display()));
        }
    }
    let listener = transport::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    let own_uid = transport::current_uid();

    // Storage opens before the accept loop, so no client is served without it.
    let storage_paths = StoragePaths::new(&paths.data_dir);
    storage_paths
        .ensure_dirs()
        .with_context(|| format!("preparing {}", paths.data_dir.display()))?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    // Taken once: every run a restart turns `lost` ends at this time (AC-6).
    let started_at = clock.now_ms();
    match sweep_backup_temps(&storage_paths) {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "removed unfinished backups"),
        Err(err) => tracing::warn!(%err, "could not sweep unfinished backups"),
    }
    let (committed_tx, committed_rx) = mpsc::unbounded_channel();
    let (notices_tx, notices_rx) = mpsc::unbounded_channel();
    let run_log = faults::run_log(Arc::new(FileRunLog::new(&storage_paths.runs)));
    // A failed open still serves the stable methods and `storage.recover` (AC-3).
    let storage = Storage::open(
        storage_paths,
        Arc::clone(&clock),
        Arc::clone(&run_log),
        committed_tx,
        notices_tx,
    );
    let mut leftovers = None;
    if let Some(store) = storage.store() {
        // Before any client is served: no one can see a run as live that died with the last engine.
        let recovered =
            recovery::recover_at_startup(store.as_ref(), Arc::clone(&run_log), started_at)
                .await
                .context("recovering storage after the last engine stopped")?;
        tracing::info!(
            lost_runs = recovered.lost,
            discarded_workspaces = recovered.discarded,
            deleted_logs = recovered.deleted_logs,
            removed_files = recovered.removed_files,
            "startup recovery done"
        );
        // No hook still waits for an approval the last engine held (AC-10).
        match store.expire_pending_approvals().await {
            Ok(expired) if !expired.value.is_empty() => {
                tracing::info!(expired = expired.value.len(), "expired pending approvals");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(%err, "could not expire pending approvals"),
        }
        // Before any connection: no run of this engine is in it (spec 0004, AC-10).
        match recovery::snapshot_leftovers(store.as_ref()).await {
            Ok(snapshot) => leftovers = Some((Arc::clone(&store), snapshot)),
            Err(err) => {
                tracing::warn!(%err, "could not read leftover processes; they wait for the next start")
            }
        }
    }

    // No launch dir outlives its engine (spec 0003, AC-13), and none is in use yet.
    let runtime = Arc::new(FsRuntimeDir::new(&paths.data_dir));
    match runtime.sweep() {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "removed leftover launch dirs"),
        Err(err) => tracing::warn!(%err, "could not sweep leftover launch dirs"),
    }
    // Detection probes run in `agent-runtime/` itself.
    if let Err(err) = runtime.ensure_root() {
        tracing::warn!(%err, "could not create the agent runtime dir");
    }
    let login: Arc<dyn LoginEnv> = if config.inherit_login_env {
        tracing::info!("using the engine's own environment as the login environment");
        Arc::new(InheritedEnv::capture())
    } else {
        ShellLoginEnv::new(None)
    };
    let detector = Detector::new(DetectorDeps {
        adapters: adapters.clone(),
        runner: Arc::new(SystemProcessRunner),
        login: Arc::clone(&login),
        runtime: Arc::clone(&runtime) as Arc<dyn RuntimeDir>,
        clock: Arc::clone(&clock),
    });
    let hub = RunHub::new(Arc::clone(&run_log));
    let processes: Arc<dyn ProcessTable> = Arc::new(SysProcessTable::new());
    // What a crashed engine left running, stopped while clients are served.
    if let Some((store, snapshot)) = leftovers {
        let table = Arc::clone(&processes);
        tokio::spawn(recovery::cleanup_leftovers(
            store,
            table,
            snapshot,
            Timings::default().sweep_term_wait,
        ));
    }
    let (approvals_tx, approvals_rx) = mpsc::unbounded_channel();
    let agents = AgentRuns::new(AgentRunsDeps {
        approvals: approvals_tx,
        adapters,
        hub: Arc::clone(&hub),
        pty: Arc::new(PortablePty),
        processes: Arc::clone(&processes),
        runtime,
        login: Arc::clone(&login),
        tokens: Arc::new(RandomTokens::default()),
        clock: Arc::clone(&clock),
        signal_helper: config.helper(crate::config::SIGNAL_HELPER),
        socket: paths.socket.to_string_lossy().into_owned(),
        timings: Timings::default(),
    });
    // One shared process tree scan for every live run (spec 0004, AC-7).
    tokio::spawn(AgentRuns::scan_loop(Arc::downgrade(&agents)));
    // The first login capture, then the first detection pass, in the
    // background; a launch waits for the capture (spec 0003, AC-3, AC-16).
    {
        let (login, detector) = (Arc::clone(&login), Arc::clone(&detector));
        tokio::spawn(async move {
            let captured = login.refresh().await;
            if let Some(why) = &captured.fallback {
                tracing::warn!(%why, "login environment fell back");
            }
            detector.detect().await;
        });
    }
    let engine = Engine::new(
        hub,
        agents,
        detector,
        login,
        Arc::clone(&storage),
        run_log,
        paths.data_dir.clone(),
    );
    engine::spawn_broadcaster(&engine, committed_rx, notices_rx, approvals_rx);
    engine::spawn_log_cap(&engine);
    engine::spawn_write_retry(&engine);
    engine::spawn_log_failures(&engine);
    let _flusher = engine
        .hub
        .spawn_flusher()
        .context("starting the run flusher")?;
    engine
        .lifecycle
        .spawn_idle_monitor(engine.agents.watch_live(), config.idle_exit);
    let mut exit = engine.lifecycle.watch_exit();
    let mut connections = JoinSet::new();

    tracing::info!(
        version = crate::ENGINE_VERSION,
        pid = engine.pid,
        socket = %paths.socket.display(),
        adapters = ?engine.agents.adapters().ids().collect::<Vec<_>>(),
        devtools = cfg!(feature = "devtools"),
        "engine started"
    );

    let signal_lifecycle = Arc::clone(&engine.lifecycle);
    tokio::spawn(async move {
        match sy_infra::signals::terminate().await {
            Ok(name) => signal_lifecycle.request_exit(ExitReason::Signal(name)),
            Err(err) => tracing::warn!(%err, "cannot listen for signals"),
        }
    });

    let reason = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    match transport::peer_uid(&stream).map(|peer| transport::check_peer(peer, own_uid)) {
                        Ok(Ok(())) => { connections.spawn(serve(Arc::clone(&engine), stream)); }
                        Ok(Err(rejected)) => tracing::warn!(%rejected, "refused connection"),
                        Err(err) => tracing::warn!(%err, "refused connection: cannot read peer credentials"),
                    }
                }
                Err(err) => tracing::warn!(%err, "accept failed"),
            },
            Some(()) = async { connections.join_next().await.map(|_| ()) } => {}
            changed = exit.wait_for(Option::is_some) => {
                break changed.ok().and_then(|r| *r).unwrap_or(ExitReason::Shutdown);
            }
        }
    };

    tracing::info!(?reason, "engine stopping");
    drop(listener);
    // Every live run at once: its graceful step (capped at 3 s), SIGHUP, 5 s,
    // SIGKILL, then its end is recorded (spec 0003, AC-12), then its
    // leftover sweep (spec 0004, AC-12): about 13 s at most.
    let stopped = engine.agents.live_count();
    engine.agents.stop_all().await;
    // Each connection sees the exit, finishes the request it is handling (a
    // store commit may still be in flight), closes its own outbox and drains
    // it. Only a connection still busy after the grace period is cut off.
    let _ = tokio::time::timeout(config.flush_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    engine.close_all_connections();
    connections.abort_all();
    if let Err(err) = std::fs::remove_file(&paths.socket)
        && err.kind() != ErrorKind::NotFound
    {
        tracing::warn!(%err, "could not remove socket");
    }
    storage.checkpoint().await;
    tracing::info!(stopped_runs = stopped, "engine stopped");
    Ok(())
}

async fn serve(engine: Arc<Engine>, stream: Stream) {
    let outbox = Arc::new(Outbox::new(CLIENT_QUEUE_BYTES));
    let id = engine.register_connection(Arc::clone(&outbox));
    engine.lifecycle.client_connected();
    tracing::debug!(connection = id, "client connected");

    let (read, mut write) = stream.into_split();
    let writer_outbox = Arc::clone(&outbox);
    let writer = tokio::spawn(async move {
        while let Some(batch) = writer_outbox.take_batch().await {
            let len = batch.len();
            if write.write_all(batch.as_bytes()).await.is_err() {
                writer_outbox.close();
                break;
            }
            writer_outbox.written(len);
        }
        let _ = write.shutdown().await;
    });

    let mut ctx = ConnectionCtx {
        id,
        session: Session::NoHello,
        signal: false,
        sink: ClientSink::new(Arc::clone(&outbox)),
        outbox: Arc::clone(&outbox),
        hook: None,
    };
    let mut lines = transport::line_reader(read);
    let mut exit = engine.lifecycle.watch_exit();
    loop {
        // A connection with an open `signal.hook` stays until its answer is
        // sent and the helper hangs up, even while the engine exits (AC-12).
        let hook_open = ctx.hook_open();
        let next = tokio::select! {
            next = lines.next() => next,
            _ = exit.wait_for(Option::is_some), if !hook_open => break,
        };
        match next {
            Some(Ok(text)) => {
                if let Some(response) = dispatch::line(&engine, &mut ctx, &text).await {
                    outbox.push(response);
                }
            }
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                tracing::error!(
                    connection = id,
                    "message over 1 MiB, closing this connection"
                );
                break;
            }
            Some(Err(err)) => {
                tracing::warn!(connection = id, %err, "read failed, closing this connection");
                break;
            }
            None => break,
        }
    }

    // The helper went away with its call open: nobody waits for the answer.
    if let Some(hook) = ctx.hook.take() {
        hook.abort();
    }
    outbox.close();
    engine.unregister_connection(id);
    let _ = writer.await;
    engine.lifecycle.client_disconnected();
    tracing::debug!(connection = id, "client disconnected");
}
