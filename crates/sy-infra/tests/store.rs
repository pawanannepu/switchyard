#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! `SqliteStore` against real SQLite files in a temp dir (spec 0002, AC-1,
//! AC-6, AC-7, AC-8, AC-9, AC-10, AC-14, AC-16 and the rebuild rule). Startup recovery
//! runs sy-core's use case over the real store and real log files.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use rusqlite::Connection;
use sy_core::ids::{ProjectId, RunId, TaskId, WorkspaceId};
use sy_core::model::{
    Actor, AdapterEvent, EndReason, Event, EventKind, InitialWorkspaceState,
    InvalidWorkspaceTransition, JsonValue, LogState, NewProject, NewRun, NewTask, NewWorkspace,
    NotAComparison, RunEnd, RunKind, RunStatus, RunTransition, STORAGE_PROBE_KEY, Workspace,
    WorkspaceChange, WorkspaceOutcome, WorkspaceState,
};
use sy_core::ports::{
    Clock, EventFilter, ListQuery, RunFilter, RunLog, RunLogError, RunProcess, Store, StoreError,
};
use sy_core::recovery::{Recovered, recover_at_startup};
use sy_core::retention::{
    self, DEFAULT_LOG_CAP_BYTES, LOG_CAP_KEY, RetentionError, enforce_log_cap,
};
use sy_infra::storage::{
    FileRunLog, MIGRATIONS, Migration, SqliteStore, StoragePaths, UNIQUE_INDEXES,
    sweep_backup_temps,
};
use tokio::sync::mpsc;

/// Ticks one millisecond per read, so every `*_at` is distinct and known.
struct TickClock(AtomicI64);

impl Clock for TickClock {
    fn now_ms(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: SqliteStore,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Fixture {
    fn new() -> Self {
        Self::with(MIGRATIONS)
    }

    fn with(migrations: &[Migration]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = StoragePaths::new(dir.path());
        paths.ensure_dirs().unwrap();
        let (tx, events) = mpsc::unbounded_channel();
        let store = SqliteStore::open_with(
            &paths.db,
            migrations,
            Arc::new(TickClock(AtomicI64::new(1000))),
            tx,
        )
        .unwrap();
        Self { dir, store, events }
    }

    fn db(&self) -> std::path::PathBuf {
        self.dir.path().join("switchyard.db")
    }

    /// A second connection to the same file, for checks the port doesn't expose.
    fn raw(&self) -> Connection {
        Connection::open(self.db()).unwrap()
    }

    async fn project(&self, repo: &str) -> ProjectId {
        self.store
            .create_project(
                Actor::User,
                NewProject {
                    name: "p".into(),
                    repo_path: repo.into(),
                    base_branch: "main".into(),
                },
            )
            .await
            .unwrap()
            .value
            .id
    }

    async fn task(
        &self,
        project: &ProjectId,
        workspaces: Vec<NewWorkspace>,
    ) -> Result<(TaskId, Vec<Workspace>), StoreError> {
        let committed = self
            .store
            .create_task(
                Actor::User,
                NewTask {
                    project_id: project.clone(),
                    title: "t".into(),
                    prompt: "do it".into(),
                    base_ref: "main".into(),
                    parent_task_id: None,
                    workspaces,
                },
            )
            .await?;
        Ok((committed.value.0.id, committed.value.1))
    }

    async fn run(&self, ws: &WorkspaceId, kind: RunKind) -> Result<RunId, StoreError> {
        let id = RunId::new();
        self.store
            .create_run(
                Actor::User,
                NewRun {
                    id: id.clone(),
                    workspace_id: ws.clone(),
                    kind,
                    input: None,
                    resumes_run_id: None,
                },
            )
            .await?;
        Ok(id)
    }

    /// Move a run through `statuses` in order (terminal ones end with [`STOP`]).
    async fn walk(&self, run: &RunId, statuses: &[RunStatus]) {
        for status in statuses {
            let change = if status.is_terminal() {
                RunTransition::end(*status, STOP)
            } else {
                RunTransition::to(*status)
            };
            self.store
                .transition_run(Actor::User, run, change)
                .await
                .unwrap();
        }
    }

    /// The run logs in this fixture's `runs/`.
    fn run_log(&self) -> Arc<FileRunLog> {
        Arc::new(FileRunLog::new(&self.dir.path().join("runs")))
    }

    /// Drain the broadcaster channel.
    fn broadcast(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }
}

fn ws(branch: &str, path: &str) -> NewWorkspace {
    NewWorkspace {
        id: WorkspaceId::new(),
        agent_id: "synthetic".into(),
        agent_options: "{}".into(),
        branch: branch.into(),
        worktree_path: path.into(),
        base_commit: String::new(),
        state: InitialWorkspaceState::Ready,
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

const STOP: RunEnd = RunEnd {
    reason: EndReason::UserStop,
    exit_code: None,
    log_bytes: 12,
};

// ---- AC-1: open, pragmas, schema, modes ----

#[tokio::test]
async fn open_should_set_the_pragmas_and_reach_the_newest_version() {
    let f = Fixture::new();
    f.store.get_run(&RunId::new()).await.unwrap_err(); // the store thread is up
    let raw = f.raw();
    let journal: String = raw
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    let version: i64 = raw
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        (journal.as_str(), version),
        ("wal", MIGRATIONS.len() as i64)
    );
}

#[tokio::test]
async fn spine_tables_should_exist_and_be_strict() {
    let f = Fixture::new();
    let raw = f.raw();
    let mut st = raw
        .prepare("SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let tables: Vec<(String, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "event",
            "project",
            "project_setting",
            "run",
            "setting",
            "task",
            "workspace"
        ]
    );
    assert!(tables.iter().all(|(_, strict)| *strict == 1));
}

#[tokio::test]
async fn every_mapped_unique_index_should_exist_with_its_columns() {
    let f = Fixture::new();
    let raw = f.raw();
    for (index, columns) in UNIQUE_INDEXES {
        let (table, _) = columns.split_once('.').unwrap();
        let unique: i64 = raw
            .query_row(
                "SELECT \"unique\" FROM pragma_index_list(?1) WHERE name = ?2",
                [table, index],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| panic!("{index} is missing"));
        let mut st = raw
            .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
            .unwrap();
        let cols: Vec<String> = st
            .query_map([index], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|c| format!("{table}.{}", c.unwrap()))
            .collect();
        assert_eq!(
            (unique, cols.join(", ")),
            (1, (*columns).to_owned()),
            "{index}"
        );
    }
}

#[tokio::test]
async fn database_files_should_be_0600_and_dirs_0700() {
    let f = Fixture::new();
    f.project("/repo").await; // forces the WAL and SHM files into being
    let dir = f.dir.path();
    let files =
        ["switchyard.db", "switchyard.db-wal", "switchyard.db-shm"].map(|n| mode(&dir.join(n)));
    let dirs = [dir.to_path_buf(), dir.join("runs"), dir.join("backups")].map(|d| mode(&d));
    assert_eq!((files, dirs), ([0o600; 3], [0o700; 3]));
}

#[tokio::test]
async fn ensure_dirs_should_narrow_a_wide_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    StoragePaths::new(dir.path()).ensure_dirs().unwrap();
    assert_eq!(mode(dir.path()), 0o700);
}

// ---- AC-7: invariants ----

#[tokio::test]
async fn duplicate_repo_path_should_be_project_exists() {
    let f = Fixture::new();
    f.project("/repo").await;
    let err = f
        .store
        .create_project(
            Actor::User,
            NewProject {
                name: "again".into(),
                repo_path: "/repo".into(),
                base_branch: "main".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err, StoreError::ProjectExists);
}

#[tokio::test]
async fn find_project_by_repo_path_should_find_only_that_path() {
    let f = Fixture::new();
    let id = f.project("/repo").await;
    let found = f
        .store
        .find_project_by_repo_path("/repo")
        .await
        .unwrap()
        .map(|p| p.id);
    let missing = f.store.find_project_by_repo_path("/other").await.unwrap();
    assert_eq!((found, missing), (Some(id), None));
}

#[tokio::test]
async fn workspace_should_always_get_its_tasks_project() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, workspaces) = f
        .task(&project, vec![ws("a", "/w/a"), ws("b", "/w/b")])
        .await
        .unwrap();
    assert!(workspaces.iter().all(|w| w.project_id == project));
}

