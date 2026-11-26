//! `SqliteStore`: sy-core's `Store` port on SQLite.
//!
//! One `rusqlite::Connection` lives on a dedicated OS thread (spec 0001's one
//! connection rule), fed by a bounded command channel with a oneshot reply per
//! command. Each command is one transaction that writes its rows and its
//! events together. After each commit the thread pushes the committed events
//! into one ordered channel, which the broadcaster drains, so notifications
//! leave in `event_id` order (AC-18).
//!
//! A caller that stops waiting doesn't cancel a command: once sent, it runs
//! and commits, and its events still reach the broadcaster.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::config::DbConfig;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};
use sy_core::ids::{ApprovalId, ProjectId, RunId, TaskId, WorkspaceId};
use sy_core::model::{
    Actor, AdapterEvent, Approval, ApprovalDecision, ApprovalKind, ApprovalStatus, DecidedBy,
    EndReason, Event, EventKind, EventScope, JsonValue, LogState, NewApproval, NewEvent,
    NewProject, NewRun, NewTask, NewWorkspace, Project, ProjectSettings, Run, RunKind, RunStatus,
    RunTransition, STORAGE_PROBE_KEY, Task, Workspace, WorkspaceChange, WorkspaceOutcome,
    WorkspaceState, cut_summary,
};
use sy_core::ports::{
    ApprovalFilter, Clock, Committed, EventFilter, EventPage, Keyset, ListQuery, LogReconcile,
    Page, RunFilter, RunProcess, Store, StoreError, TrimCandidate, approval_page_size, page_size,
};
use tokio::sync::{mpsc, oneshot};

use super::{OpenError, StoragePaths, backup, migrate, open_private_file};

/// Commands that may wait for the database thread at once.
const COMMAND_QUEUE: usize = 1024;

/// A command slower than this logs a warning.
const SLOW_COMMAND: Duration = Duration::from_millis(100);

/// Each named unique index and the column list SQLite names when it is
/// violated. SQLite reports a unique violation by columns, not index name, so
/// this table maps one to the other; a test checks it against the live schema.
pub const UNIQUE_INDEXES: &[(&str, &str)] = &[
    ("uq_project_repo_path", "project.repo_path"),
    ("uq_run_live_agent", "run.workspace_id"),
    (
        "uq_workspace_branch",
        "workspace.project_id, workspace.branch",
    ),
    ("uq_workspace_winner", "workspace.task_id"),
    ("uq_workspace_worktree_path", "workspace.worktree_path"),
];

fn unique_index_error(index: &str) -> Option<StoreError> {
    Some(match index {
        "uq_project_repo_path" => StoreError::ProjectExists,
        "uq_run_live_agent" => StoreError::RunAlreadyLive,
        "uq_workspace_branch" => StoreError::BranchInUse,
        "uq_workspace_winner" => StoreError::WinnerExists,
        "uq_workspace_worktree_path" => StoreError::WorktreePathInUse,
        _ => return None,
    })
}

/// Map a SQLite error to a `StoreError`. The text kept never holds row data.
fn map_err(err: rusqlite::Error) -> StoreError {
    match &err {
        rusqlite::Error::SqliteFailure(e, message) => match e.code {
            ErrorCode::ConstraintViolation => {
                let columns = message
                    .as_deref()
                    .and_then(|m| m.strip_prefix("UNIQUE constraint failed: "));
                if let Some(error) = columns
                    .and_then(|c| UNIQUE_INDEXES.iter().find(|(_, cols)| *cols == c))
                    .and_then(|(index, _)| unique_index_error(index))
                {
                    return error;
                }
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY {
                    return StoreError::NotFound;
                }
                StoreError::Io(format!(
                    "constraint failed: {}",
                    message.as_deref().unwrap_or("?")
                ))
            }
            ErrorCode::DiskFull => StoreError::Full,
            _ => StoreError::Io(format!("{:?} ({})", e.code, e.extended_code)),
        },
        rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound,
        _ => StoreError::Io(err.to_string()),
    }
}

fn corrupt(what: &str) -> StoreError {
    StoreError::Io(format!("unreadable {what} in the database"))
}

/// Open (or create, as `0600`) the database and set the per connection
/// pragmas. Writes nothing, so a damaged or newer file is left exactly as it
/// was. Closing it does not checkpoint until [`checkpoint_on_close`] turns
/// that back on: as the last connection to a file whose WAL a crash left
/// behind, a plain close would fold that WAL into the database and delete
/// `-wal` and `-shm`, so a file that then fails its checks would no longer be
/// the one to set aside (AC-3, AC-4).
fn connect(path: &Path) -> Result<Connection, OpenError> {
    open_private_file(path).map_err(|e| OpenError::Unavailable(e.to_string()))?;
    let conn = Connection::open(path).map_err(|e| classify_open(&e))?;
    let setup = || -> rusqlite::Result<()> {
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_size_limit", 67_108_864)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.set_prepared_statement_cache_capacity(64);
        Ok(())
    };
    setup().map_err(|e| classify_open(&e))?;
    Ok(conn)
}

/// Switch to WAL (persistent, so it can write the file header). Only done
/// once the file has passed its checks.
fn use_wal(conn: &Connection) -> Result<(), OpenError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(|e| classify_open(&e))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(%mode, "the database did not switch to WAL");
    }
    Ok(())
}

/// Let a connection to a file that passed its checks checkpoint on close again.
fn checkpoint_on_close(conn: &Connection) -> Result<(), OpenError> {
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, false)
        .map(|_| ())
        .map_err(|e| classify_open(&e))
}

/// Open (or create, as `0600`) the database and set the connection pragmas.
pub fn open_connection(path: &Path) -> Result<Connection, OpenError> {
    let conn = connect(path)?;
    use_wal(&conn)?;
    checkpoint_on_close(&conn)?;
    Ok(conn)
}

fn classify_open(err: &rusqlite::Error) -> OpenError {
    match err.sqlite_error_code() {
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => {
            OpenError::Corrupt(err.to_string())
        }
        _ => OpenError::Unavailable(err.to_string()),
    }
}

/// The open sequence (AC-3): open the file, read `user_version`, refuse a
/// newer schema, run `PRAGMA quick_check`, switch to WAL, call `before`
/// (with the version and whether migrations are pending; the backup hook),
/// then migrate. Returns the connection and the schema version it reached.
fn open_checked(
    path: &Path,
    migrations: &[migrate::Migration],
    before: impl FnOnce(&Connection, i64, bool) -> Result<(), OpenError>,
) -> Result<(Connection, i64), OpenError> {
    let mut conn = connect(path)?;
    let version = migrate::user_version(&conn).map_err(|e| classify_open(&e))?;
    let known = migrate::latest(migrations);
    if version > known {
        return Err(OpenError::NewerSchema {
            found: version,
            known,
        });
    }
    match backup::quick_check(&conn) {
        Ok(true) => {}
        Ok(false) => {
            return Err(OpenError::Corrupt(
                "the integrity check (quick_check) found problems".into(),
            ));
        }
        Err(err) => return Err(classify_open(&err)),
    }
    use_wal(&conn)?;
    before(&conn, version, version < known)?;
    migrate::run(&mut conn, migrations)?;
    checkpoint_on_close(&conn)?;
    Ok((conn, known))
}

/// A store opened through the full startup sequence.
pub struct Opened {
    /// The store.
    pub store: SqliteStore,
    /// The schema version it is at now (`PRAGMA user_version`).
    pub schema_version: i64,
    /// The backup written while opening, if one was due.
    pub backup: Option<String>,
}

type Job = Box<dyn FnOnce(&mut Db) + Send>;

struct Db {
    conn: Connection,
    clock: Arc<dyn Clock>,
    events: mpsc::UnboundedSender<Event>,
}

