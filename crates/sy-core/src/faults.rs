//! Test doubles that make storage fail on demand (spec 0002, *Disk full*
//! scenario). Compiled only with the `faults` feature: the daemon's
//! `devtools` build wires them in when `SWITCHYARD_DEV_FAULTS` is set, so the
//! socket tests can fill the "disk" of a real engine.
//!
//! [`FaultyStore`] fails every write with [`StoreError::Full`] while its
//! switch is on; reads pass through. [`FaultyRunLog`] makes each append
//! write half its bytes and then fail with [`RunLogError::Full`] (a partial
//! write the hub must cut back); everything else passes through.

use std::sync::Arc;

use crate::ids::{ApprovalId, ProjectId, RunId, TaskId, WorkspaceId};
use crate::model::{
    Actor, AdapterEvent, Approval, ApprovalDecision, JsonValue, LogState, NewApproval, NewProject,
    NewRun, NewTask, NewWorkspace, Project, ProjectSettings, Run, RunTransition, Task, Workspace,
    WorkspaceChange, WorkspaceOutcome,
};
use crate::ports::{
    ApprovalFilter, Committed, EventFilter, EventPage, ListQuery, LogReconcile, Page, RunFilter,
    RunLog, RunLogError, RunProcess, Store, StoreError, TrackedProcess, TrimCandidate,
};

/// Whether a fault is on now. Checked on every call.
pub type Switch = Arc<dyn Fn() -> bool + Send + Sync>;

/// A [`Store`] whose writes fail with `Full` while `full()` is true.
pub struct FaultyStore {
    inner: Arc<dyn Store>,
    full: Switch,
}

impl FaultyStore {
    /// Wrap `inner`.
    pub fn new(inner: Arc<dyn Store>, full: Switch) -> Self {
        Self { inner, full }
    }

    fn check(&self) -> Result<(), StoreError> {
        if (self.full)() {
            Err(StoreError::Full)
        } else {
            Ok(())
        }
    }
}

/// A [`RunLog`] whose appends fail part way with `Full` while `full()` is true.
pub struct FaultyRunLog {
    inner: Arc<dyn RunLog>,
    full: Switch,
}

impl FaultyRunLog {
    /// Wrap `inner`.
    pub fn new(inner: Arc<dyn RunLog>, full: Switch) -> Self {
        Self { inner, full }
    }
}

impl RunLog for FaultyRunLog {
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
        if (self.full)() {
            let half = &bytes[..bytes.len() / 2];
            if !half.is_empty() {
                self.inner.append(run, offset, half)?;
            }
            return Err(RunLogError::Full);
        }
        self.inner.append(run, offset, bytes)
    }
    fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError> {
        self.inner.truncate(run, len)
    }
    fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError> {
        self.inner.read(run, offset, max)
    }
    fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
        self.inner.len(run)
    }
    fn delete(&self, run: &RunId) -> Result<(), RunLogError> {
        self.inner.delete(run)
    }
    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
        self.inner.list()
    }
}

