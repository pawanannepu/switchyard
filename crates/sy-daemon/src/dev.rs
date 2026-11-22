//! `dev.*` methods, compiled only with the `devtools` feature.
//!
//! `dev.synthetic_run` creates fake runs that stream ANSI colored log lines at
//! a set rate and, with `echo`, write every `run.input` straight back as
//! output. The scaffold's load check uses it until feature 4's fake agent lands.
//!
//! Every synthetic run is a real row (spec 0002, AC-15): one fixture project,
//! one task per call, one workspace plus one `agent` run per synthetic run,
//! all through the `Store`, and output is saved through the `RunLog` before
//! it is delivered. This is the end to end thread for persist before broadcast.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sy_core::ids::{RunId, WorkspaceId};
use sy_core::model::{
    Actor, EndReason, InitialWorkspaceState, NewProject, NewRun, NewTask, NewWorkspace, Project,
    RunEnd, RunKind, RunStatus, RunTransition,
};
use sy_core::ports::{Store, StoreError};
use sy_core::runs::{RunControl, RunWriter};
use sy_protocol::{
    ErrorCode, RpcError, StopRunsResult, SyntheticRunParams, SyntheticRunResult, TaskId,
    WorkspaceId as WireWorkspaceId,
};
use tokio::sync::mpsc;

use crate::dispatch;
use crate::engine::Engine;
use crate::wire;

const MAX_RUNS: u32 = 64;
const MAX_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;
const TICK: Duration = Duration::from_millis(16);

const FILLER: &str =
    "the quick brown fox jumps over the lazy dog while switchyard keeps every agent in its lane ";

const FIXTURE_DIR: &str = "dev-fixture";

fn store_error(err: &StoreError) -> RpcError {
    // Feature 8 gives the store errors their own protocol codes.
    RpcError::new(ErrorCode::InternalError, err.to_string())
}

/// The dev fixture project, created on first use and found by its path after.
async fn fixture_project(engine: &Engine, store: &dyn Store) -> Result<Project, StoreError> {
    let repo_path = engine
        .data_dir
        .join(FIXTURE_DIR)
        .to_string_lossy()
        .into_owned();
    if let Some(project) = store.find_project_by_repo_path(&repo_path).await? {
        return Ok(project);
    }
    let created = store
        .create_project(
            Actor::User,
            NewProject {
                name: "Dev fixture".into(),
                repo_path,
                base_branch: "main".into(),
            },
        )
        .await?;
    Ok(created.value)
}

fn fixture_workspace(engine: &Engine) -> NewWorkspace {
    let id = WorkspaceId::new();
    let worktree_path = engine
        .data_dir
        .join(FIXTURE_DIR)
        .join(id.as_str())
        .to_string_lossy()
        .into_owned();
    NewWorkspace {
        branch: format!("dev/{id}"),
        id,
        agent_id: "synthetic".into(),
        agent_options: "{}".into(),
        worktree_path,
        base_commit: String::new(),
        state: InitialWorkspaceState::Ready,
    }
}

pub async fn synthetic_run(
    engine: &Arc<Engine>,
    p: &SyntheticRunParams,
) -> Result<SyntheticRunResult, RpcError> {
    if p.count == 0 || p.count > MAX_RUNS {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!("count must be 1 to {MAX_RUNS}"),
        ));
    }
    if p.bytes_per_sec > MAX_BYTES_PER_SEC {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!("bytes_per_sec must be at most {MAX_BYTES_PER_SEC}"),
        ));
    }
    let store = dispatch::store(engine)?;
    let project = fixture_project(engine, store.as_ref())
        .await
        .map_err(|e| store_error(&e))?;
    let workspaces = (0..p.count).map(|_| fixture_workspace(engine)).collect();
    let (task, workspaces) = store
        .create_task(
            Actor::User,
            NewTask {
                project_id: project.id,
                title: "Synthetic runs".into(),
                prompt: String::new(),
                base_ref: "main".into(),
                parent_task_id: None,
                workspaces,
            },
        )
        .await
        .map_err(|e| store_error(&e))?
        .value;

    let mut run_ids = Vec::with_capacity(workspaces.len());
    for ws in &workspaces {
        let id = start_run(engine, store.as_ref(), &ws.id, p.bytes_per_sec, p.echo).await?;
        run_ids.push(wire::run_id(&id));
    }
    tracing::info!(
        count = p.count,
        bytes_per_sec = p.bytes_per_sec,
        echo = p.echo,
        task = %task.id,
        "synthetic runs started"
    );
    Ok(SyntheticRunResult {
        run_ids,
        task_id: TaskId(task.id.to_string()),
        workspace_ids: workspaces
            .iter()
            .map(|w| WireWorkspaceId(w.id.to_string()))
            .collect(),
    })
}