#[tokio::test]
async fn task_in_an_unknown_project_should_be_not_found_and_write_nothing() {
    let f = Fixture::new();
    let err = f.task(&ProjectId::new(), vec![]).await.unwrap_err();
    let tasks: i64 = f
        .raw()
        .query_row("SELECT count(*) FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!((err, tasks), (StoreError::NotFound, 0));
}

#[tokio::test]
async fn add_workspace_should_copy_the_project_and_reject_an_unknown_task() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (task, _) = f.task(&project, vec![]).await.unwrap();
    let added = f
        .store
        .add_workspace(Actor::User, &task, ws("a", "/w/a"))
        .await
        .unwrap();
    let unknown = f
        .store
        .add_workspace(Actor::User, &TaskId::new(), ws("b", "/w/b"))
        .await;
    assert_eq!(
        (added.value.project_id, unknown.unwrap_err()),
        (project, StoreError::NotFound)
    );
}

#[tokio::test]
async fn reused_branch_should_be_branch_in_use_until_discarded() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (task, first) = f.task(&project, vec![ws("feat", "/w/1")]).await.unwrap();
    let taken = f
        .store
        .add_workspace(Actor::User, &task, ws("feat", "/w/2"))
        .await
        .unwrap_err();
    f.raw()
        .execute(
            "UPDATE workspace SET state = 'discarded' WHERE id = ?1",
            [first[0].id.as_str()],
        )
        .unwrap();
    let reused = f
        .store
        .add_workspace(Actor::User, &task, ws("feat", "/w/3"))
        .await;
    assert_eq!(taken, StoreError::BranchInUse);
    assert!(reused.is_ok());
}

#[tokio::test]
async fn same_branch_in_another_project_should_be_allowed() {
    let f = Fixture::new();
    let a = f.project("/a").await;
    let b = f.project("/b").await;
    f.task(&a, vec![ws("feat", "/w/a")]).await.unwrap();
    assert!(f.task(&b, vec![ws("feat", "/w/b")]).await.is_ok());
}

#[tokio::test]
async fn duplicate_worktree_path_should_be_worktree_path_in_use_and_roll_back_the_task() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    f.task(&project, vec![ws("a", "/w/same")]).await.unwrap();
    let err = f
        .task(&project, vec![ws("b", "/w/same")])
        .await
        .unwrap_err();
    let tasks: i64 = f
        .raw()
        .query_row("SELECT count(*) FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!((err, tasks), (StoreError::WorktreePathInUse, 1));
}

#[tokio::test]
async fn second_winner_should_be_winner_exists() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f
        .task(&project, vec![ws("a", "/w/a"), ws("b", "/w/b")])
        .await
        .unwrap();
    let won = f
        .store
        .set_workspace_outcome(Actor::User, &w[0].id, WorkspaceOutcome::Winner)
        .await
        .unwrap();
    let second = f
        .store
        .set_workspace_outcome(Actor::User, &w[1].id, WorkspaceOutcome::Winner)
        .await;
    assert_eq!(won.value.outcome, Some(WorkspaceOutcome::Winner));
    assert_eq!(second.unwrap_err(), StoreError::WinnerExists);
}

#[tokio::test]
async fn second_live_agent_run_should_be_run_already_live_but_a_setup_run_is_fine() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let second = f.run(&w[0].id, RunKind::Agent).await.unwrap_err();
    let setup = f.run(&w[0].id, RunKind::Setup).await;
    assert_eq!(second, StoreError::RunAlreadyLive);
    assert!(setup.is_ok());
}

#[tokio::test]
async fn new_agent_run_should_be_allowed_once_the_last_one_ended() {
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let first = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.store
        .transition_run(
            Actor::User,
            &first,
            RunTransition::end(RunStatus::Stopped, STOP),
        )
        .await
        .unwrap();
    assert!(f.run(&w[0].id, RunKind::Agent).await.is_ok());
}

#[tokio::test]
async fn run_in_an_unknown_workspace_should_be_not_found() {
    let f = Fixture::new();
    assert_eq!(
        f.run(&WorkspaceId::new(), RunKind::Agent)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
}

// ---- AC-8 through the store: transitions and events ----

#[tokio::test]
async fn transitions_should_write_the_row_and_one_event_each() {
    let mut f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.broadcast();
    for change in [
        RunTransition::to(RunStatus::Starting),
        RunTransition::to(RunStatus::Running),
        RunTransition::end(RunStatus::Stopped, STOP),
    ] {
        f.store
            .transition_run(Actor::User, &run, change)
            .await
            .unwrap();
    }
    let stored = f.store.get_run(&run).await.unwrap();
    let statuses: Vec<_> = f
        .broadcast()
        .into_iter()
        .map(|e| match e.kind {
            EventKind::RunStatusChanged { from, status, .. } => (from, status),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        statuses,
        [
            (RunStatus::Queued, RunStatus::Starting),
            (RunStatus::Starting, RunStatus::Running),
            (RunStatus::Running, RunStatus::Stopped)
        ]
    );
    assert!(stored.started_at.is_some() && stored.ended_at.is_some());
    assert_eq!(
        (stored.status, stored.end_reason, stored.log_bytes),
        (RunStatus::Stopped, Some(EndReason::UserStop), 12)
    );
}

#[tokio::test]
async fn refused_transition_should_write_nothing_and_send_no_event() {
    let mut f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.broadcast();
    let err = f
        .store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Running))
        .await
        .unwrap_err();
    let lost = f
        .store
        .transition_run(Actor::User, &run, RunTransition::end(RunStatus::Lost, STOP))
        .await;
    assert!(matches!(err, StoreError::InvalidTransition(_)) && lost.is_err());
    assert_eq!(
        (
            f.store.get_run(&run).await.unwrap().status,
            f.broadcast().len()
        ),
        (RunStatus::Queued, 0)
    );
}