#[async_trait::async_trait]
impl Store for FaultyStore {
    async fn create_project(
        &self,
        actor: Actor,
        new: NewProject,
    ) -> Result<Committed<Project>, StoreError> {
        self.check()?;
        self.inner.create_project(actor, new).await
    }
    async fn find_project_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<Project>, StoreError> {
        self.inner.find_project_by_repo_path(repo_path).await
    }
    async fn create_task(
        &self,
        actor: Actor,
        new: NewTask,
    ) -> Result<Committed<(Task, Vec<Workspace>)>, StoreError> {
        self.check()?;
        self.inner.create_task(actor, new).await
    }
    async fn add_workspace(
        &self,
        actor: Actor,
        task: &TaskId,
        new: NewWorkspace,
    ) -> Result<Committed<Workspace>, StoreError> {
        self.check()?;
        self.inner.add_workspace(actor, task, new).await
    }
    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Committed<Run>, StoreError> {
        self.check()?;
        self.inner.create_run(actor, new).await
    }
    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError> {
        self.inner.get_run(run).await
    }
    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Committed<Run>, StoreError> {
        self.check()?;
        self.inner.transition_run(actor, run, change).await
    }
    async fn mark_live_runs_lost(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Run>>, StoreError> {
        self.check()?;
        self.inner.mark_live_runs_lost(actor, at).await
    }
    async fn discard_preparing_workspaces(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Workspace>>, StoreError> {
        self.check()?;
        self.inner.discard_preparing_workspaces(actor, at).await
    }
    async fn reconcile_log_bytes(
        &self,
        files: Vec<(RunId, u64)>,
    ) -> Result<LogReconcile, StoreError> {
        self.check()?;
        self.inner.reconcile_log_bytes(files).await
    }
    async fn events_since(
        &self,
        cursor: i64,
        limit: u32,
        filter: EventFilter,
    ) -> Result<EventPage, StoreError> {
        self.inner.events_since(cursor, limit, filter).await
    }
    async fn get_project(&self, project: &ProjectId) -> Result<Project, StoreError> {
        self.inner.get_project(project).await
    }
    async fn list_projects(
        &self,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Project>, StoreError> {
        self.inner.list_projects(include_archived, query).await
    }
    async fn archive_project(
        &self,
        actor: Actor,
        project: &ProjectId,
    ) -> Result<Committed<Project>, StoreError> {
        self.check()?;
        self.inner.archive_project(actor, project).await
    }
    async fn delete_project(&self, project: &ProjectId) -> Result<Vec<RunId>, StoreError> {
        self.check()?;
        self.inner.delete_project(project).await
    }
    async fn set_project_setting(
        &self,
        actor: Actor,
        project: &ProjectId,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<i64>, StoreError> {
        self.check()?;
        self.inner
            .set_project_setting(actor, project, key, value)
            .await
    }
    async fn project_settings(&self, project: &ProjectId) -> Result<ProjectSettings, StoreError> {
        self.inner.project_settings(project).await
    }
    async fn set_setting(
        &self,
        actor: Actor,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<()>, StoreError> {
        self.check()?;
        self.inner.set_setting(actor, key, value).await
    }
    async fn get_setting(&self, key: &str) -> Result<Option<JsonValue>, StoreError> {
        self.inner.get_setting(key).await
    }
    async fn get_task(&self, task: &TaskId) -> Result<Task, StoreError> {
        self.inner.get_task(task).await
    }
    async fn list_tasks(
        &self,
        project: &ProjectId,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Task>, StoreError> {
        self.inner
            .list_tasks(project, include_archived, query)
            .await
    }
    async fn archive_task(
        &self,
        actor: Actor,
        task: &TaskId,
    ) -> Result<Committed<Task>, StoreError> {
        self.check()?;
        self.inner.archive_task(actor, task).await
    }
    async fn delete_task(&self, task: &TaskId) -> Result<Vec<RunId>, StoreError> {
        self.check()?;
        self.inner.delete_task(task).await
    }
    async fn get_workspace(&self, workspace: &WorkspaceId) -> Result<Workspace, StoreError> {
        self.inner.get_workspace(workspace).await
    }
    async fn list_workspaces(
        &self,
        task: &TaskId,
        query: ListQuery,
    ) -> Result<Page<Workspace>, StoreError> {
        self.inner.list_workspaces(task, query).await
    }
    async fn set_workspace_state(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        change: WorkspaceChange,
    ) -> Result<Committed<Workspace>, StoreError> {
        self.check()?;
        self.inner
            .set_workspace_state(actor, workspace, change)
            .await
    }
    async fn set_workspace_outcome(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        outcome: WorkspaceOutcome,
    ) -> Result<Committed<Workspace>, StoreError> {
        self.check()?;
        self.inner
            .set_workspace_outcome(actor, workspace, outcome)
            .await
    }
    async fn list_runs(
        &self,
        filter: RunFilter,
        query: ListQuery,
    ) -> Result<Page<Run>, StoreError> {
        self.inner.list_runs(filter, query).await
    }
    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError> {
        self.check()?;
        self.inner.set_run_process(run, process).await
    }
    async fn track_processes(
        &self,
        run: &RunId,
        rows: &[TrackedProcess],
        main: bool,
    ) -> Result<(), StoreError> {
        self.check()?;
        self.inner.track_processes(run, rows, main).await
    }
    async fn untrack_processes(&self, run: &RunId, keys: &[(i64, i64)]) -> Result<(), StoreError> {
        self.check()?;
        self.inner.untrack_processes(run, keys).await
    }
    async fn list_tracked_processes(&self) -> Result<Vec<(RunId, TrackedProcess)>, StoreError> {
        self.inner.list_tracked_processes().await
    }
    async fn set_agent_session_ref(
        &self,
        run: &RunId,
        session_ref: &str,
    ) -> Result<Run, StoreError> {
        self.check()?;
        self.inner.set_agent_session_ref(run, session_ref).await
    }
    async fn set_log_state(&self, run: &RunId, state: LogState) -> Result<Run, StoreError> {
        self.check()?;
        self.inner.set_log_state(run, state).await
    }
    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<Committed<()>, StoreError> {
        self.check()?;
        self.inner.append_event(actor, actor_ref, run, event).await
    }
    async fn create_approval(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        new: NewApproval,
    ) -> Result<Committed<Approval>, StoreError> {
        self.check()?;
        self.inner.create_approval(actor, actor_ref, new).await
    }
    async fn decide_approval(
        &self,
        actor: Actor,
        approval: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Committed<Approval>, StoreError> {
        self.check()?;
        self.inner.decide_approval(actor, approval, decision).await
    }
    async fn get_approval(&self, approval: &ApprovalId) -> Result<Approval, StoreError> {
        self.inner.get_approval(approval).await
    }
    async fn list_approvals(
        &self,
        filter: ApprovalFilter,
        query: ListQuery,
    ) -> Result<Page<Approval>, StoreError> {
        self.inner.list_approvals(filter, query).await
    }
    async fn cancel_pending_approvals(
        &self,
        actor: Actor,
        run: &RunId,
        message: Option<String>,
    ) -> Result<Committed<Vec<Approval>>, StoreError> {
        self.check()?;
        self.inner
            .cancel_pending_approvals(actor, run, message)
            .await
    }
    async fn expire_pending_approvals(&self) -> Result<Committed<Vec<Approval>>, StoreError> {
        self.check()?;
        self.inner.expire_pending_approvals().await
    }
    async fn log_total_ended(&self) -> Result<u64, StoreError> {
        self.inner.log_total_ended().await
    }
    async fn log_trim_candidates(&self) -> Result<Vec<TrimCandidate>, StoreError> {
        self.inner.log_trim_candidates().await
    }
    async fn mark_log_trimmed(
        &self,
        actor: Actor,
        run: &RunId,
    ) -> Result<Committed<u64>, StoreError> {
        self.check()?;
        self.inner.mark_log_trimmed(actor, run).await
    }
}
