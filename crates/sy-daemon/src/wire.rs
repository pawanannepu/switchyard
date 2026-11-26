//! Domain values to protocol DTOs (and the ids back). `sy-core` never sees
//! `sy-protocol` types, and the protocol never sees core internals.

use sy_core::ids::{ApprovalId, ProjectId, RunId, TaskId, WorkspaceId};
use sy_core::model::{
    Actor, Approval, ApprovalKind, ApprovalStatus, DecidedBy, Event, EventKind, FileChange,
    JsonValue, MessageRole, RunKind, RunStatus, WorkspaceOutcome, WorkspaceState,
};
use sy_core::ports::EventFilter;
use sy_protocol::{ErrorCode, EventPayload, EventSinceParams, RpcError};

pub fn run_id(id: &RunId) -> sy_protocol::RunId {
    sy_protocol::RunId(id.to_string())
}

fn workspace_id(id: &WorkspaceId) -> sy_protocol::WorkspaceId {
    sy_protocol::WorkspaceId(id.to_string())
}

fn status_source(source: sy_adapter_api::StatusSource) -> sy_protocol::StatusSource {
    match source {
        sy_adapter_api::StatusSource::Hooks => sy_protocol::StatusSource::Hooks,
        sy_adapter_api::StatusSource::Patterns => sy_protocol::StatusSource::Patterns,
        sy_adapter_api::StatusSource::ExitOnly => sy_protocol::StatusSource::ExitOnly,
    }
}

fn signed_in(signed_in: sy_adapter_api::SignedIn) -> sy_protocol::SignedIn {
    match signed_in {
        sy_adapter_api::SignedIn::Yes => sy_protocol::SignedIn::Yes,
        sy_adapter_api::SignedIn::No => sy_protocol::SignedIn::No,
        sy_adapter_api::SignedIn::Unknown => sy_protocol::SignedIn::Unknown,
    }
}

/// An adapter and its last detection in their wire shape.
pub fn agent(a: &sy_core::agents::detect::AgentInfo) -> sy_protocol::AgentInfo {
    let c = a.capabilities;
    sy_protocol::AgentInfo {
        id: a.id.clone(),
        display_name: a.display_name.clone(),
        capabilities: sy_protocol::AgentCapabilities {
            launch: c.launch,
            approvals: c.approvals,
            resume: c.resume,
            events: c.events,
            status_source: status_source(c.status_source),
        },
        detection: a.detection.as_ref().map(|d| sy_protocol::AgentDetection {
            installed: d.detection.installed,
            signed_in: signed_in(d.detection.signed_in),
            version: d.detection.version.clone(),
            path: d.path.clone(),
            hint: d.detection.hint.clone(),
            checked_at: d.checked_at,
        }),
    }
}

/// A login environment capture's state.
pub fn login_env(snapshot: &sy_core::agents::ports::LoginEnvSnapshot) -> sy_protocol::LoginEnvInfo {
    sy_protocol::LoginEnvInfo {
        status: if snapshot.fallback.is_some() {
            sy_protocol::LoginEnvStatus::Fallback
        } else {
            sy_protocol::LoginEnvStatus::Ok
        },
        message: snapshot.fallback.clone(),
    }
}

pub fn approval_id(id: &ApprovalId) -> sy_protocol::ApprovalId {
    sy_protocol::ApprovalId(id.to_string())
}

pub fn approval_kind(kind: ApprovalKind) -> sy_protocol::ApprovalKind {
    match kind {
        ApprovalKind::Command => sy_protocol::ApprovalKind::Command,
        ApprovalKind::FileEdit => sy_protocol::ApprovalKind::FileEdit,
        ApprovalKind::Fetch => sy_protocol::ApprovalKind::Fetch,
        ApprovalKind::McpTool => sy_protocol::ApprovalKind::McpTool,
        ApprovalKind::Other => sy_protocol::ApprovalKind::Other,
    }
}

pub fn approval_status(status: ApprovalStatus) -> sy_protocol::ApprovalStatus {
    match status {
        ApprovalStatus::Pending => sy_protocol::ApprovalStatus::Pending,
        ApprovalStatus::Approved => sy_protocol::ApprovalStatus::Approved,
        ApprovalStatus::Denied => sy_protocol::ApprovalStatus::Denied,
        ApprovalStatus::Expired => sy_protocol::ApprovalStatus::Expired,
        ApprovalStatus::Cancelled => sy_protocol::ApprovalStatus::Cancelled,
    }
}