#[tokio::test]
async fn same_status_report_should_return_no_event() {
    let mut f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Starting))
        .await
        .unwrap();
    f.broadcast();
    let again = f
        .store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Starting))
        .await
        .unwrap();
    assert!(again.events.is_empty() && f.broadcast().is_empty());
}

#[tokio::test]
async fn unknown_run_should_be_not_found() {
    let f = Fixture::new();
    let err = f
        .store
        .transition_run(
            Actor::User,
            &RunId::new(),
            RunTransition::to(RunStatus::Starting),
        )
        .await
        .unwrap_err();
    assert_eq!(err, StoreError::NotFound);
}

#[tokio::test]
async fn broadcast_channel_should_carry_every_committed_event_in_id_order() {
    let mut f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f
        .task(&project, vec![ws("a", "/w/a"), ws("b", "/w/b")])
        .await
        .unwrap();
    for ws in &w {
        let run = f.run(&ws.id, RunKind::Agent).await.unwrap();
        f.store
            .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Starting))
            .await
            .unwrap();
    }
    let ids: Vec<i64> = f.broadcast().iter().map(|e| e.id).collect();
    let journal = f
        .store
        .events_since(0, 0, EventFilter::default())
        .await
        .unwrap();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(ids, journal.events.iter().map(|e| e.id).collect::<Vec<_>>());
}