impl Db {
    /// Commit `tx` and hand its events to the broadcaster, in order.
    fn commit(
        tx: Transaction<'_>,
        events: &[Event],
        sink: &mpsc::UnboundedSender<Event>,
    ) -> Result<(), StoreError> {
        tx.commit().map_err(map_err)?;
        for event in events.iter() {
            // The broadcaster may be gone at shutdown; the rows are committed either way.
            let _ = sink.send(event.clone());
        }
        Ok(())
    }
}

/// The SQLite store. Cheap to share; every call goes to the database thread.
pub struct SqliteStore {
    jobs: mpsc::Sender<Job>,
}

impl SqliteStore {
    /// Open storage the way the engine does at startup (AC-2, AC-3): the
    /// checks, a backup when one is due (before any pending migration, or
    /// when the newest is over 24 hours old), then migrations, then the
    /// database thread. Committed events go to `events`, in id order.
    pub fn open(
        paths: &StoragePaths,
        clock: Arc<dyn Clock>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Opened, OpenError> {
        Self::open_paths_with(paths, migrate::MIGRATIONS, clock, events)
    }

    /// [`SqliteStore::open`] with an explicit migration list (tests add their own).
    pub fn open_paths_with(
        paths: &StoragePaths,
        migrations: &[migrate::Migration],
        clock: Arc<dyn Clock>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Opened, OpenError> {
        let mut written = None;
        let now = clock.now_ms();
        let (conn, schema_version) =
            open_checked(&paths.db, migrations, |conn, version, pending| {
                if !backup::backup_due(paths, version, pending, now) {
                    return Ok(());
                }
                match backup::write_backup(conn, paths, version, now) {
                    Ok(name) => {
                        written = Some(name);
                        Ok(())
                    }
                    // Never migrate without a backup to go back to.
                    Err(err) if pending => Err(OpenError::Unavailable(format!(
                        "no backup could be written before migrating: {err}"
                    ))),
                    Err(err) => {
                        tracing::warn!(%err, "the daily backup could not be written");
                        Ok(())
                    }
                }
            })?;
        Ok(Opened {
            store: Self::start(conn, clock, events)?,
            schema_version,
            backup: written,
        })
    }

    /// Open the database at `path` with the checks and migrations but no
    /// backups (store tests).
    pub fn open_with(
        path: &Path,
        migrations: &[migrate::Migration],
        clock: Arc<dyn Clock>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Self, OpenError> {
        let (conn, _) = open_checked(path, migrations, |_, _, _| Ok(()))?;
        Self::start(conn, clock, events)
    }

    fn start(
        conn: Connection,
        clock: Arc<dyn Clock>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Self, OpenError> {
        let (jobs, mut rx) = mpsc::channel::<Job>(COMMAND_QUEUE);
        let mut db = Db {
            conn,
            clock,
            events,
        };
        std::thread::Builder::new()
            .name("store".into())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    job(&mut db);
                }
            })
            .map_err(|e| OpenError::Unavailable(e.to_string()))?;
        Ok(Self { jobs })
    }

    async fn call<T, F>(&self, name: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T, StoreError> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let job: Job = Box::new(move |db| {
            let span = tracing::debug_span!("store.command", name);
            let _entered = span.enter();
            let started = Instant::now();
            let result = f(db);
            let elapsed = started.elapsed();
            let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            match &result {
                Err(err) if matches!(err, StoreError::Io(_) | StoreError::Full) => {
                    tracing::error!(command = name, duration_ms, %err, "store command failed");
                }
                _ if elapsed > SLOW_COMMAND => {
                    tracing::warn!(command = name, duration_ms, "slow store command")
                }
                _ => tracing::trace!(command = name, duration_ms, "store command"),
            }
            let _ = reply.send(result);
        });
        self.jobs
            .send(job)
            .await
            .map_err(|_| StoreError::Unavailable)?;
        answer.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Run `PRAGMA wal_checkpoint(TRUNCATE)`, at clean shutdown.
    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        self.call("checkpoint", |db| {
            db.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .map_err(map_err)
        })
        .await
    }
}

// ---- row mapping ----

fn parse_id<T>(
    text: String,
    parse: fn(&str) -> Result<T, sy_core::ids::IdError>,
) -> rusqlite::Result<T> {
    parse(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_text<T>(text: &str, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    parse(text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown value {text:?}").into(),
        )
    })
}

const PROJECT_COLUMNS: &str =
    "id, name, repo_path, base_branch, settings_revision, created_at, archived_at";

fn project_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse_id(r.get(0)?, ProjectId::parse)?,
        name: r.get(1)?,
        repo_path: r.get(2)?,
        base_branch: r.get(3)?,
        settings_revision: r.get(4)?,
        created_at: r.get(5)?,
        archived_at: r.get(6)?,
    })
}

const TASK_COLUMNS: &str =
    "id, project_id, title, prompt, base_ref, parent_task_id, created_at, archived_at";

fn task_row(r: &Row<'_>) -> rusqlite::Result<Task> {
    let parent: Option<String> = r.get(5)?;
    Ok(Task {
        id: parse_id(r.get(0)?, TaskId::parse)?,
        project_id: parse_id(r.get(1)?, ProjectId::parse)?,
        title: r.get(2)?,
        prompt: r.get(3)?,
        base_ref: r.get(4)?,
        parent_task_id: parent.map(|id| parse_id(id, TaskId::parse)).transpose()?,
        created_at: r.get(6)?,
        archived_at: r.get(7)?,
    })
}

/// A JSON column's text for `value`.
fn json_text(value: &JsonValue) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|_| corrupt("setting value"))
}

/// A JSON column's value.
fn json_value(text: &str) -> rusqlite::Result<JsonValue> {
    serde_json::from_str(text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// The keyset start and fetch size (one extra row, to tell if more follow).
fn keyset_params(query: &ListQuery) -> (i64, String, i64, u32) {
    let limit = page_size(query.limit);
    let (at, id) = query
        .after
        .as_ref()
        .map_or((i64::MIN, String::new()), |k| (k.at, k.id.clone()));
    (at, id, i64::from(limit) + 1, limit)
}

/// Cut a fetch of `limit + 1` rows down to a page.
fn page<T>(mut items: Vec<T>, limit: u32, key: impl Fn(&T) -> Keyset) -> Page<T> {
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next = if more { items.last().map(key) } else { None };
    Page { items, next }
}

const WORKSPACE_COLUMNS: &str = "id, task_id, project_id, agent_id, agent_options, branch, worktree_path, base_commit, state, outcome, merged_commit, created_at, closed_at";

fn workspace_row(r: &Row<'_>) -> rusqlite::Result<Workspace> {
    let outcome: Option<String> = r.get(9)?;
    Ok(Workspace {
        id: parse_id(r.get(0)?, WorkspaceId::parse)?,
        task_id: parse_id(r.get(1)?, TaskId::parse)?,
        project_id: parse_id(r.get(2)?, ProjectId::parse)?,
        agent_id: r.get(3)?,
        agent_options: r.get(4)?,
        branch: r.get(5)?,
        worktree_path: r.get(6)?,
        base_commit: r.get(7)?,
        state: parse_text(&r.get::<_, String>(8)?, WorkspaceState::parse)?,
        outcome: outcome
            .map(|o| parse_text(&o, WorkspaceOutcome::parse))
            .transpose()?,
        merged_commit: r.get(10)?,
        created_at: r.get(11)?,
        closed_at: r.get(12)?,
    })
}

const RUN_COLUMNS: &str = "id, workspace_id, kind, input, resumes_run_id, agent_session_ref, status, exit_code, end_reason, pid, pgid, pid_started_at, queued_at, started_at, ended_at, log_bytes, log_state";

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let resumes: Option<String> = r.get(4)?;
    let end_reason: Option<String> = r.get(8)?;
    let log_bytes: i64 = r.get(15)?;
    Ok(Run {
        id: parse_id(r.get(0)?, RunId::parse)?,
        workspace_id: parse_id(r.get(1)?, WorkspaceId::parse)?,
        kind: parse_text(&r.get::<_, String>(2)?, RunKind::parse)?,
        input: r.get(3)?,
        resumes_run_id: resumes.map(|id| parse_id(id, RunId::parse)).transpose()?,
        agent_session_ref: r.get(5)?,
        status: parse_text(&r.get::<_, String>(6)?, RunStatus::parse)?,
        exit_code: r.get(7)?,
        end_reason: end_reason
            .map(|e| parse_text(&e, EndReason::parse))
            .transpose()?,
        pid: r.get(9)?,
        pgid: r.get(10)?,
        pid_started_at: r.get(11)?,
        queued_at: r.get(12)?,
        started_at: r.get(13)?,
        ended_at: r.get(14)?,
        log_bytes: u64::try_from(log_bytes).unwrap_or(0),
        log_state: parse_text(&r.get::<_, String>(16)?, LogState::parse)?,
    })
}

fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    let project: Option<String> = r.get(1)?;
    let task: Option<String> = r.get(2)?;
    let workspace: Option<String> = r.get(3)?;
    let run: Option<String> = r.get(4)?;
    let kind: String = r.get(8)?;
    let payload: String = r.get(9)?;
    let payload: Value = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind: EventKind = serde_json::from_value(json!({ "kind": kind, "payload": payload }))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(Event {
        id: r.get(0)?,
        at: r.get(5)?,
        actor: parse_text(&r.get::<_, String>(6)?, Actor::parse)?,
        actor_ref: r.get(7)?,
        scope: EventScope {
            project_id: project
                .map(|id| parse_id(id, ProjectId::parse))
                .transpose()?,
            task_id: task.map(|id| parse_id(id, TaskId::parse)).transpose()?,
            workspace_id: workspace
                .map(|id| parse_id(id, WorkspaceId::parse))
                .transpose()?,
            run_id: run.map(|id| parse_id(id, RunId::parse)).transpose()?,
        },
        kind,
    })
}