/// The wire status as the domain's (for `approval.list`'s filter).
pub fn core_approval_status(status: sy_protocol::ApprovalStatus) -> ApprovalStatus {
    match status {
        sy_protocol::ApprovalStatus::Pending => ApprovalStatus::Pending,
        sy_protocol::ApprovalStatus::Approved => ApprovalStatus::Approved,
        sy_protocol::ApprovalStatus::Denied => ApprovalStatus::Denied,
        sy_protocol::ApprovalStatus::Expired => ApprovalStatus::Expired,
        sy_protocol::ApprovalStatus::Cancelled => ApprovalStatus::Cancelled,
    }
}

fn decided_by(by: DecidedBy) -> sy_protocol::DecidedBy {
    match by {
        DecidedBy::User => sy_protocol::DecidedBy::User,
        DecidedBy::Policy => sy_protocol::DecidedBy::Policy,
        DecidedBy::Agent => sy_protocol::DecidedBy::Agent,
    }
}

/// An approval in its wire shape.
pub fn approval(a: &Approval) -> sy_protocol::Approval {
    sy_protocol::Approval {
        id: approval_id(&a.id),
        run_id: run_id(&a.run_id),
        workspace_id: workspace_id(&a.workspace_id),
        kind: approval_kind(a.kind),
        summary: a.summary.clone(),
        detail: a.detail.clone(),
        status: approval_status(a.status),
        requested_at: a.requested_at,
        decided_at: a.decided_at,
        decided_by: a.decided_by.map(decided_by),
        decided_by_ref: a.decided_by_ref.clone(),
        message: a.message.clone(),
    }
}

fn task_id(id: &TaskId) -> sy_protocol::TaskId {
    sy_protocol::TaskId(id.to_string())
}

fn project_id(id: &ProjectId) -> sy_protocol::ProjectId {
    sy_protocol::ProjectId(id.to_string())
}

pub fn status(status: RunStatus) -> sy_protocol::RunStatus {
    match status {
        RunStatus::Queued => sy_protocol::RunStatus::Queued,
        RunStatus::Starting => sy_protocol::RunStatus::Starting,
        RunStatus::Running => sy_protocol::RunStatus::Running,
        RunStatus::WaitingInput => sy_protocol::RunStatus::WaitingInput,
        RunStatus::Finished => sy_protocol::RunStatus::Finished,
        RunStatus::Failed => sy_protocol::RunStatus::Failed,
        RunStatus::Stopped => sy_protocol::RunStatus::Stopped,
        RunStatus::Lost => sy_protocol::RunStatus::Lost,
    }
}

fn actor(actor: Actor) -> sy_protocol::Actor {
    match actor {
        Actor::User => sy_protocol::Actor::User,
        Actor::Engine => sy_protocol::Actor::Engine,
        Actor::Policy => sy_protocol::Actor::Policy,
        Actor::Agent => sy_protocol::Actor::Agent,
    }
}

fn run_kind(kind: RunKind) -> sy_protocol::RunKind {
    match kind {
        RunKind::Agent => sy_protocol::RunKind::Agent,
        RunKind::Setup => sy_protocol::RunKind::Setup,
        RunKind::RunScript => sy_protocol::RunKind::RunScript,
    }
}

fn workspace_state(state: WorkspaceState) -> sy_protocol::WorkspaceState {
    match state {
        WorkspaceState::Preparing => sy_protocol::WorkspaceState::Preparing,
        WorkspaceState::Ready => sy_protocol::WorkspaceState::Ready,
        WorkspaceState::Merged => sy_protocol::WorkspaceState::Merged,
        WorkspaceState::Discarded => sy_protocol::WorkspaceState::Discarded,
    }
}

fn outcome(outcome: WorkspaceOutcome) -> sy_protocol::WorkspaceOutcome {
    match outcome {
        WorkspaceOutcome::Winner => sy_protocol::WorkspaceOutcome::Winner,
        WorkspaceOutcome::NotChosen => sy_protocol::WorkspaceOutcome::NotChosen,
    }
}