#[tokio::test]
async fn run_event_should_carry_every_ancestor_id() {
    let mut f = Fixture::new();
    let project = f.project("/repo").await;
    let (task, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let event = f.broadcast().pop().unwrap();
    assert_eq!(
        (
            event.scope.project_id,
            event.scope.task_id,
            event.scope.workspace_id,
            event.scope.run_id
        ),
        (Some(project), Some(task), Some(w[0].id.clone()), Some(run))
    );
}

// ---- events_since basics (the method lands in Milestone 2) ----

#[tokio::test]
async fn events_since_should_page_filter_and_round_trip() {
    let mut f = Fixture::new();
    let a = f.project("/a").await;
    let b = f.project("/b").await;
    f.task(&a, vec![ws("x", "/w/x")]).await.unwrap();
    f.task(&b, vec![]).await.unwrap();
    let sent = f.broadcast();
    let first = f
        .store
        .events_since(0, 2, EventFilter::default())
        .await
        .unwrap();
    let rest = f
        .store
        .events_since(first.next_cursor, 0, EventFilter::default())
        .await
        .unwrap();
    let only_a = f
        .store
        .events_since(
            0,
            0,
            EventFilter {
                project_id: Some(a.clone()),
                ..EventFilter::default()
            },
        )
        .await
        .unwrap();
    assert!(first.has_more && !rest.has_more);
    assert_eq!([first.events, rest.events].concat(), sent);
    assert!(
        only_a.events.len() == 3
            && only_a
                .events
                .iter()
                .all(|e| e.scope.project_id.as_ref() == Some(&a))
    );
}

// ---- AC-6: startup recovery ----

const ENGINE_START: i64 = 9_000;

#[tokio::test]
async fn recovery_should_mark_live_runs_lost_and_leave_queued_runs_queued() {
    use RunStatus::*;
    let mut f = Fixture::new();
    let p = f.project("/repo").await;
    let (_, w) = f
        .task(
            &p,
            vec![
                ws("a", "/w/a"),
                ws("b", "/w/b"),
                ws("c", "/w/c"),
                ws("d", "/w/d"),
            ],
        )
        .await
        .unwrap();
    let mut live = Vec::new();
    for (ws, path) in w.iter().zip([
        &[Starting][..],
        &[Starting, Running],
        &[Starting, WaitingInput],
    ]) {
        let run = f.run(&ws.id, RunKind::Agent).await.unwrap();
        f.walk(&run, path).await;
        live.push(run);
    }
    let queued = f.run(&w[3].id, RunKind::Agent).await.unwrap();
    f.broadcast();

    let got = recover_at_startup(&f.store, f.run_log(), ENGINE_START)
        .await
        .unwrap();
    assert_eq!(got.lost, 3);
    for run in &live {
        let row = f.store.get_run(run).await.unwrap();
        assert_eq!(
            (row.status, row.end_reason, row.ended_at),
            (Lost, Some(EndReason::EngineRestart), Some(ENGINE_START)),
            "{run}"
        );
    }
    assert_eq!(f.store.get_run(&queued).await.unwrap().status, Queued);
    let events = f.broadcast();
    let lost_events: Vec<(RunId, RunStatus, i64)> = events
        .iter()
        .filter(|e| e.actor == Actor::Engine)
        .filter_map(|e| match &e.kind {
            EventKind::RunStatusChanged {
                run_id,
                status,
                end_reason: Some(EndReason::EngineRestart),
                at,
                ..
            } => Some((run_id.clone(), *status, *at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        (lost_events.len(), events.len()),
        (3, 3),
        "one event per lost run and nothing else: {events:#?}"
    );
    assert!(
        lost_events
            .iter()
            .all(|(run, status, at)| live.contains(run) && *status == Lost && *at == ENGINE_START)
    );
}

#[tokio::test]
async fn recovery_should_discard_preparing_workspaces_only() {
    let mut f = Fixture::new();
    let p = f.project("/repo").await;
    let preparing = NewWorkspace {
        state: InitialWorkspaceState::Preparing,
        ..ws("prep", "/w/prep")
    };
    let (_, w) = f
        .task(&p, vec![preparing, ws("ready", "/w/ready")])
        .await
        .unwrap();
    f.broadcast();

    let got = recover_at_startup(&f.store, f.run_log(), ENGINE_START)
        .await
        .unwrap();
    let raw = f.raw();
    let state_of = |id: &WorkspaceId| -> (String, Option<i64>) {
        raw.query_row(
            "SELECT state, closed_at FROM workspace WHERE id = ?1",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(
        (got.discarded, state_of(&w[0].id), state_of(&w[1].id)),
        (
            1,
            ("discarded".to_owned(), Some(ENGINE_START)),
            ("ready".to_owned(), None)
        )
    );
    let events = f.broadcast();
    assert!(
        matches!(
            events.as_slice(),
            [Event {
                actor: Actor::Engine,
                kind: EventKind::WorkspaceStateChanged {
                    from: WorkspaceState::Preparing,
                    to: WorkspaceState::Discarded
                },
                ..
            }]
        ),
        "{events:#?}"
    );
    assert_eq!(events[0].scope.workspace_id.as_ref(), Some(&w[0].id));
}

#[tokio::test]
async fn recovery_should_reconcile_log_bytes_flag_vanished_logs_and_sweep_stale_files() {
    use RunStatus::*;
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let (_, w) = f
        .task(
            &p,
            vec![
                ws("a", "/w/a"),
                ws("b", "/w/b"),
                ws("c", "/w/c"),
                ws("d", "/w/d"),
            ],
        )
        .await
        .unwrap();
    // Killed while running: its log has 7 bytes, the row still says 0.
    let killed = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.walk(&killed, &[Starting, Running]).await;
    log.append(&killed, 0, b"7 bytes").unwrap();
    // Ended with 12 bytes saved, then its file vanished.
    let vanished = f.run(&w[1].id, RunKind::Agent).await.unwrap();
    f.walk(&vanished, &[Starting, Stopped]).await;
    // Never wrote output: no file, and it stays present.
    let silent = f.run(&w[2].id, RunKind::Agent).await.unwrap();
    // Trimmed, but a crash left its file behind.
    let trimmed = f.run(&w[3].id, RunKind::Agent).await.unwrap();
    f.walk(&trimmed, &[Starting, Stopped]).await;
    f.raw()
        .execute(
            "UPDATE run SET log_state = 'trimmed' WHERE id = ?1",
            [trimmed.as_str()],
        )
        .unwrap();
    log.append(&trimmed, 0, b"left over").unwrap();
    // A log with no run row (a crash after a permanent delete), and a file that isn't a log.
    let orphan = RunId::new();
    log.append(&orphan, 0, b"orphan").unwrap();
    let notes = f.dir.path().join("runs").join("notes.txt");
    std::fs::write(&notes, "keep").unwrap();

    let got = recover_at_startup(&f.store, Arc::clone(&log) as Arc<dyn RunLog>, ENGINE_START)
        .await
        .unwrap();
    let fields = |run: &RunId| -> (i64, String) {
        f.raw()
            .query_row(
                "SELECT log_bytes, log_state FROM run WHERE id = ?1",
                [run.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(
        got,
        Recovered {
            lost: 1,
            discarded: 0,
            deleted_logs: 1,
            removed_files: 2
        }
    );
    assert_eq!(
        [
            fields(&killed),
            fields(&vanished),
            fields(&silent),
            fields(&trimmed)
        ],
        [
            (7, "present".to_owned()),
            (0, "deleted".to_owned()),
            (0, "present".to_owned()),
            (0, "trimmed".to_owned()),
        ]
    );
    assert_eq!(
        (
            log.len(&killed),
            log.len(&trimmed),
            log.len(&orphan),
            notes.exists()
        ),
        (
            Ok(7),
            Err(sy_core::ports::RunLogError::NotFound),
            Err(sy_core::ports::RunLogError::NotFound),
            true
        )
    );
    assert_eq!(
        f.store.get_run(&vanished).await.unwrap().log_state,
        LogState::Deleted
    );
}

#[tokio::test]
async fn recovery_should_change_nothing_on_a_second_start() {
    let mut f = Fixture::new();
    let p = f.project("/repo").await;
    let (_, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.walk(&run, &[RunStatus::Starting]).await;
    recover_at_startup(&f.store, f.run_log(), ENGINE_START)
        .await
        .unwrap();
    f.broadcast();
    let again = recover_at_startup(&f.store, f.run_log(), ENGINE_START + 1)
        .await
        .unwrap();
    assert_eq!((again, f.broadcast().len()), (Recovered::default(), 0));
}

#[test]
fn backup_temps_should_be_swept_and_finished_backups_kept() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(dir.path());
    paths.ensure_dirs().unwrap();
    let tmp = paths.backups.join("switchyard-v1-5.db.tmp");
    let kept = paths.backups.join("switchyard-v1-4.db");
    std::fs::write(&tmp, "partial").unwrap();
    std::fs::write(&kept, "backup").unwrap();
    assert_eq!(
        (
            sweep_backup_temps(&paths).unwrap(),
            tmp.exists(),
            kept.exists()
        ),
        (1, false, true)
    );
}

// ---- the rebuild rule ----

#[tokio::test]
async fn rebuild_migration_on_run_should_keep_every_child_row() {
    const REBUILD_RUN: &str = "-- rebuild
CREATE TABLE run_new (
  id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('agent', 'setup', 'run_script')), input TEXT,
  resumes_run_id TEXT REFERENCES run (id) ON DELETE SET NULL, agent_session_ref TEXT,
  status TEXT NOT NULL CHECK (status IN ('queued', 'starting', 'running', 'waiting_input', 'finished', 'failed', 'stopped', 'lost', 'paused')),
  exit_code INTEGER, end_reason TEXT, pid INTEGER, pgid INTEGER, pid_started_at INTEGER,
  queued_at INTEGER NOT NULL, started_at INTEGER, ended_at INTEGER,
  log_bytes INTEGER NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
  log_state TEXT NOT NULL DEFAULT 'present' CHECK (log_state IN ('present', 'incomplete', 'trimmed', 'deleted'))
) STRICT;
INSERT INTO run_new SELECT * FROM run;
DROP TABLE run;
ALTER TABLE run_new RENAME TO run;
CREATE UNIQUE INDEX uq_run_live_agent ON run (workspace_id)
  WHERE kind = 'agent' AND status IN ('queued', 'starting', 'running', 'waiting_input');
CREATE INDEX ix_run_workspace ON run (workspace_id, queued_at);
CREATE INDEX ix_run_status ON run (status);
CREATE INDEX ix_run_resumes ON run (resumes_run_id);
";
    let f = Fixture::new();
    let project = f.project("/repo").await;
    let (_, w) = f.task(&project, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    f.store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Starting))
        .await
        .unwrap();
    let count = |c: &Connection| -> i64 {
        c.query_row(
            "SELECT count(*) FROM event WHERE run_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let before = count(&f.raw());
    let db = f.db();
    drop(f.store);

    let migrations = [
        MIGRATIONS,
        &[Migration {
            name: "0002_rebuild_run.sql",
            sql: REBUILD_RUN,
            rebuild: true,
        }],
    ]
    .concat();
    let (tx, _rx) = mpsc::unbounded_channel();
    let reopened = SqliteStore::open_with(
        &db,
        &migrations,
        Arc::new(TickClock(AtomicI64::new(5000))),
        tx,
    )
    .unwrap();
    let after_run = reopened.get_run(&run).await.unwrap();
    let raw = Connection::open(&db).unwrap();
    let paused_ok = raw
        .execute(
            "UPDATE run SET status = 'paused' WHERE id = ?1",
            [run.as_str()],
        )
        .is_ok();
    assert_eq!(
        (count(&raw), after_run.status, paused_ok),
        (before, RunStatus::Starting, true)
    );
    // foreign_keys came back on for the store: a task in no project is refused by its FK.
    let orphan = reopened
        .create_task(
            Actor::User,
            NewTask {
                project_id: ProjectId::new(),
                title: "t".into(),
                prompt: String::new(),
                base_ref: "main".into(),
                parent_task_id: None,
                workspaces: vec![],
            },
        )
        .await;
    assert_eq!(orphan.unwrap_err(), StoreError::NotFound);
}

// ---- Milestone 3: the complete spine store (AC-7, AC-9, AC-10) ----

const KIB: u64 = 1024;

/// End a run on `ws` with `bytes` of output saved to its log file.
async fn ended_run(f: &Fixture, log: &FileRunLog, ws: &WorkspaceId, bytes: u64) -> RunId {
    let run = f.run(ws, RunKind::Agent).await.unwrap();
    f.walk(&run, &[RunStatus::Starting]).await;
    let data = vec![b'x'; usize::try_from(bytes).unwrap()];
    log.append(&run, 0, &data).unwrap();
    let end = RunEnd {
        reason: EndReason::Exited,
        exit_code: Some(0),
        log_bytes: bytes,
    };
    f.store
        .transition_run(
            Actor::User,
            &run,
            RunTransition::end(RunStatus::Finished, end),
        )
        .await
        .unwrap();
    run
}

fn event_count(raw: &Connection) -> i64 {
    raw.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))
        .unwrap()
}

fn rows(raw: &Connection, table: &str) -> i64 {
    raw.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn outcome_should_need_a_comparison_and_repeat_as_a_no_op() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (_, solo) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let (_, pair) = f
        .task(&p, vec![ws("b", "/w/b"), ws("c", "/w/c")])
        .await
        .unwrap();
    let refused = f
        .store
        .set_workspace_outcome(Actor::User, &solo[0].id, WorkspaceOutcome::Winner)
        .await;
    let first = f
        .store
        .set_workspace_outcome(Actor::User, &pair[0].id, WorkspaceOutcome::NotChosen)
        .await
        .unwrap();
    let again = f
        .store
        .set_workspace_outcome(Actor::User, &pair[0].id, WorkspaceOutcome::NotChosen)
        .await
        .unwrap();
    assert_eq!(
        refused.unwrap_err(),
        StoreError::NotAComparison(NotAComparison)
    );
    assert!(matches!(
        first.events[0].kind,
        EventKind::WorkspaceOutcomeSet {
            outcome: WorkspaceOutcome::NotChosen
        }
    ));
    assert!(again.events.is_empty());
}

#[tokio::test]
async fn workspace_lifecycle_should_close_on_merge_and_refuse_going_back() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (_, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let merged = f
        .store
        .set_workspace_state(
            Actor::User,
            &w[0].id,
            WorkspaceChange::Merged {
                commit: "abc123".into(),
            },
        )
        .await
        .unwrap();
    let before = event_count(&f.raw());
    let back = f
        .store
        .set_workspace_state(Actor::User, &w[0].id, WorkspaceChange::Ready)
        .await;
    let stored = f.store.get_workspace(&w[0].id).await.unwrap();
    assert_eq!(
        (stored.state, stored.merged_commit.as_deref()),
        (WorkspaceState::Merged, Some("abc123"))
    );
    assert!(stored.closed_at.is_some());
    assert!(matches!(
        merged.events[0].kind,
        EventKind::WorkspaceStateChanged {
            from: WorkspaceState::Ready,
            to: WorkspaceState::Merged
        }
    ));
    assert_eq!(
        back.unwrap_err(),
        StoreError::InvalidWorkspaceTransition(InvalidWorkspaceTransition {
            from: WorkspaceState::Merged,
            to: WorkspaceState::Ready
        })
    );
    assert_eq!(event_count(&f.raw()), before);
}

#[tokio::test]
async fn project_setting_should_bump_the_revision_and_journal_the_value() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let value = JsonValue::Object([("on".to_owned(), JsonValue::Bool(true))].into());
    let first = f
        .store
        .set_project_setting(Actor::User, &p, "notifications.muted", value.clone())
        .await
        .unwrap();
    let same = f
        .store
        .set_project_setting(Actor::User, &p, "notifications.muted", value.clone())
        .await
        .unwrap();
    let second = f
        .store
        .set_project_setting(Actor::User, &p, "limits.running", JsonValue::Int(3))
        .await
        .unwrap();
    let settings = f.store.project_settings(&p).await.unwrap();
    let missing = f
        .store
        .set_project_setting(Actor::User, &ProjectId::new(), "k", JsonValue::Null)
        .await;
    assert_eq!((first.value, same.value, second.value), (1, 1, 2));
    assert!(same.events.is_empty());
    assert_eq!(
        first.events[0].kind,
        EventKind::ProjectSettingChanged {
            key: "notifications.muted".into(),
            value: value.clone(),
            settings_revision: 1,
        }
    );
    assert_eq!(first.events[0].scope.project_id.as_ref(), Some(&p));
    assert_eq!(settings.revision, 2);
    assert_eq!(settings.values.get("notifications.muted"), Some(&value));
    assert_eq!(
        settings.values.get("limits.running"),
        Some(&JsonValue::Int(3))
    );
    assert_eq!(f.store.get_project(&p).await.unwrap().settings_revision, 2);
    assert_eq!(missing.unwrap_err(), StoreError::NotFound);
}

#[tokio::test]
async fn global_setting_should_journal_with_no_scope_except_the_probe() {
    let f = Fixture::new();
    let set = f
        .store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::Int(5))
        .await
        .unwrap();
    let probe_1 = f
        .store
        .set_setting(Actor::Engine, STORAGE_PROBE_KEY, JsonValue::Int(1))
        .await
        .unwrap();
    let probe_2 = f
        .store
        .set_setting(Actor::Engine, STORAGE_PROBE_KEY, JsonValue::Int(1))
        .await
        .unwrap();
    let page = f
        .store
        .events_since(0, 0, EventFilter::default())
        .await
        .unwrap();
    assert_eq!(set.events[0].scope, Default::default());
    assert!(probe_1.events.is_empty() && probe_2.events.is_empty());
    assert_eq!(
        f.store.get_setting(LOG_CAP_KEY).await.unwrap(),
        Some(JsonValue::Int(5))
    );
    assert_eq!(
        f.store.get_setting(STORAGE_PROBE_KEY).await.unwrap(),
        Some(JsonValue::Int(1))
    );
    assert_eq!(f.store.get_setting("nope").await.unwrap(), None);
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, set.events[0].kind);
}

#[tokio::test]
async fn archive_should_refuse_queued_or_live_runs_and_happen_once() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let queued_task = f.store.archive_task(Actor::User, &t).await;
    let queued_project = f.store.archive_project(Actor::User, &p).await;
    f.walk(&run, &[RunStatus::Starting, RunStatus::Running])
        .await;
    let live = f.store.archive_task(Actor::User, &t).await;
    f.walk(&run, &[RunStatus::Stopped]).await;
    let archived = f.store.archive_task(Actor::User, &t).await.unwrap();
    let again = f.store.archive_task(Actor::User, &t).await.unwrap();
    assert_eq!(queued_task.unwrap_err(), StoreError::HasLiveRuns);
    assert_eq!(queued_project.unwrap_err(), StoreError::HasLiveRuns);
    assert_eq!(live.unwrap_err(), StoreError::HasLiveRuns);
    assert!(archived.value.archived_at.is_some());
    assert_eq!(archived.events[0].kind, EventKind::TaskArchived {});
    assert_eq!(archived.events[0].scope.task_id.as_ref(), Some(&t));
    assert!(again.events.is_empty());
    assert_eq!(again.value.archived_at, archived.value.archived_at);
    // The rows and the log stay.
    assert_eq!(
        f.store.get_run(&run).await.unwrap().status,
        RunStatus::Stopped
    );
}

#[tokio::test]
async fn delete_task_should_need_an_archive_then_remove_rows_events_and_logs_only() {
    let f = Fixture::new();
    let log = f.run_log();
    // A real repo and worktree on disk, to prove nothing touches them.
    let repo = f.dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let worktree = f.dir.path().join("wt");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join("file.rs"), "fn main() {}").unwrap();
    let p = f.project(repo.to_str().unwrap()).await;
    let (keep, kept_ws) = f.task(&p, vec![ws("keep", "/w/keep")]).await.unwrap();
    let (gone, w) = f
        .task(
            &p,
            vec![ws("a", worktree.to_str().unwrap()), ws("b", "/w/b")],
        )
        .await
        .unwrap();
    let r1 = ended_run(&f, &log, &w[0].id, 10).await;
    let r2 = ended_run(&f, &log, &w[1].id, 20).await;
    let kept_run = ended_run(&f, &log, &kept_ws[0].id, 30).await;

    let unarchived = retention::delete_task(&f.store, log.clone(), &gone).await;
    f.store.archive_task(Actor::User, &gone).await.unwrap();
    let removed = retention::delete_task(&f.store, log.clone(), &gone)
        .await
        .unwrap();

    let raw = f.raw();
    let left: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM event WHERE task_id = ?1",
            [gone.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        unarchived.unwrap_err(),
        RetentionError::Store(StoreError::NotArchived)
    );
    assert_eq!(removed, vec![r1.clone(), r2.clone()]);
    assert_eq!(
        f.store.get_task(&gone).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        f.store.get_run(&r1).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(left, 0);
    assert_eq!(rows(&raw, "workspace"), 1);
    assert_eq!(log.len(&r1).unwrap_err(), RunLogError::NotFound);
    assert_eq!(log.len(&r2).unwrap_err(), RunLogError::NotFound);
    // The other task, its log, the project and its own events are untouched.
    assert!(f.store.get_task(&keep).await.is_ok());
    assert_eq!(log.len(&kept_run).unwrap(), 30);
    assert!(f.store.get_project(&p).await.is_ok());
    assert!(repo.join(".git").is_dir());
    assert!(worktree.join("file.rs").is_file());
}

#[tokio::test]
async fn delete_should_refuse_a_live_run_even_once_archived() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    f.store.archive_task(Actor::User, &t).await.unwrap();
    f.store.archive_project(Actor::User, &p).await.unwrap();
    // Nothing stops a run being queued in an archived task; delete must still refuse it.
    f.run(&w[0].id, RunKind::Agent).await.unwrap();
    assert_eq!(
        f.store.delete_task(&t).await.unwrap_err(),
        StoreError::HasLiveRuns
    );
    assert_eq!(
        f.store.delete_project(&p).await.unwrap_err(),
        StoreError::HasLiveRuns
    );
}

#[tokio::test]
async fn delete_project_should_cascade_everything_and_allow_its_unarchived_tasks() {
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let other = f.project("/other").await;
    f.store
        .set_project_setting(Actor::User, &p, "k", JsonValue::Int(1))
        .await
        .unwrap();
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = ended_run(&f, &log, &w[0].id, 8).await;
    let not_yet = f.store.delete_project(&p).await;
    f.store.archive_project(Actor::User, &p).await.unwrap();
    // The task itself was never archived, but its project was: it counts as archived.
    let task_ok = f.store.delete_task(&t).await.unwrap();
    let (_, w2) = f.task(&p, vec![ws("b", "/w/b")]).await.unwrap();
    let run2 = ended_run(&f, &log, &w2[0].id, 8).await;
    let removed = retention::delete_project(&f.store, log.clone(), &p)
        .await
        .unwrap();

    let raw = f.raw();
    assert_eq!(not_yet.unwrap_err(), StoreError::NotArchived);
    assert_eq!(task_ok, vec![run]);
    assert_eq!(removed, vec![run2.clone()]);
    for table in ["project_setting", "task", "workspace", "run"] {
        assert_eq!(rows(&raw, table), 0, "{table}");
    }
    let left: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM event WHERE project_id = ?1",
            [p.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(log.len(&run2).unwrap_err(), RunLogError::NotFound);
    assert!(f.store.get_project(&other).await.is_ok());
}

#[tokio::test]
async fn crash_between_delete_and_unlink_should_leave_orphans_the_next_start_sweeps() {
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = ended_run(&f, &log, &w[0].id, 5).await;
    f.store.archive_task(Actor::User, &t).await.unwrap();
    // The store commits; the engine dies before the use case removes the file.
    f.store.delete_task(&t).await.unwrap();
    assert_eq!(log.len(&run).unwrap(), 5);
    let got = recover_at_startup(&f.store, log.clone() as Arc<dyn RunLog>, ENGINE_START)
        .await
        .unwrap();
    assert_eq!(got.removed_files, 1);
    assert_eq!(log.len(&run).unwrap_err(), RunLogError::NotFound);
}

#[tokio::test]
async fn lists_should_page_by_keyset_and_hide_archived_unless_asked() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let mut tasks = Vec::new();
    for i in 0..5 {
        let (t, _) = f
            .task(&p, vec![ws(&format!("b{i}"), &format!("/w/{i}"))])
            .await
            .unwrap();
        tasks.push(t);
    }
    f.store.archive_task(Actor::User, &tasks[1]).await.unwrap();
    let mut seen = Vec::new();
    let mut query = ListQuery {
        after: None,
        limit: 2,
    };
    loop {
        let page = f.store.list_tasks(&p, true, query.clone()).await.unwrap();
        assert!(page.items.len() <= 2);
        seen.extend(page.items.into_iter().map(|t| t.id));
        match page.next {
            Some(next) => query.after = Some(next),
            None => break,
        }
    }
    let active = f
        .store
        .list_tasks(&p, false, ListQuery::default())
        .await
        .unwrap();
    let projects = f
        .store
        .list_projects(false, ListQuery::default())
        .await
        .unwrap();
    assert_eq!(seen, tasks);
    assert_eq!(active.items.len(), 4);
    assert!(active.items.iter().all(|t| t.id != tasks[1]));
    assert!(active.next.is_none());
    assert_eq!(projects.items.len(), 1);
    f.store.archive_project(Actor::User, &p).await.unwrap();
    let hidden = f
        .store
        .list_projects(false, ListQuery::default())
        .await
        .unwrap();
    let shown = f
        .store
        .list_projects(true, ListQuery::default())
        .await
        .unwrap();
    assert!(hidden.items.is_empty());
    assert_eq!(shown.items.len(), 1);
}

#[tokio::test]
async fn list_runs_should_filter_by_workspace_or_status_in_queue_order() {
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let (t, w) = f
        .task(&p, vec![ws("a", "/w/a"), ws("b", "/w/b")])
        .await
        .unwrap();
    let done = ended_run(&f, &log, &w[0].id, 1).await;
    let queued = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let setup = f.run(&w[1].id, RunKind::Setup).await.unwrap();
    let by_ws = f
        .store
        .list_runs(RunFilter::Workspace(w[0].id.clone()), ListQuery::default())
        .await
        .unwrap();
    let by_status = f
        .store
        .list_runs(
            RunFilter::Status(vec![RunStatus::Queued]),
            ListQuery::default(),
        )
        .await
        .unwrap();
    let workspaces = f
        .store
        .list_workspaces(&t, ListQuery::default())
        .await
        .unwrap();
    let ids = |runs: &[sy_core::model::Run]| runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&by_ws.items), vec![done, queued.clone()]);
    assert_eq!(ids(&by_status.items), vec![queued, setup]);
    assert_eq!(
        workspaces
            .items
            .iter()
            .map(|w| w.id.clone())
            .collect::<Vec<_>>(),
        w.iter().map(|w| w.id.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn run_setters_should_write_the_row_and_no_event() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (_, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let before = event_count(&f.raw());
    f.store
        .set_run_process(
            &run,
            RunProcess {
                pid: 41,
                pgid: 40,
                pid_started_at: 7,
            },
        )
        .await
        .unwrap();
    f.store.set_agent_session_ref(&run, "sess-1").await.unwrap();
    let after = f
        .store
        .set_log_state(&run, LogState::Incomplete)
        .await
        .unwrap();
    let missing = f
        .store
        .set_log_state(&RunId::new(), LogState::Incomplete)
        .await;
    assert_eq!(
        (after.pid, after.pgid, after.pid_started_at),
        (Some(41), Some(40), Some(7))
    );
    assert_eq!(after.agent_session_ref.as_deref(), Some("sess-1"));
    assert_eq!(after.log_state, LogState::Incomplete);
    assert_eq!(missing.unwrap_err(), StoreError::NotFound);
    assert_eq!(event_count(&f.raw()), before);
}

#[tokio::test]
async fn append_event_should_file_an_adapter_event_under_every_ancestor() {
    let f = Fixture::new();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = f.run(&w[0].id, RunKind::Agent).await.unwrap();
    let committed = f
        .store
        .append_event(
            Actor::Agent,
            Some(w[0].id.to_string()),
            &run,
            AdapterEvent::CommandRun {
                argv: vec!["cargo".into(), "test".into()],
                exit_code: Some(0),
            },
        )
        .await
        .unwrap();
    let page = f
        .store
        .events_since(
            0,
            0,
            EventFilter {
                run_id: Some(run.clone()),
                ..EventFilter::default()
            },
        )
        .await
        .unwrap();
    let event = page.events.last().unwrap();
    assert_eq!(event, &committed.events[0]);
    assert_eq!(event.actor_ref.as_deref(), Some(w[0].id.as_str()));
    assert_eq!(
        (
            event.scope.project_id.as_ref(),
            event.scope.task_id.as_ref(),
            event.scope.workspace_id.as_ref()
        ),
        (Some(&p), Some(&t), Some(&w[0].id))
    );
}

/// A run log whose `delete` checks the row is already committed `trimmed`,
/// proving the commit comes before the unlink (AC-10).
struct CheckingLog {
    inner: Arc<FileRunLog>,
    db: std::sync::Mutex<Connection>,
    unlinked: std::sync::Mutex<Vec<RunId>>,
}

impl RunLog for CheckingLog {
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
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
        let state: String = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT log_state FROM run WHERE id = ?1",
                [run.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "trimmed", "unlinked before trimmed was committed");
        self.unlinked.lock().unwrap().push(run.clone());
        self.inner.delete(run)
    }
    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
        self.inner.list()
    }
}

#[tokio::test]
async fn log_cap_should_trim_oldest_archived_first_and_never_active_or_live_logs() {
    let mut f = Fixture::new();
    let files = f.run_log();
    let log = Arc::new(CheckingLog {
        inner: files.clone(),
        db: std::sync::Mutex::new(f.raw()),
        unlinked: std::sync::Mutex::new(Vec::new()),
    });
    let size = 600 * KIB;
    let p = f.project("/repo").await;
    let gone = f.project("/gone").await;
    let (task_a, wa) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let (task_b, wb) = f.task(&p, vec![ws("b", "/w/b")]).await.unwrap();
    let (_, wc) = f.task(&gone, vec![ws("c", "/w/c")]).await.unwrap();
    let (_, wd) = f
        .task(&p, vec![ws("d", "/w/d"), ws("e", "/w/e")])
        .await
        .unwrap();
    let ra = ended_run(&f, &files, &wa[0].id, size).await;
    let rb = ended_run(&f, &files, &wb[0].id, size).await;
    let rc = ended_run(&f, &files, &wc[0].id, size).await;
    let active = ended_run(&f, &files, &wd[0].id, size).await;
    // A live run in the active task, with 100 KiB appended so far.
    let live = f.run(&wd[1].id, RunKind::Agent).await.unwrap();
    f.walk(&live, &[RunStatus::Starting, RunStatus::Running])
        .await;
    // Archive order: B first, then the project holding C, then A.
    f.store.archive_task(Actor::User, &task_b).await.unwrap();
    f.store.archive_project(Actor::User, &gone).await.unwrap();
    f.store.archive_task(Actor::User, &task_a).await.unwrap();
    f.store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::Int(1024 * 1024))
        .await
        .unwrap();
    f.broadcast();

    // 4 × 600 KiB + 100 KiB live = 2500 KiB; trimming B, C, A leaves 700 KiB.
    let check = enforce_log_cap(&f.store, log.clone(), 100 * KIB)
        .await
        .unwrap();

    let trims: Vec<_> = f
        .broadcast()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::RunLogTrimmed { bytes } => Some((e.scope.run_id.unwrap(), bytes)),
            _ => None,
        })
        .collect();
    assert_eq!(
        *log.unlinked.lock().unwrap(),
        vec![rb.clone(), rc.clone(), ra.clone()]
    );
    assert_eq!(
        trims,
        vec![(rb.clone(), size), (rc, size), (ra.clone(), size)]
    );
    assert_eq!(
        (check.before, check.after, check.trimmed, check.still_over()),
        (2500 * KIB, 700 * KIB, 3, false)
    );
    let trimmed = f.store.get_run(&rb).await.unwrap();
    assert_eq!(
        (trimmed.log_state, trimmed.log_bytes),
        (LogState::Trimmed, 0)
    );
    assert_eq!(files.len(&ra).unwrap_err(), RunLogError::NotFound);
    assert_eq!(files.len(&active).unwrap(), size);
    assert_eq!(
        f.store.get_run(&active).await.unwrap().log_state,
        LogState::Present
    );
    assert_eq!(f.store.log_total_ended().await.unwrap(), size);
}