// ---- writes inside a transaction ----

fn insert_event(tx: &Transaction<'_>, event: NewEvent) -> Result<Event, StoreError> {
    let value = serde_json::to_value(&event.kind).map_err(|_| corrupt("event"))?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("event"))?;
    let payload = value
        .get("payload")
        .map_or_else(|| "{}".to_owned(), Value::to_string);
    let s = &event.scope;
    tx.prepare_cached(
        "INSERT INTO event (project_id, task_id, workspace_id, run_id, at, actor, actor_ref, kind, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .and_then(|mut st| {
        st.execute(params![
            s.project_id.as_ref().map(ProjectId::as_str),
            s.task_id.as_ref().map(TaskId::as_str),
            s.workspace_id.as_ref().map(WorkspaceId::as_str),
            s.run_id.as_ref().map(RunId::as_str),
            event.at,
            event.actor.as_str(),
            event.actor_ref,
            kind,
            payload,
        ])
    })
    .map_err(map_err)?;
    Ok(Event::committed(tx.last_insert_rowid(), event))
}

fn get_workspace(tx: &Connection, id: &WorkspaceId) -> Result<Workspace, StoreError> {
    tx.prepare_cached(&format!(
        "SELECT {WORKSPACE_COLUMNS} FROM workspace WHERE id = ?1"
    ))
    .and_then(|mut st| st.query_row([id.as_str()], workspace_row))
    .map_err(map_err)
}

fn get_run(conn: &Connection, id: &RunId) -> Result<Run, StoreError> {
    conn.prepare_cached(&format!("SELECT {RUN_COLUMNS} FROM run WHERE id = ?1"))
        .and_then(|mut st| st.query_row([id.as_str()], run_row))
        .map_err(map_err)
}

fn get_project(conn: &Connection, id: &ProjectId) -> Result<Project, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT {PROJECT_COLUMNS} FROM project WHERE id = ?1"
    ))
    .and_then(|mut st| st.query_row([id.as_str()], project_row))
    .map_err(map_err)
}

fn get_task(conn: &Connection, id: &TaskId) -> Result<Task, StoreError> {
    conn.prepare_cached(&format!("SELECT {TASK_COLUMNS} FROM task WHERE id = ?1"))
        .and_then(|mut st| st.query_row([id.as_str()], task_row))
        .map_err(map_err)
}

/// Whether any run under `scope` (a `workspace` column: `task_id` or
/// `project_id`) is queued or live. Only these two fixed columns are passed.
fn has_active_runs(conn: &Connection, scope: &'static str, id: &str) -> Result<bool, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT EXISTS (SELECT 1 FROM run r JOIN workspace w ON w.id = r.workspace_id
         WHERE w.{scope} = ?1 AND r.status IN ('queued', 'starting', 'running', 'waiting_input'))"
    ))
    .and_then(|mut st| st.query_row([id], |r| r.get(0)))
    .map_err(map_err)
}

/// Every run id under `scope` (see [`has_active_runs`]).
fn run_ids_under(
    conn: &Connection,
    scope: &'static str,
    id: &str,
) -> Result<Vec<RunId>, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT r.id FROM run r JOIN workspace w ON w.id = r.workspace_id
         WHERE w.{scope} = ?1 ORDER BY r.queued_at, r.id"
    ))
    .and_then(|mut st| {
        st.query_map([id], |r| parse_id(r.get(0)?, RunId::parse))?
            .collect::<rusqlite::Result<Vec<_>>>()
    })
    .map_err(map_err)
}

/// Update one run column by a fixed statement and return the run as it is now.
fn update_run(
    db: &mut Db,
    run: &RunId,
    sql: &'static str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Run, StoreError> {
    let tx = db.conn.transaction().map_err(map_err)?;
    let id = run.as_str();
    let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(values.len() + 1);
    bound.push(&id);
    bound.extend_from_slice(values);
    let changed = tx
        .prepare_cached(sql)
        .and_then(|mut st| st.execute(bound.as_slice()))
        .map_err(map_err)?;
    if changed == 0 {
        return Err(StoreError::NotFound);
    }
    let next = get_run(&tx, run)?;
    tx.commit().map_err(map_err)?;
    Ok(next)
}

const APPROVAL_COLUMNS: &str = "a.id, a.run_id, r.workspace_id, a.kind, a.summary, a.detail, a.status, a.requested_at, a.decided_at, a.decided_by, a.decided_by_ref, a.response";
/// Approvals with their run, for the workspace id.
const APPROVAL_FROM: &str = "approval a JOIN run r ON r.id = a.run_id";

fn approval_row(r: &Row<'_>) -> rusqlite::Result<Approval> {
    let decided_by: Option<String> = r.get(9)?;
    let response: Option<String> = r.get(11)?;
    let detail: String = r.get(5)?;
    let as_json = |index: usize, text: &str| {
        serde_json::from_str::<Value>(text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    };
    let message = response
        .map(|text| as_json(11, &text))
        .transpose()?
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned));
    Ok(Approval {
        id: parse_id(r.get(0)?, ApprovalId::parse)?,
        run_id: parse_id(r.get(1)?, RunId::parse)?,
        workspace_id: parse_id(r.get(2)?, WorkspaceId::parse)?,
        kind: parse_text(&r.get::<_, String>(3)?, ApprovalKind::parse)?,
        summary: r.get(4)?,
        detail: as_json(5, &detail)?,
        status: parse_text(&r.get::<_, String>(6)?, ApprovalStatus::parse)?,
        requested_at: r.get(7)?,
        decided_at: r.get(8)?,
        decided_by: decided_by
            .map(|d| parse_text(&d, DecidedBy::parse))
            .transpose()?,
        decided_by_ref: r.get(10)?,
        message,
    })
}

fn get_approval(conn: &Connection, id: &ApprovalId) -> Result<Approval, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM {APPROVAL_FROM} WHERE a.id = ?1"
    ))
    .and_then(|mut st| st.query_row([id.as_str()], approval_row))
    .map_err(map_err)
}