fn role(role: MessageRole) -> sy_protocol::MessageRole {
    match role {
        MessageRole::User => sy_protocol::MessageRole::User,
        MessageRole::Agent => sy_protocol::MessageRole::Agent,
        MessageRole::System => sy_protocol::MessageRole::System,
    }
}

fn file_change(change: FileChange) -> sy_protocol::FileChange {
    match change {
        FileChange::Added => sy_protocol::FileChange::Added,
        FileChange::Modified => sy_protocol::FileChange::Modified,
        FileChange::Deleted => sy_protocol::FileChange::Deleted,
        FileChange::Renamed => sy_protocol::FileChange::Renamed,
    }
}

/// A setting value in its wire form. `JsonValue` is plain serde data, so this
/// can't fail; `null` stands in if it ever did.
fn json(value: &JsonValue) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn payload(kind: &EventKind) -> EventPayload {
    match kind {
        EventKind::ProjectCreated {
            name,
            repo_path,
            base_branch,
        } => EventPayload::ProjectCreated {
            name: name.clone(),
            repo_path: repo_path.clone(),
            base_branch: base_branch.clone(),
        },
        EventKind::ProjectArchived {} => EventPayload::ProjectArchived {},
        EventKind::ProjectSettingChanged {
            key,
            value,
            settings_revision,
        } => EventPayload::ProjectSettingChanged {
            key: key.clone(),
            value: json(value),
            settings_revision: *settings_revision,
        },
        EventKind::SettingChanged { key, value } => EventPayload::SettingChanged {
            key: key.clone(),
            value: json(value),
        },
        EventKind::TaskCreated {
            title,
            base_ref,
            parent_task_id,
        } => EventPayload::TaskCreated {
            title: title.clone(),
            base_ref: base_ref.clone(),
            parent_task_id: parent_task_id.as_ref().map(task_id),
        },
        EventKind::TaskArchived {} => EventPayload::TaskArchived {},
        EventKind::WorkspaceCreated {
            agent_id,
            branch,
            worktree_path,
            state,
        } => EventPayload::WorkspaceCreated {
            agent_id: agent_id.clone(),
            branch: branch.clone(),
            worktree_path: worktree_path.clone(),
            state: workspace_state(*state),
        },
        EventKind::WorkspaceStateChanged { from, to } => EventPayload::WorkspaceStateChanged {
            from: workspace_state(*from),
            to: workspace_state(*to),
        },
        EventKind::WorkspaceOutcomeSet { outcome: o } => EventPayload::WorkspaceOutcomeSet {
            outcome: outcome(*o),
        },
        EventKind::RunCreated {
            kind,
            status: s,
            resumes_run_id,
        } => EventPayload::RunCreated {
            kind: run_kind(*kind),
            status: status(*s),
            resumes_run_id: resumes_run_id.as_ref().map(run_id),
        },
        EventKind::RunStatusChanged {
            run_id: run,
            workspace_id: ws,
            from,
            status: to,
            end_reason,
            exit_code,
            at,
        } => EventPayload::RunStatusChanged {
            run_id: run_id(run),
            workspace_id: workspace_id(ws),
            from: status(*from),
            status: status(*to),
            end_reason: end_reason.map(|r| r.as_str().to_owned()),
            exit_code: *exit_code,
            at: *at,
        },
        EventKind::RunLogTrimmed { bytes } => EventPayload::RunLogTrimmed { bytes: *bytes },
        EventKind::Message { role: r, text } => EventPayload::Message {
            role: role(*r),
            text: text.clone(),
        },
        EventKind::CommandRun { argv, exit_code } => EventPayload::CommandRun {
            argv: argv.clone(),
            exit_code: *exit_code,
        },
        EventKind::FileChanged {
            path,
            change,
            from_path,
        } => EventPayload::FileChanged {
            path: path.clone(),
            change: file_change(*change),
            from_path: from_path.clone(),
        },
        EventKind::ApprovalRequested {
            approval_id: id,
            kind,
            summary,
        } => EventPayload::ApprovalRequested {
            approval_id: approval_id(id),
            kind: approval_kind(*kind),
            summary: summary.clone(),
        },
        EventKind::ApprovalDecided {
            approval_id: id,
            status,
            decided_by: by,
            decided_by_ref,
            message,
        } => EventPayload::ApprovalDecided {
            approval_id: approval_id(id),
            status: approval_status(*status),
            decided_by: by.map(decided_by),
            decided_by_ref: decided_by_ref.clone(),
            message: message.clone(),
        },
    }
}