#[tokio::test]
async fn log_cap_should_stop_once_under_and_leave_untrimmable_logs_over() {
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let (old, wo) = f.task(&p, vec![ws("o", "/w/o")]).await.unwrap();
    let (newer, wn) = f.task(&p, vec![ws("n", "/w/n")]).await.unwrap();
    let (_, wa) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let r_old = ended_run(&f, &log, &wo[0].id, 300).await;
    let r_new = ended_run(&f, &log, &wn[0].id, 300).await;
    ended_run(&f, &log, &wa[0].id, 300).await;
    f.store.archive_task(Actor::User, &old).await.unwrap();
    f.store.archive_task(Actor::User, &newer).await.unwrap();

    // Under the default 5 GiB cap: nothing happens.
    let idle = enforce_log_cap(&f.store, log.clone(), 0).await.unwrap();
    // A cap of 650 B: trimming the oldest archived log (900 → 600) is enough.
    f.store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::Int(650))
        .await
        .unwrap();
    let once = enforce_log_cap(&f.store, log.clone(), 0).await.unwrap();
    // A cap of 100 B: the other archived log goes, the active one can't.
    f.store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::Int(100))
        .await
        .unwrap();
    let over = enforce_log_cap(&f.store, log.clone(), 0).await.unwrap();
    // A cap that isn't a byte count falls back to the default.
    f.store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::String("big".into()))
        .await
        .unwrap();

    assert_eq!((idle.cap, idle.trimmed), (DEFAULT_LOG_CAP_BYTES, 0));
    assert_eq!((once.before, once.after, once.trimmed), (900, 600, 1));
    assert_eq!(
        f.store.get_run(&r_old).await.unwrap().log_state,
        LogState::Trimmed
    );
    assert_eq!(
        (over.after, over.trimmed, over.still_over()),
        (300, 1, true)
    );
    assert_eq!(
        f.store.get_run(&r_new).await.unwrap().log_state,
        LogState::Trimmed
    );
    assert_eq!(
        retention::log_cap(&f.store).await.unwrap(),
        DEFAULT_LOG_CAP_BYTES
    );
}

