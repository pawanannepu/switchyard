//! `dev.*` methods, compiled only with the `devtools` feature.
//!
//! `dev.agent_run` (spec 0003, AC-4) creates fixture rows the way spec 0002
//! AC-15 describes (one fixture project, one task per call, one workspace per
//! run) and launches each run through the real agent path: the adapter, a
//! real PTY, the run hub. With the fake agent (`agent_id: "fake"`) its
//! script decides what each run does; a `flood` script drives the load check.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use sy_adapter_api::PtySize;
use sy_core::agents::{StartError, StartRun, StoreWrites};
use sy_core::ids::WorkspaceId;
use sy_core::model::{Actor, InitialWorkspaceState, NewProject, NewTask, NewWorkspace, Project};
use sy_core::ports::{Store, StoreError};
use sy_protocol::{
    AgentRunParams, AgentRunResult, ErrorCode, RpcError, TaskId, WorkspaceId as WireWorkspaceId,
};

use crate::dispatch;
use crate::engine::Engine;
use crate::wire;

/// Most runs one `dev.agent_run` call starts.
const MAX_RUNS: u32 = 32;
const DEFAULT_SIZE: PtySize = PtySize {
    cols: 120,
    rows: 40,
};
const FIXTURE_DIR: &str = "dev-fixture";

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

/// A fixture workspace, with its worktree as a plain `0700` directory
/// (feature 8 makes it a git worktree).
fn fixture_workspace(
    engine: &Engine,
    agent_id: &str,
    options: &str,
) -> std::io::Result<NewWorkspace> {
    let id = WorkspaceId::new();
    let fixture = engine.data_dir.join(FIXTURE_DIR);
    let worktree = fixture.join(id.as_str());
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&fixture)?;
    DirBuilder::new().mode(0o700).create(&worktree)?;
    Ok(NewWorkspace {
        branch: format!("dev/{id}"),
        id,
        agent_id: agent_id.to_owned(),
        agent_options: options.to_owned(),
        worktree_path: path_text(&worktree),
        base_commit: String::new(),
        state: InitialWorkspaceState::Ready,
    })
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn internal(message: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::InternalError, message.to_string())
}

pub async fn agent_run(
    engine: &Arc<Engine>,
    p: AgentRunParams,
) -> Result<AgentRunResult, RpcError> {
    let count = p.count.unwrap_or(1);
    if count == 0 || count > MAX_RUNS {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!("count must be 1 to {MAX_RUNS}"),
        ));
    }
    // Both checked before any row exists.
    let adapter = engine.agents.adapters().get(&p.agent_id).ok_or_else(|| {
        RpcError::new(
            ErrorCode::AgentNotFound,
            format!("no agent adapter {}", p.agent_id),
        )
    })?;
    if !adapter.capabilities().launch {
        return Err(RpcError::new(
            ErrorCode::AgentLaunchUnsupported,
            format!("{} can't launch runs yet", adapter.display_name()),
        ));
    }
    let options = p.options.unwrap_or_else(|| json!({})).to_string();
    let size = PtySize {
        cols: p.cols.unwrap_or(DEFAULT_SIZE.cols),
        rows: p.rows.unwrap_or(DEFAULT_SIZE.rows),
    };
    let store = dispatch::store(engine)?;
    let project = fixture_project(engine, store.as_ref())
        .await
        .map_err(|e| dispatch::store_error(&e))?;
    let workspaces = (0..count)
        .map(|_| fixture_workspace(engine, &p.agent_id, &options))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| internal(format!("creating a fixture worktree: {e}")))?;
    let (task, workspaces) = store
        .create_task(
            Actor::User,
            NewTask {
                project_id: project.id,
                title: format!("{} runs", adapter.display_name()),
                prompt: p.prompt.clone().unwrap_or_default(),
                base_ref: "main".into(),
                parent_task_id: None,
                workspaces,
            },
        )
        .await
        .map_err(|e| dispatch::store_error(&e))?
        .value;

    let writes = Arc::new(StoreWrites(Arc::clone(&store)));
    let mut run_ids = Vec::with_capacity(workspaces.len());
    for workspace in &workspaces {
        // Each run starts (or fails) on its own; rows already made stay.
        let run = engine
            .agents
            .start(
                writes.clone(),
                StartRun {
                    workspace: workspace.clone(),
                    prompt: p.prompt.clone().unwrap_or_default(),
                    size,
                },
            )
            .await
            .map_err(|err| match err {
                StartError::UnknownAgent(id) => {
                    RpcError::new(ErrorCode::AgentNotFound, format!("no agent adapter {id}"))
                }
                StartError::Store(e) => dispatch::store_error(&e),
                StartError::Hub(e) => internal(e),
            })?;
        run_ids.push(wire::run_id(&run));
    }
    tracing::info!(count, agent_id = %p.agent_id, task = %task.id, "dev agent runs started");
    Ok(AgentRunResult {
        task_id: TaskId(task.id.to_string()),
        workspace_ids: workspaces
            .iter()
            .map(|w| WireWorkspaceId(w.id.to_string()))
            .collect(),
        run_ids,
    })
}