/// The pending approvals of one run (or every run), oldest first.
fn pending_approvals(conn: &Connection, run: Option<&RunId>) -> Result<Vec<Approval>, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM {APPROVAL_FROM}
         WHERE a.status = 'pending' AND (?1 IS NULL OR a.run_id = ?1)
         ORDER BY a.requested_at, a.id"
    ))
    .and_then(|mut st| {
        st.query_map([run.map(RunId::as_str)], approval_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
    })
    .map_err(map_err)
}

/// Move one approval out of `pending` inside `tx`, with its event. The
/// `WHERE status = 'pending'` makes the first change win.
fn decide_in(
    tx: &Transaction<'_>,
    actor: Actor,
    current: &Approval,
    decision: &ApprovalDecision,
    now: i64,
) -> Result<(Approval, Event), StoreError> {
    let next = current.decide(decision, now)?;
    // `response` is `{ message? }` for your decisions, and only a message otherwise.
    let response = (next.decided_by.is_some() || next.message.is_some()).then(|| {
        match &next.message {
            Some(m) => json!({ "message": m }),
            None => json!({}),
        }
        .to_string()
    });
    let changed = tx
        .prepare_cached(
            "UPDATE approval SET status = ?2, decided_at = ?3, decided_by = ?4, decided_by_ref = ?5, response = ?6
             WHERE id = ?1 AND status = 'pending'",
        )
        .and_then(|mut st| {
            st.execute(params![
                next.id.as_str(),
                next.status.as_str(),
                next.decided_at,
                next.decided_by.map(DecidedBy::as_str),
                next.decided_by_ref,
                response,
            ])
        })
        .map_err(map_err)?;
    if changed == 0 {
        return Err(StoreError::NotFound);
    }
    let ws = get_workspace(tx, &next.workspace_id)?;
    let event = insert_event(
        tx,
        NewEvent {
            at: now,
            actor,
            actor_ref: None,
            scope: EventScope::run(&ws, &next.run_id),
            kind: next.decided_event(),
        },
    )?;
    Ok((next, event))
}

/// Decide every approval in `pending` as `decision`, in one transaction.
fn decide_all(
    db: &mut Db,
    actor: Actor,
    run: Option<&RunId>,
    decision: &ApprovalDecision,
) -> Result<Committed<Vec<Approval>>, StoreError> {
    let now = db.clock.now_ms();
    let tx = db.conn.transaction().map_err(map_err)?;
    let mut approvals = Vec::new();
    let mut events = Vec::new();
    for current in pending_approvals(&tx, run)? {
        let (next, event) = decide_in(&tx, actor, &current, decision, now)?;
        approvals.push(next);
        events.push(event);
    }
    Db::commit(tx, &events, &db.events)?;
    Ok(Committed {
        value: approvals,
        events,
    })
}

/// Insert a workspace, copying `project_id` from its task inside the insert.
fn insert_workspace(
    tx: &Transaction<'_>,
    actor: Actor,
    now: i64,
    task: &TaskId,
    new: NewWorkspace,
) -> Result<(Workspace, Event), StoreError> {
    let state = WorkspaceState::from(new.state);
    let inserted = tx
        .prepare_cached(
            "INSERT INTO workspace (id, task_id, project_id, agent_id, agent_options, branch, worktree_path, base_commit, state, created_at)
             SELECT ?1, id, project_id, ?2, ?3, ?4, ?5, ?6, ?7, ?8 FROM task WHERE id = ?9",
        )
        .and_then(|mut st| {
            st.execute(params![
                new.id.as_str(),
                new.agent_id,
                new.agent_options,
                new.branch,
                new.worktree_path,
                new.base_commit,
                state.as_str(),
                now,
                task.as_str(),
            ])
        })
        .map_err(map_err)?;
    if inserted == 0 {
        return Err(StoreError::NotFound);
    }
    let ws = get_workspace(tx, &new.id)?;
    let event = insert_event(
        tx,
        NewEvent {
            at: now,
            actor,
            actor_ref: None,
            scope: EventScope::workspace(&ws),
            kind: EventKind::WorkspaceCreated {
                agent_id: ws.agent_id.clone(),
                branch: ws.branch.clone(),
                worktree_path: ws.worktree_path.clone(),
                state,
            },
        },
    )?;
    Ok((ws, event))
}