#[tokio::test]
async fn mark_log_trimmed_should_refuse_a_run_that_is_no_longer_a_candidate() {
    let f = Fixture::new();
    let log = f.run_log();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let active = ended_run(&f, &log, &w[0].id, 9).await;
    let not_archived = f
        .store
        .mark_log_trimmed(Actor::Engine, &active)
        .await
        .unwrap();
    f.store.archive_task(Actor::User, &t).await.unwrap();
    let first = f
        .store
        .mark_log_trimmed(Actor::Engine, &active)
        .await
        .unwrap();
    let again = f
        .store
        .mark_log_trimmed(Actor::Engine, &active)
        .await
        .unwrap();
    let missing = f.store.mark_log_trimmed(Actor::Engine, &RunId::new()).await;
    assert_eq!((not_archived.value, not_archived.events.len()), (0, 0));
    assert_eq!((first.value, first.events.len()), (9, 1));
    assert_eq!((again.value, again.events.len()), (0, 0));
    assert_eq!(missing.unwrap_err(), StoreError::NotFound);
}

// ---- AC-6, AC-9, AC-10: a log file that won't go never fails the use case ----

/// The real run logs, except that `delete` always fails (a stuck file).
struct StuckDeletes(Arc<FileRunLog>);

impl RunLog for StuckDeletes {
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
        self.0.append(run, offset, bytes)
    }
    fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError> {
        self.0.truncate(run, len)
    }
    fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError> {
        self.0.read(run, offset, max)
    }
    fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
        self.0.len(run)
    }
    fn delete(&self, _run: &RunId) -> Result<(), RunLogError> {
        Err(RunLogError::Io("operation not permitted".into()))
    }
    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
        self.0.list()
    }
}