/// A committed journal event in its wire shape.
pub fn event(event: &Event) -> sy_protocol::Event {
    let s = &event.scope;
    sy_protocol::Event {
        id: event.id,
        at: event.at,
        actor: actor(event.actor),
        actor_ref: event.actor_ref.clone(),
        project_id: s.project_id.as_ref().map(project_id),
        task_id: s.task_id.as_ref().map(task_id),
        workspace_id: s.workspace_id.as_ref().map(workspace_id),
        run_id: s.run_id.as_ref().map(run_id),
        payload: payload(&event.kind),
    }
}

fn parse<T, E>(
    field: &str,
    text: Option<&String>,
    parse: fn(&str) -> Result<T, E>,
) -> Result<Option<T>, RpcError> {
    text.map(|t| {
        parse(t).map_err(|_| {
            RpcError::new(
                ErrorCode::InvalidParams,
                format!("{field}: {t:?} is not a valid id"),
            )
        })
    })
    .transpose()
}

/// The store filter for `event.since`. A filter id that isn't a valid id is `InvalidParams`.
pub fn event_filter(p: &EventSinceParams) -> Result<EventFilter, RpcError> {
    Ok(EventFilter {
        project_id: parse(
            "project_id",
            p.project_id.as_ref().map(|id| &id.0),
            ProjectId::parse,
        )?,
        task_id: parse("task_id", p.task_id.as_ref().map(|id| &id.0), TaskId::parse)?,
        workspace_id: parse(
            "workspace_id",
            p.workspace_id.as_ref().map(|id| &id.0),
            WorkspaceId::parse,
        )?,
        run_id: parse("run_id", p.run_id.as_ref().map(|id| &id.0), RunId::parse)?,
    })
}

#[cfg(test)]
mod tests {
    use sy_core::model::{EndReason, EventScope};

    use super::*;

    #[test]
    fn run_status_changed_payload_should_match_the_notification_fields() {
        let run = RunId::new();
        let ws = WorkspaceId::new();
        let core = Event {
            id: 7,
            at: 100,
            actor: Actor::Engine,
            actor_ref: None,
            scope: EventScope {
                run_id: Some(run.clone()),
                workspace_id: Some(ws.clone()),
                ..EventScope::default()
            },
            kind: EventKind::RunStatusChanged {
                run_id: run.clone(),
                workspace_id: ws.clone(),
                from: RunStatus::Running,
                status: RunStatus::Lost,
                end_reason: Some(EndReason::EngineRestart),
                exit_code: None,
                at: 100,
            },
        };
        let wire = serde_json::to_value(event(&core)).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "id": 7, "at": 100, "actor": "engine",
                "workspace_id": ws.as_str(), "run_id": run.as_str(),
                "kind": "run_status_changed",
                "payload": {
                    "run_id": run.as_str(), "workspace_id": ws.as_str(),
                    "from": "running", "status": "lost",
                    "end_reason": "engine_restart", "at": 100
                }
            })
        );
    }

    #[test]
    fn wire_payload_should_serialise_exactly_like_the_stored_payload() {
        // The journal stores `EventKind`'s JSON; the wire must say the same thing.
        let kinds = [
            EventKind::TaskArchived {},
            EventKind::Message {
                role: MessageRole::Agent,
                text: "hi".into(),
            },
            EventKind::FileChanged {
                path: "b".into(),
                change: FileChange::Renamed,
                from_path: Some("a".into()),
            },
            EventKind::WorkspaceStateChanged {
                from: WorkspaceState::Preparing,
                to: WorkspaceState::Discarded,
            },
        ];
        for kind in kinds {
            assert_eq!(
                serde_json::to_value(payload(&kind)).unwrap(),
                serde_json::to_value(&kind).unwrap()
            );
        }
    }

    #[test]
    fn bad_filter_id_should_be_invalid_params() {
        let p = EventSinceParams {
            cursor: 0,
            limit: None,
            project_id: None,
            task_id: Some(sy_protocol::TaskId("run_nope".into())),
            workspace_id: None,
            run_id: None,
        };
        assert_eq!(
            event_filter(&p).map_err(|e| e.code),
            Err(ErrorCode::InvalidParams.code())
        );
    }
}