#[async_trait::async_trait]
impl Store for SqliteStore {
    async fn create_project(
        &self,
        actor: Actor,
        new: NewProject,
    ) -> Result<Committed<Project>, StoreError> {
        self.call("create_project", move |db| {
            let now = db.clock.now_ms();
            let project = Project {
                id: ProjectId::new(),
                name: new.name,
                repo_path: new.repo_path,
                base_branch: new.base_branch,
                settings_revision: 0,
                created_at: now,
                archived_at: None,
            };
            let tx = db.conn.transaction().map_err(map_err)?;
            tx.prepare_cached(
                "INSERT INTO project (id, name, repo_path, base_branch, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    project.id.as_str(),
                    project.name,
                    project.repo_path,
                    project.base_branch,
                    now
                ])
            })
            .map_err(map_err)?;
            let event = insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::project(&project.id),
                    kind: EventKind::ProjectCreated {
                        name: project.name.clone(),
                        repo_path: project.repo_path.clone(),
                        base_branch: project.base_branch.clone(),
                    },
                },
            )?;
            let events = vec![event];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: project, events })
        })
        .await
    }

    async fn find_project_by_repo_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<Project>, StoreError> {
        let repo_path = repo_path.to_owned();
        self.call("find_project_by_repo_path", move |db| {
            db.conn
                .prepare_cached(&format!(
                    "SELECT {PROJECT_COLUMNS} FROM project WHERE repo_path = ?1"
                ))
                .and_then(|mut st| st.query_row([&repo_path], project_row).optional())
                .map_err(map_err)
        })
        .await
    }

    async fn create_task(
        &self,
        actor: Actor,
        new: NewTask,
    ) -> Result<Committed<(Task, Vec<Workspace>)>, StoreError> {
        self.call("create_task", move |db| {
            let now = db.clock.now_ms();
            let task = Task {
                id: TaskId::new(),
                project_id: new.project_id,
                title: new.title,
                prompt: new.prompt,
                base_ref: new.base_ref,
                parent_task_id: new.parent_task_id,
                created_at: now,
                archived_at: None,
            };
            let tx = db.conn.transaction().map_err(map_err)?;
            tx.prepare_cached(
                "INSERT INTO task (id, project_id, title, prompt, base_ref, parent_task_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    task.id.as_str(),
                    task.project_id.as_str(),
                    task.title,
                    task.prompt,
                    task.base_ref,
                    task.parent_task_id.as_ref().map(TaskId::as_str),
                    now
                ])
            })
            .map_err(map_err)?;
            let mut events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::task(&task),
                    kind: EventKind::TaskCreated {
                        title: task.title.clone(),
                        base_ref: task.base_ref.clone(),
                        parent_task_id: task.parent_task_id.clone(),
                    },
                },
            )?];
            let mut workspaces = Vec::with_capacity(new.workspaces.len());
            for ws in new.workspaces {
                let (ws, event) = insert_workspace(&tx, actor, now, &task.id, ws)?;
                workspaces.push(ws);
                events.push(event);
            }
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: (task, workspaces),
                events,
            })
        })
        .await
    }

    async fn add_workspace(
        &self,
        actor: Actor,
        task: &TaskId,
        new: NewWorkspace,
    ) -> Result<Committed<Workspace>, StoreError> {
        let task = task.clone();
        self.call("add_workspace", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let (ws, event) = insert_workspace(&tx, actor, now, &task, new)?;
            let events = vec![event];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: ws, events })
        })
        .await
    }

    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Committed<Run>, StoreError> {
        self.call("create_run", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let ws = get_workspace(&tx, &new.workspace_id)?;
            tx.prepare_cached(
                "INSERT INTO run (id, workspace_id, kind, input, resumes_run_id, status, queued_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    new.id.as_str(),
                    new.workspace_id.as_str(),
                    new.kind.as_str(),
                    new.input,
                    new.resumes_run_id.as_ref().map(RunId::as_str),
                    now
                ])
            })
            .map_err(map_err)?;
            let run = get_run(&tx, &new.id)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::run(&ws, &run.id),
                    kind: EventKind::RunCreated {
                        kind: run.kind,
                        status: run.status,
                        resumes_run_id: run.resumes_run_id.clone(),
                    },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: run, events })
        })
        .await
    }

    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError> {
        let run = run.clone();
        self.call("get_run", move |db| get_run(&db.conn, &run))
            .await
    }

    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Committed<Run>, StoreError> {
        let run = run.clone();
        self.call("transition_run", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_run(&tx, &run)?;
            let Some(next) = current.transition(change, now)? else {
                return Ok(Committed {
                    value: current,
                    events: Vec::new(),
                });
            };
            tx.prepare_cached(
                "UPDATE run SET status = ?2, started_at = ?3, ended_at = ?4, end_reason = ?5, exit_code = ?6, log_bytes = ?7
                 WHERE id = ?1",
            )
            .and_then(|mut st| {
                st.execute(params![
                    next.id.as_str(),
                    next.status.as_str(),
                    next.started_at,
                    next.ended_at,
                    next.end_reason.map(EndReason::as_str),
                    next.exit_code,
                    i64::try_from(next.log_bytes).unwrap_or(i64::MAX),
                ])
            })
            .map_err(map_err)?;
            let ws = get_workspace(&tx, &next.workspace_id)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::run(&ws, &next.id),
                    kind: EventKind::RunStatusChanged {
                        run_id: next.id.clone(),
                        workspace_id: next.workspace_id.clone(),
                        from: current.status,
                        status: next.status,
                        end_reason: next.end_reason,
                        exit_code: next.exit_code,
                        at: now,
                    },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: next, events })
        })
        .await
    }

    async fn mark_live_runs_lost(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Run>>, StoreError> {
        self.call("mark_live_runs_lost", move |db| {
            let tx = db.conn.transaction().map_err(map_err)?;
            let live = tx
                .prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM run
                     WHERE status IN ('starting', 'running', 'waiting_input')
                     ORDER BY queued_at, id"
                ))
                .and_then(|mut st| {
                    st.query_map([], run_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            let mut runs = Vec::with_capacity(live.len());
            let mut events = Vec::with_capacity(live.len());
            for current in live {
                let next = current.lose(at)?;
                tx.prepare_cached(
                    "UPDATE run SET status = ?2, ended_at = ?3, end_reason = ?4, exit_code = NULL WHERE id = ?1",
                )
                .and_then(|mut st| {
                    st.execute(params![
                        next.id.as_str(),
                        next.status.as_str(),
                        next.ended_at,
                        next.end_reason.map(EndReason::as_str),
                    ])
                })
                .map_err(map_err)?;
                let ws = get_workspace(&tx, &next.workspace_id)?;
                events.push(insert_event(
                    &tx,
                    NewEvent {
                        at,
                        actor,
                        actor_ref: None,
                        scope: EventScope::run(&ws, &next.id),
                        kind: EventKind::RunStatusChanged {
                            run_id: next.id.clone(),
                            workspace_id: next.workspace_id.clone(),
                            from: current.status,
                            status: next.status,
                            end_reason: next.end_reason,
                            exit_code: None,
                            at,
                        },
                    },
                )?);
                runs.push(next);
            }
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: runs,
                events,
            })
        })
        .await
    }

    async fn discard_preparing_workspaces(
        &self,
        actor: Actor,
        at: i64,
    ) -> Result<Committed<Vec<Workspace>>, StoreError> {
        self.call("discard_preparing_workspaces", move |db| {
            let tx = db.conn.transaction().map_err(map_err)?;
            let preparing = tx
                .prepare_cached(&format!(
                    "SELECT {WORKSPACE_COLUMNS} FROM workspace WHERE state = 'preparing' ORDER BY created_at, id"
                ))
                .and_then(|mut st| {
                    st.query_map([], workspace_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            let to = WorkspaceState::Discarded;
            let mut workspaces = Vec::with_capacity(preparing.len());
            let mut events = Vec::with_capacity(preparing.len());
            for current in preparing {
                if !current.state.can_transition_to(to) {
                    continue;
                }
                tx.prepare_cached("UPDATE workspace SET state = ?2, closed_at = ?3 WHERE id = ?1")
                    .and_then(|mut st| st.execute(params![current.id.as_str(), to.as_str(), at]))
                    .map_err(map_err)?;
                let next = Workspace {
                    state: to,
                    closed_at: Some(at),
                    ..current.clone()
                };
                events.push(insert_event(
                    &tx,
                    NewEvent {
                        at,
                        actor,
                        actor_ref: None,
                        scope: EventScope::workspace(&next),
                        kind: EventKind::WorkspaceStateChanged {
                            from: current.state,
                            to,
                        },
                    },
                )?);
                workspaces.push(next);
            }
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: workspaces,
                events,
            })
        })
        .await
    }

    async fn reconcile_log_bytes(
        &self,
        files: Vec<(RunId, u64)>,
    ) -> Result<LogReconcile, StoreError> {
        self.call("reconcile_log_bytes", move |db| {
            let mut files: HashMap<RunId, u64> = files.into_iter().collect();
            let tx = db.conn.transaction().map_err(map_err)?;
            let rows = tx
                .prepare_cached("SELECT id, log_bytes, log_state FROM run")
                .and_then(|mut st| {
                    st.query_map([], |r| {
                        let bytes: i64 = r.get(1)?;
                        Ok((
                            parse_id(r.get(0)?, RunId::parse)?,
                            u64::try_from(bytes).unwrap_or(0),
                            parse_text(&r.get::<_, String>(2)?, LogState::parse)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            let mut out = LogReconcile::default();
            for (run, stored, state) in rows {
                let check = state.reconcile(stored, files.remove(&run));
                if check.log_state == LogState::Deleted && state != LogState::Deleted {
                    out.deleted.push(run.clone());
                }
                if check.remove_file {
                    out.remove.push(run.clone());
                }
                if (check.log_bytes, check.log_state) != (stored, state) {
                    tx.prepare_cached(
                        "UPDATE run SET log_bytes = ?2, log_state = ?3 WHERE id = ?1",
                    )
                    .and_then(|mut st| {
                        st.execute(params![
                            run.as_str(),
                            i64::try_from(check.log_bytes).unwrap_or(i64::MAX),
                            check.log_state.as_str(),
                        ])
                    })
                    .map_err(map_err)?;
                }
            }
            // What is left has no run row (an orphan from a crash after a permanent delete).
            out.remove.extend(files.into_keys());
            tx.commit().map_err(map_err)?;
            tracing::debug!(
                deleted = out.deleted.len(),
                remove = out.remove.len(),
                "reconciled run logs"
            );
            Ok(out)
        })
        .await
    }

    async fn events_since(
        &self,
        cursor: i64,
        limit: u32,
        filter: EventFilter,
    ) -> Result<EventPage, StoreError> {
        self.call("events_since", move |db| {
            let limit = page_size(limit);
            // Only fixed fragments are joined; every value is a bound parameter.
            let mut sql = String::from(
                "SELECT id, project_id, task_id, workspace_id, run_id, at, actor, actor_ref, kind, payload FROM event WHERE id > ?",
            );
            let mut values: Vec<String> = Vec::new();
            for (column, value) in [
                ("project_id", filter.project_id.as_ref().map(ProjectId::as_str)),
                ("task_id", filter.task_id.as_ref().map(TaskId::as_str)),
                ("workspace_id", filter.workspace_id.as_ref().map(WorkspaceId::as_str)),
                ("run_id", filter.run_id.as_ref().map(RunId::as_str)),
            ] {
                if let Some(value) = value {
                    sql.push_str(&format!(" AND {column} = ?"));
                    values.push(value.to_owned());
                }
            }
            sql.push_str(" ORDER BY id LIMIT ?");
            let mut st = db.conn.prepare_cached(&sql).map_err(map_err)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&cursor];
            bound.extend(values.iter().map(|v| v as &dyn rusqlite::ToSql));
            let fetch = i64::from(limit) + 1;
            bound.push(&fetch);
            let mut events = st
                .query_map(bound.as_slice(), event_row)
                .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
                .map_err(map_err)?;
            let has_more = events.len() > limit as usize;
            events.truncate(limit as usize);
            let next_cursor = events.last().map_or(cursor, |e| e.id);
            Ok(EventPage {
                events,
                next_cursor,
                has_more,
            })
        })
        .await
    }

    // ---- projects ----

    async fn get_project(&self, project: &ProjectId) -> Result<Project, StoreError> {
        let project = project.clone();
        self.call("get_project", move |db| get_project(&db.conn, &project))
            .await
    }

    async fn list_projects(
        &self,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Project>, StoreError> {
        self.call("list_projects", move |db| {
            let (at, id, fetch, limit) = keyset_params(&query);
            let rows = db
                .conn
                .prepare_cached(&format!(
                    "SELECT {PROJECT_COLUMNS} FROM project
                     WHERE (created_at > ?1 OR (created_at = ?1 AND id > ?2))
                       AND (?3 OR archived_at IS NULL)
                     ORDER BY created_at, id LIMIT ?4"
                ))
                .and_then(|mut st| {
                    st.query_map(params![at, id, include_archived, fetch], project_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            Ok(page(rows, limit, |p| Keyset {
                at: p.created_at,
                id: p.id.to_string(),
            }))
        })
        .await
    }

    async fn archive_project(
        &self,
        actor: Actor,
        project: &ProjectId,
    ) -> Result<Committed<Project>, StoreError> {
        let project = project.clone();
        self.call("archive_project", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_project(&tx, &project)?;
            if current.archived_at.is_some() {
                return Ok(Committed {
                    value: current,
                    events: Vec::new(),
                });
            }
            if has_active_runs(&tx, "project_id", project.as_str())? {
                return Err(StoreError::HasLiveRuns);
            }
            tx.prepare_cached("UPDATE project SET archived_at = ?2 WHERE id = ?1")
                .and_then(|mut st| st.execute(params![project.as_str(), now]))
                .map_err(map_err)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::project(&project),
                    kind: EventKind::ProjectArchived {},
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: Project {
                    archived_at: Some(now),
                    ..current
                },
                events,
            })
        })
        .await
    }

    async fn delete_project(&self, project: &ProjectId) -> Result<Vec<RunId>, StoreError> {
        let project = project.clone();
        self.call("delete_project", move |db| {
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_project(&tx, &project)?;
            if current.archived_at.is_none() {
                return Err(StoreError::NotArchived);
            }
            if has_active_runs(&tx, "project_id", project.as_str())? {
                return Err(StoreError::HasLiveRuns);
            }
            let runs = run_ids_under(&tx, "project_id", project.as_str())?;
            // Cascades to settings, tasks, workspaces, runs and their events.
            tx.prepare_cached("DELETE FROM project WHERE id = ?1")
                .and_then(|mut st| st.execute([project.as_str()]))
                .map_err(map_err)?;
            tx.commit().map_err(map_err)?;
            tracing::info!(%project, runs = runs.len(), "deleted a project for good");
            Ok(runs)
        })
        .await
    }

    async fn set_project_setting(
        &self,
        actor: Actor,
        project: &ProjectId,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<i64>, StoreError> {
        let project = project.clone();
        let key = key.to_owned();
        self.call("set_project_setting", move |db| {
            let now = db.clock.now_ms();
            let text = json_text(&value)?;
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_project(&tx, &project)?;
            let stored: Option<String> = tx
                .prepare_cached("SELECT value FROM project_setting WHERE project_id = ?1 AND key = ?2")
                .and_then(|mut st| st.query_row([project.as_str(), &key], |r| r.get(0)).optional())
                .map_err(map_err)?;
            let same = stored
                .map(|s| json_value(&s))
                .transpose()
                .map_err(map_err)?
                .is_some_and(|old| old == value);
            if same {
                return Ok(Committed {
                    value: current.settings_revision,
                    events: Vec::new(),
                });
            }
            tx.prepare_cached(
                "INSERT INTO project_setting (project_id, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (project_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            )
            .and_then(|mut st| st.execute(params![project.as_str(), key, text, now]))
            .map_err(map_err)?;
            let revision = current.settings_revision + 1;
            tx.prepare_cached("UPDATE project SET settings_revision = ?2 WHERE id = ?1")
                .and_then(|mut st| st.execute(params![project.as_str(), revision]))
                .map_err(map_err)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::project(&project),
                    kind: EventKind::ProjectSettingChanged {
                        key,
                        value,
                        settings_revision: revision,
                    },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: revision,
                events,
            })
        })
        .await
    }

    async fn project_settings(&self, project: &ProjectId) -> Result<ProjectSettings, StoreError> {
        let project = project.clone();
        self.call("project_settings", move |db| {
            let revision = get_project(&db.conn, &project)?.settings_revision;
            let values = db
                .conn
                .prepare_cached("SELECT key, value FROM project_setting WHERE project_id = ?1")
                .and_then(|mut st| {
                    st.query_map([project.as_str()], |r| {
                        Ok((r.get::<_, String>(0)?, json_value(&r.get::<_, String>(1)?)?))
                    })?
                    .collect::<rusqlite::Result<_>>()
                })
                .map_err(map_err)?;
            Ok(ProjectSettings { revision, values })
        })
        .await
    }

    // ---- global settings ----

    async fn set_setting(
        &self,
        actor: Actor,
        key: &str,
        value: JsonValue,
    ) -> Result<Committed<()>, StoreError> {
        let key = key.to_owned();
        self.call("set_setting", move |db| {
            let now = db.clock.now_ms();
            let text = json_text(&value)?;
            let journaled = key != STORAGE_PROBE_KEY;
            let tx = db.conn.transaction().map_err(map_err)?;
            if journaled {
                let stored: Option<String> = tx
                    .prepare_cached("SELECT value FROM setting WHERE key = ?1")
                    .and_then(|mut st| st.query_row([&key], |r| r.get(0)).optional())
                    .map_err(map_err)?;
                let same = stored
                    .map(|s| json_value(&s))
                    .transpose()
                    .map_err(map_err)?
                    .is_some_and(|old| old == value);
                if same {
                    return Ok(Committed {
                        value: (),
                        events: Vec::new(),
                    });
                }
            }
            tx.prepare_cached(
                "INSERT INTO setting (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            )
            .and_then(|mut st| st.execute(params![key, text, now]))
            .map_err(map_err)?;
            let mut events = Vec::new();
            if journaled {
                events.push(insert_event(
                    &tx,
                    NewEvent {
                        at: now,
                        actor,
                        actor_ref: None,
                        scope: EventScope::default(),
                        kind: EventKind::SettingChanged { key, value },
                    },
                )?);
            }
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: (), events })
        })
        .await
    }

    async fn get_setting(&self, key: &str) -> Result<Option<JsonValue>, StoreError> {
        let key = key.to_owned();
        self.call("get_setting", move |db| {
            db.conn
                .prepare_cached("SELECT value FROM setting WHERE key = ?1")
                .and_then(|mut st| {
                    st.query_row([&key], |r| json_value(&r.get::<_, String>(0)?))
                        .optional()
                })
                .map_err(map_err)
        })
        .await
    }

    // ---- tasks ----

    async fn get_task(&self, task: &TaskId) -> Result<Task, StoreError> {
        let task = task.clone();
        self.call("get_task", move |db| get_task(&db.conn, &task))
            .await
    }

    async fn list_tasks(
        &self,
        project: &ProjectId,
        include_archived: bool,
        query: ListQuery,
    ) -> Result<Page<Task>, StoreError> {
        let project = project.clone();
        self.call("list_tasks", move |db| {
            let (at, id, fetch, limit) = keyset_params(&query);
            let rows = db
                .conn
                .prepare_cached(&format!(
                    "SELECT {TASK_COLUMNS} FROM task
                     WHERE project_id = ?1
                       AND (created_at > ?2 OR (created_at = ?2 AND id > ?3))
                       AND (?4 OR archived_at IS NULL)
                     ORDER BY created_at, id LIMIT ?5"
                ))
                .and_then(|mut st| {
                    st.query_map(
                        params![project.as_str(), at, id, include_archived, fetch],
                        task_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            Ok(page(rows, limit, |t| Keyset {
                at: t.created_at,
                id: t.id.to_string(),
            }))
        })
        .await
    }

    async fn archive_task(
        &self,
        actor: Actor,
        task: &TaskId,
    ) -> Result<Committed<Task>, StoreError> {
        let task = task.clone();
        self.call("archive_task", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_task(&tx, &task)?;
            if current.archived_at.is_some() {
                return Ok(Committed {
                    value: current,
                    events: Vec::new(),
                });
            }
            if has_active_runs(&tx, "task_id", task.as_str())? {
                return Err(StoreError::HasLiveRuns);
            }
            tx.prepare_cached("UPDATE task SET archived_at = ?2 WHERE id = ?1")
                .and_then(|mut st| st.execute(params![task.as_str(), now]))
                .map_err(map_err)?;
            let next = Task {
                archived_at: Some(now),
                ..current
            };
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::task(&next),
                    kind: EventKind::TaskArchived {},
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: next,
                events,
            })
        })
        .await
    }

    async fn delete_task(&self, task: &TaskId) -> Result<Vec<RunId>, StoreError> {
        let task = task.clone();
        self.call("delete_task", move |db| {
            let tx = db.conn.transaction().map_err(map_err)?;
            let archived: Option<i64> = tx
                .prepare_cached(
                    "SELECT COALESCE(t.archived_at, p.archived_at) FROM task t
                     JOIN project p ON p.id = t.project_id WHERE t.id = ?1",
                )
                .and_then(|mut st| st.query_row([task.as_str()], |r| r.get(0)))
                .map_err(map_err)?;
            if archived.is_none() {
                return Err(StoreError::NotArchived);
            }
            if has_active_runs(&tx, "task_id", task.as_str())? {
                return Err(StoreError::HasLiveRuns);
            }
            let runs = run_ids_under(&tx, "task_id", task.as_str())?;
            // Cascades to workspaces, runs and their events; child tasks keep
            // existing with `parent_task_id` cleared.
            tx.prepare_cached("DELETE FROM task WHERE id = ?1")
                .and_then(|mut st| st.execute([task.as_str()]))
                .map_err(map_err)?;
            tx.commit().map_err(map_err)?;
            tracing::info!(%task, runs = runs.len(), "deleted a task for good");
            Ok(runs)
        })
        .await
    }

    // ---- workspaces ----

    async fn get_workspace(&self, workspace: &WorkspaceId) -> Result<Workspace, StoreError> {
        let workspace = workspace.clone();
        self.call("get_workspace", move |db| {
            get_workspace(&db.conn, &workspace)
        })
        .await
    }

    async fn list_workspaces(
        &self,
        task: &TaskId,
        query: ListQuery,
    ) -> Result<Page<Workspace>, StoreError> {
        let task = task.clone();
        self.call("list_workspaces", move |db| {
            let (at, id, fetch, limit) = keyset_params(&query);
            let rows = db
                .conn
                .prepare_cached(&format!(
                    "SELECT {WORKSPACE_COLUMNS} FROM workspace
                     WHERE task_id = ?1 AND (created_at > ?2 OR (created_at = ?2 AND id > ?3))
                     ORDER BY created_at, id LIMIT ?4"
                ))
                .and_then(|mut st| {
                    st.query_map(params![task.as_str(), at, id, fetch], workspace_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            Ok(page(rows, limit, |w| Keyset {
                at: w.created_at,
                id: w.id.to_string(),
            }))
        })
        .await
    }

    async fn set_workspace_state(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        change: WorkspaceChange,
    ) -> Result<Committed<Workspace>, StoreError> {
        let workspace = workspace.clone();
        self.call("set_workspace_state", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_workspace(&tx, &workspace)?;
            let Some(next) = current.change_state(change, now)? else {
                return Ok(Committed {
                    value: current,
                    events: Vec::new(),
                });
            };
            tx.prepare_cached(
                "UPDATE workspace SET state = ?2, closed_at = ?3, merged_commit = ?4 WHERE id = ?1",
            )
            .and_then(|mut st| {
                st.execute(params![
                    next.id.as_str(),
                    next.state.as_str(),
                    next.closed_at,
                    next.merged_commit,
                ])
            })
            .map_err(map_err)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::workspace(&next),
                    kind: EventKind::WorkspaceStateChanged {
                        from: current.state,
                        to: next.state,
                    },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: next,
                events,
            })
        })
        .await
    }

    async fn set_workspace_outcome(
        &self,
        actor: Actor,
        workspace: &WorkspaceId,
        outcome: WorkspaceOutcome,
    ) -> Result<Committed<Workspace>, StoreError> {
        let workspace = workspace.clone();
        self.call("set_workspace_outcome", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_workspace(&tx, &workspace)?;
            let siblings: i64 = tx
                .prepare_cached("SELECT COUNT(*) FROM workspace WHERE task_id = ?1")
                .and_then(|mut st| st.query_row([current.task_id.as_str()], |r| r.get(0)))
                .map_err(map_err)?;
            let siblings = usize::try_from(siblings).unwrap_or(0);
            let Some(next) = current.with_outcome(outcome, siblings)? else {
                return Ok(Committed {
                    value: current,
                    events: Vec::new(),
                });
            };
            tx.prepare_cached("UPDATE workspace SET outcome = ?2 WHERE id = ?1")
                .and_then(|mut st| st.execute(params![next.id.as_str(), outcome.as_str()]))
                .map_err(map_err)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::workspace(&next),
                    kind: EventKind::WorkspaceOutcomeSet { outcome },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: next,
                events,
            })
        })
        .await
    }

    // ---- runs ----

    async fn list_runs(
        &self,
        filter: RunFilter,
        query: ListQuery,
    ) -> Result<Page<Run>, StoreError> {
        self.call("list_runs", move |db| {
            let (at, id, fetch, limit) = keyset_params(&query);
            let keyset =
                "(queued_at > ?2 OR (queued_at = ?2 AND id > ?3)) ORDER BY queued_at, id LIMIT ?4";
            let (sql, scope) = match &filter {
                RunFilter::Workspace(ws) => (
                    format!("SELECT {RUN_COLUMNS} FROM run WHERE workspace_id = ?1 AND {keyset}"),
                    ws.as_str().to_owned(),
                ),
                RunFilter::Status(statuses) => {
                    let names: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
                    (
                        format!(
                            "SELECT {RUN_COLUMNS} FROM run
                             WHERE status IN (SELECT value FROM json_each(?1)) AND {keyset}"
                        ),
                        serde_json::to_string(&names).map_err(|_| corrupt("status list"))?,
                    )
                }
            };
            let rows = db
                .conn
                .prepare_cached(&sql)
                .and_then(|mut st| {
                    st.query_map(params![scope, at, id, fetch], run_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            Ok(page(rows, limit, |r| Keyset {
                at: r.queued_at,
                id: r.id.to_string(),
            }))
        })
        .await
    }

    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError> {
        let run = run.clone();
        self.call("set_run_process", move |db| {
            update_run(
                db,
                &run,
                "UPDATE run SET pid = ?2, pgid = ?3, pid_started_at = ?4 WHERE id = ?1",
                &[&process.pid, &process.pgid, &process.pid_started_at],
            )
        })
        .await
    }

    async fn set_agent_session_ref(
        &self,
        run: &RunId,
        session_ref: &str,
    ) -> Result<Run, StoreError> {
        let run = run.clone();
        let session_ref = session_ref.to_owned();
        self.call("set_agent_session_ref", move |db| {
            update_run(
                db,
                &run,
                "UPDATE run SET agent_session_ref = ?2 WHERE id = ?1",
                &[&session_ref],
            )
        })
        .await
    }

    async fn set_log_state(&self, run: &RunId, state: LogState) -> Result<Run, StoreError> {
        let run = run.clone();
        self.call("set_log_state", move |db| {
            update_run(
                db,
                &run,
                "UPDATE run SET log_state = ?2 WHERE id = ?1",
                &[&state.as_str()],
            )
        })
        .await
    }

    // ---- journal ----

    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<Committed<()>, StoreError> {
        let run = run.clone();
        self.call("append_event", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let row = get_run(&tx, &run)?;
            let ws = get_workspace(&tx, &row.workspace_id)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref,
                    scope: EventScope::run(&ws, &run),
                    kind: event.into(),
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed { value: (), events })
        })
        .await
    }

    // ---- approvals (spec 0003) ----

    async fn create_approval(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        new: NewApproval,
    ) -> Result<Committed<Approval>, StoreError> {
        self.call("create_approval", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let run = get_run(&tx, &new.run_id)?;
            let ws = get_workspace(&tx, &run.workspace_id)?;
            let summary = cut_summary(&new.summary);
            tx.prepare_cached(
                "INSERT INTO approval (id, run_id, kind, summary, detail, status, requested_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    new.id.as_str(),
                    new.run_id.as_str(),
                    new.kind.as_str(),
                    summary,
                    new.detail.to_string(),
                    now
                ])
            })
            .map_err(map_err)?;
            let approval = get_approval(&tx, &new.id)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref,
                    scope: EventScope::run(&ws, &run.id),
                    kind: EventKind::ApprovalRequested {
                        approval_id: approval.id.clone(),
                        kind: approval.kind,
                        summary: approval.summary.clone(),
                    },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: approval,
                events,
            })
        })
        .await
    }

    async fn decide_approval(
        &self,
        actor: Actor,
        approval: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Committed<Approval>, StoreError> {
        let approval = approval.clone();
        self.call("decide_approval", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let current = get_approval(&tx, &approval)?;
            let (next, event) = decide_in(&tx, actor, &current, &decision, now)?;
            let events = vec![event];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: next,
                events,
            })
        })
        .await
    }

    async fn get_approval(&self, approval: &ApprovalId) -> Result<Approval, StoreError> {
        let approval = approval.clone();
        self.call("get_approval", move |db| get_approval(&db.conn, &approval))
            .await
    }

    async fn list_approvals(
        &self,
        filter: ApprovalFilter,
        query: ListQuery,
    ) -> Result<Page<Approval>, StoreError> {
        self.call("list_approvals", move |db| {
            let limit = approval_page_size(query.limit);
            let (at, id) = query
                .after
                .as_ref()
                .map_or((i64::MIN, String::new()), |k| (k.at, k.id.clone()));
            let rows = db
                .conn
                .prepare_cached(&format!(
                    "SELECT {APPROVAL_COLUMNS} FROM {APPROVAL_FROM}
                     WHERE a.status = ?1 AND (?2 IS NULL OR a.run_id = ?2)
                       AND (a.requested_at > ?3 OR (a.requested_at = ?3 AND a.id > ?4))
                     ORDER BY a.requested_at, a.id LIMIT ?5"
                ))
                .and_then(|mut st| {
                    st.query_map(
                        params![
                            filter.status.as_str(),
                            filter.run_id.as_ref().map(RunId::as_str),
                            at,
                            id,
                            i64::from(limit) + 1
                        ],
                        approval_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)?;
            Ok(page(rows, limit, |a| Keyset {
                at: a.requested_at,
                id: a.id.to_string(),
            }))
        })
        .await
    }

    async fn cancel_pending_approvals(
        &self,
        actor: Actor,
        run: &RunId,
        message: Option<String>,
    ) -> Result<Committed<Vec<Approval>>, StoreError> {
        let run = run.clone();
        self.call("cancel_pending_approvals", move |db| {
            decide_all(db, actor, Some(&run), &ApprovalDecision::cancelled(message))
        })
        .await
    }

    async fn expire_pending_approvals(&self) -> Result<Committed<Vec<Approval>>, StoreError> {
        self.call("expire_pending_approvals", |db| {
            decide_all(db, Actor::Engine, None, &ApprovalDecision::expired())
        })
        .await
    }

    // ---- log cap ----

    async fn log_total_ended(&self) -> Result<u64, StoreError> {
        self.call("log_total_ended", |db| {
            let total: i64 = db
                .conn
                .prepare_cached(
                    "SELECT COALESCE(SUM(log_bytes), 0) FROM run
                     WHERE status IN ('finished', 'failed', 'stopped', 'lost')
                       AND log_state IN ('present', 'incomplete')",
                )
                .and_then(|mut st| st.query_row([], |r| r.get(0)))
                .map_err(map_err)?;
            Ok(u64::try_from(total).unwrap_or(0))
        })
        .await
    }

    async fn log_trim_candidates(&self) -> Result<Vec<TrimCandidate>, StoreError> {
        self.call("log_trim_candidates", |db| {
            db.conn
                .prepare_cached(&format!(
                    "SELECT r.id, r.log_bytes FROM {TRIMMABLE}
                     ORDER BY COALESCE(t.archived_at, p.archived_at), r.queued_at, r.id"
                ))
                .and_then(|mut st| {
                    st.query_map([], |r| {
                        let bytes: i64 = r.get(1)?;
                        Ok(TrimCandidate {
                            run_id: parse_id(r.get(0)?, RunId::parse)?,
                            log_bytes: u64::try_from(bytes).unwrap_or(0),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(map_err)
        })
        .await
    }

    async fn mark_log_trimmed(
        &self,
        actor: Actor,
        run: &RunId,
    ) -> Result<Committed<u64>, StoreError> {
        let run = run.clone();
        self.call("mark_log_trimmed", move |db| {
            let now = db.clock.now_ms();
            let tx = db.conn.transaction().map_err(map_err)?;
            let row = get_run(&tx, &run)?;
            // Checked again here: the run may have changed since it was listed.
            let still: Option<i64> = tx
                .prepare_cached(&format!(
                    "SELECT r.log_bytes FROM {TRIMMABLE} AND r.id = ?1"
                ))
                .and_then(|mut st| st.query_row([run.as_str()], |r| r.get(0)).optional())
                .map_err(map_err)?;
            let Some(bytes) = still.and_then(|b| u64::try_from(b).ok()) else {
                return Ok(Committed {
                    value: 0,
                    events: Vec::new(),
                });
            };
            tx.prepare_cached("UPDATE run SET log_state = 'trimmed', log_bytes = 0 WHERE id = ?1")
                .and_then(|mut st| st.execute([run.as_str()]))
                .map_err(map_err)?;
            let ws = get_workspace(&tx, &row.workspace_id)?;
            let events = vec![insert_event(
                &tx,
                NewEvent {
                    at: now,
                    actor,
                    actor_ref: None,
                    scope: EventScope::run(&ws, &run),
                    kind: EventKind::RunLogTrimmed { bytes },
                },
            )?];
            Db::commit(tx, &events, &db.events)?;
            Ok(Committed {
                value: bytes,
                events,
            })
        })
        .await
    }
}

/// The runs the log cap may trim (AC-10): ended, with saved output, in a task
/// that counts as archived (its own archive time, else its project's). Used
/// with the aliases `r`, `t` and `p`, followed by `AND …` or `ORDER BY …`.
const TRIMMABLE: &str = "run r
     JOIN workspace w ON w.id = r.workspace_id
     JOIN task t ON t.id = w.task_id
     JOIN project p ON p.id = t.project_id
     WHERE r.status IN ('finished', 'failed', 'stopped', 'lost')
       AND r.log_state IN ('present', 'incomplete')
       AND r.log_bytes > 0
       AND COALESCE(t.archived_at, p.archived_at) IS NOT NULL";