#[tokio::test]
async fn recovery_should_finish_and_count_nothing_removed_when_a_stale_log_wont_go() {
    // covers: AC-6 (one stuck file never blocks the engine; the next start tries again)
    let f = Fixture::new();
    let files = f.run_log();
    let orphan = RunId::new();
    files.append(&orphan, 0, b"orphan").unwrap();
    let stuck: Arc<dyn RunLog> = Arc::new(StuckDeletes(files.clone()));

    let first = recover_at_startup(&f.store, stuck, ENGINE_START)
        .await
        .unwrap();
    let next = recover_at_startup(&f.store, files.clone() as Arc<dyn RunLog>, ENGINE_START)
        .await
        .unwrap();

    assert_eq!((first.removed_files, next.removed_files), (0, 1));
    assert_eq!(files.len(&orphan).unwrap_err(), RunLogError::NotFound);
}

#[tokio::test]
async fn delete_task_should_succeed_when_a_log_wont_go_and_leave_it_for_the_next_start() {
    // covers: AC-9 (rows go in one transaction; a file left behind is an orphan the sweep removes)
    let f = Fixture::new();
    let files = f.run_log();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = ended_run(&f, &files, &w[0].id, 5).await;
    f.store.archive_task(Actor::User, &t).await.unwrap();

    let removed = retention::delete_task(&f.store, Arc::new(StuckDeletes(files.clone())), &t)
        .await
        .unwrap();
    let left_behind = files.len(&run).unwrap();
    let swept = recover_at_startup(&f.store, files.clone() as Arc<dyn RunLog>, ENGINE_START)
        .await
        .unwrap();

    assert_eq!(removed, vec![run.clone()]);
    assert_eq!(
        f.store.get_run(&run).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!((left_behind, swept.removed_files), (5, 1));
    assert_eq!(files.len(&run).unwrap_err(), RunLogError::NotFound);
}

#[tokio::test]
async fn log_cap_should_count_a_trim_whose_unlink_failed_and_the_next_start_should_sweep_it() {
    // covers: AC-10 (`trimmed` is committed first, so a failed unlink is only a leftover file)
    let f = Fixture::new();
    let files = f.run_log();
    let p = f.project("/repo").await;
    let (t, w) = f.task(&p, vec![ws("a", "/w/a")]).await.unwrap();
    let run = ended_run(&f, &files, &w[0].id, 400).await;
    f.store.archive_task(Actor::User, &t).await.unwrap();
    f.store
        .set_setting(Actor::User, LOG_CAP_KEY, JsonValue::Int(100))
        .await
        .unwrap();

    let check = enforce_log_cap(&f.store, Arc::new(StuckDeletes(files.clone())), 0)
        .await
        .unwrap();
    let row = f.store.get_run(&run).await.unwrap();
    let left_behind = files.len(&run).unwrap();
    let swept = recover_at_startup(&f.store, files.clone() as Arc<dyn RunLog>, ENGINE_START)
        .await
        .unwrap();

    assert_eq!(
        (check.before, check.after, check.trimmed, check.still_over()),
        (400, 0, 1, false)
    );
    assert_eq!((row.log_state, row.log_bytes), (LogState::Trimmed, 0));
    assert_eq!((left_behind, swept.removed_files), (400, 1));
    assert_eq!(files.len(&run).unwrap_err(), RunLogError::NotFound);
}

#[tokio::test]
async fn log_cap_setting_should_fall_back_to_the_default_unless_it_is_a_non_negative_integer() {
    // covers: AC-10 (value sourcing: `storage.log_cap_bytes`, else 5 GiB)
    let f = Fixture::new();
    let unset = retention::log_cap(&f.store).await.unwrap();
    let mut got = Vec::new();
    for value in [
        JsonValue::Int(-1),
        JsonValue::Float(650.5),
        JsonValue::String("650".into()),
        JsonValue::Null,
        JsonValue::Int(0),
        JsonValue::UInt(u64::MAX),
    ] {
        f.store
            .set_setting(Actor::User, LOG_CAP_KEY, value)
            .await
            .unwrap();
        got.push(retention::log_cap(&f.store).await.unwrap());
    }

    assert_eq!(unset, DEFAULT_LOG_CAP_BYTES);
    assert_eq!(
        got,
        vec![
            DEFAULT_LOG_CAP_BYTES,
            DEFAULT_LOG_CAP_BYTES,
            DEFAULT_LOG_CAP_BYTES,
            DEFAULT_LOG_CAP_BYTES,
            0,
            u64::MAX,
        ]
    );
}