/// The per run order from spec 0002: `create_run` (queued), then the hub
/// entry, then `starting`, then `running`, then the producer.
async fn start_run(
    engine: &Arc<Engine>,
    store: &dyn Store,
    workspace: &WorkspaceId,
    bytes_per_sec: u64,
    echo: bool,
) -> Result<RunId, RpcError> {
    let id = RunId::new();
    store
        .create_run(
            Actor::User,
            NewRun {
                id: id.clone(),
                workspace_id: workspace.clone(),
                kind: RunKind::Agent,
                input: None,
                resumes_run_id: None,
            },
        )
        .await
        .map_err(|e| store_error(&e))?;
    let (writer, control) = engine
        .hub
        .create_run(id.clone())
        .map_err(|e| RpcError::new(ErrorCode::InternalError, e.to_string()))?;
    for status in [RunStatus::Starting, RunStatus::Running] {
        store
            .transition_run(Actor::User, &id, RunTransition::to(status))
            .await
            .map_err(|e| store_error(&e))?;
    }
    tokio::spawn(produce(writer, control, bytes_per_sec, echo));
    Ok(id)
}

/// `dev.stop_runs`: for each live run, the final flush (append and deliver),
/// then `stopped` with the final `log_bytes`, whose commit sends `run.status`.
pub async fn stop_runs(engine: &Arc<Engine>) -> Result<StopRunsResult, RpcError> {
    let store = dispatch::store(engine)?;
    let mut stopped = 0_u32;
    for id in engine.hub.run_ids() {
        let hub = Arc::clone(&engine.hub);
        let run = id.clone();
        let removed = tokio::task::spawn_blocking(move || hub.remove(&run))
            .await
            .map_err(|e| RpcError::new(ErrorCode::InternalError, e.to_string()))?;
        let Some(removed) = removed else { continue };
        // A failed append already marked the log `incomplete` when it happened (AC-13).
        store
            .transition_run(
                Actor::User,
                &id,
                RunTransition::end(
                    RunStatus::Stopped,
                    RunEnd {
                        reason: EndReason::UserStop,
                        exit_code: None,
                        log_bytes: removed.log_bytes,
                    },
                ),
            )
            .await
            .map_err(|e| store_error(&e))?;
        stopped += 1;
    }
    Ok(StopRunsResult { stopped })
}

async fn produce(
    writer: RunWriter,
    mut control: mpsc::Receiver<RunControl>,
    bytes_per_sec: u64,
    echo: bool,
) {
    let started = Instant::now();
    let mut last = started;
    let mut budget = 0.0_f64;
    let mut line_no = 0_u64;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    writer.write(
        format!(
            "\x1b[1msynthetic run {}\x1b[0m ({bytes_per_sec} B/s, echo {echo})\r\n",
            writer.id()
        )
        .as_bytes(),
    );

    loop {
        tokio::select! {
            _ = tick.tick(), if bytes_per_sec > 0 => {
                let now = Instant::now();
                let earned = bytes_per_sec as f64 * now.duration_since(last).as_secs_f64();
                budget += earned;
                last = now;
                let mut buf = String::new();
                while (buf.len() as f64) < budget {
                    line_no += 1;
                    let start = usize::try_from(line_no % 40).unwrap_or(0);
                    let filler = &FILLER[start..start + 40];
                    buf.push_str(&format!(
                        "\x1b[2m{:>9.3}s\x1b[0m \x1b[36m{}\x1b[0m \x1b[32mINFO\x1b[0m line {line_no:>7} {filler}\r\n",
                        started.elapsed().as_secs_f64(),
                        writer.id(),
                    ));
                }
                let written = buf.len() as f64;
                budget -= written;
                writer.write(buf.as_bytes());
            }
            message = control.recv() => match message {
                Some(RunControl::Input(data)) if echo => writer.write(&data),
                Some(_) => {}
                None => break,
            }
        }
    }
    tracing::debug!(run = %writer.id(), bytes = writer.total(), "synthetic run stopped");
}
