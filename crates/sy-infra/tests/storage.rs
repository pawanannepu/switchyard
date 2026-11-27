#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! Storage failure handling against real SQLite files in a temp dir (spec
//! 0002, AC-1, AC-2, AC-3, AC-13): crash safe backups, the open sequence's
//! failure mapping, the explicit file modes, and the degraded write queue
//! over the real store with sy-core's failing double in between.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use rusqlite::Connection;
use sy_core::faults::FaultyStore;
use sy_core::ids::{RunId, WorkspaceId};
use sy_core::model::{
    Actor, EndReason, Event, EventKind, InitialWorkspaceState, JsonValue, LogState, NewProject,
    NewRun, NewTask, NewWorkspace, RunEnd, RunKind, RunStatus, RunTransition, STORAGE_PROBE_KEY,
    WorkspaceChange, WorkspaceState,
};
use sy_core::ports::{Clock, EventFilter, RunLogError, RunProcess, Store, StoreError};
use sy_core::write_queue::{QUEUE_CAP, QueuedStore, StorageNotice, WriteHealth};
use sy_infra::storage::backup::{
    self, BACKUP_MAX_AGE_MS, KEEP_BACKUPS, list_backups, passes_quick_check,
};
use sy_infra::storage::{MIGRATIONS, Migration, OpenError, Opened, SqliteStore, StoragePaths};
use tokio::sync::mpsc;

/// A clock the test sets.
struct SetClock(AtomicI64);

impl SetClock {
    fn at(ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(ms)))
    }
}

impl Clock for SetClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn storage() -> (tempfile::TempDir, StoragePaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(dir.path());
    paths.ensure_dirs().unwrap();
    (dir, paths)
}

fn open(
    paths: &StoragePaths,
    migrations: &[Migration],
    clock: &Arc<SetClock>,
) -> Result<Opened, OpenError> {
    let (tx, _rx) = mpsc::unbounded_channel();
    SqliteStore::open_paths_with(paths, migrations, Arc::clone(clock) as Arc<dyn Clock>, tx)
}

/// Open, then close again (waiting for the database thread to let go).
async fn open_and_close(
    paths: &StoragePaths,
    migrations: &[Migration],
    clock: &Arc<SetClock>,
) -> Opened {
    let opened = open(paths, migrations, clock).unwrap();
    opened.store.checkpoint().await.unwrap();
    opened
}

/// Test only migrations after the spine, each adding one table.
const EXTRA: &[Migration] = &[
    Migration {
        name: "0002_t2.sql",
        sql: "CREATE TABLE t2 (x INTEGER) STRICT;",
        rebuild: false,
    },
    Migration {
        name: "0003_t3.sql",
        sql: "CREATE TABLE t3 (x INTEGER) STRICT;",
        rebuild: false,
    },
    Migration {
        name: "0004_t4.sql",
        sql: "CREATE TABLE t4 (x INTEGER) STRICT;",
        rebuild: false,
    },
    Migration {
        name: "0005_t5.sql",
        sql: "CREATE TABLE t5 (x INTEGER) STRICT;",
        rebuild: false,
    },
    Migration {
        name: "0006_t6.sql",
        sql: "CREATE TABLE t6 (x INTEGER) STRICT;",
        rebuild: false,
    },
];

/// The spine plus the first `n` test migrations.
/// The schema version the real migrations reach.
fn base() -> i64 {
    MIGRATIONS.len() as i64
}

fn upto(n: usize) -> Vec<Migration> {
    [MIGRATIONS, &EXTRA[..n]].concat()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn tmp_files(paths: &StoragePaths) -> usize {
    std::fs::read_dir(&paths.backups)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp")
        })
        .count()
}

// ---- AC-2: backups ----

#[tokio::test]
async fn creating_a_fresh_database_should_make_no_backup() {
    let (_dir, paths) = storage();
    let opened = open_and_close(&paths, MIGRATIONS, &SetClock::at(1_000)).await;
    assert_eq!(
        (opened.backup, list_backups(&paths).unwrap().len()),
        (None, 0)
    );
}

#[tokio::test]
async fn a_pending_migration_should_first_write_a_checked_0600_backup_with_no_tmp_left() {
    let (_dir, paths) = storage();
    let clock = SetClock::at(1_000);
    let first = open(&paths, MIGRATIONS, &clock).unwrap();
    first
        .store
        .create_project(
            Actor::User,
            NewProject {
                name: "p".into(),
                repo_path: "/repo".into(),
                base_branch: "main".into(),
            },
        )
        .await
        .unwrap();
    first.store.checkpoint().await.unwrap();
    drop(first);

    clock.0.store(2_000, Ordering::Relaxed);
    let second = open_and_close(&paths, &upto(1), &clock).await;
    let backups = list_backups(&paths).unwrap();
    let only = &backups[0];
    let projects: i64 = Connection::open(&only.path)
        .unwrap()
        .query_row("SELECT count(*) FROM project", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        (
            second.backup.as_deref(),
            backups.len(),
            only.version,
            passes_quick_check(&only.path),
            mode(&only.path),
            tmp_files(&paths),
            projects,
            second.schema_version,
        ),
        (
            Some(format!("switchyard-v{}-2000.db", base()).as_str()),
            1,
            base(),
            true,
            0o600,
            0,
            1,
            base() + 1
        )
    );
}

#[tokio::test]
async fn five_backups_should_leave_only_the_three_newest() {
    let (_dir, paths) = storage();
    let clock = SetClock::at(1_000);
    open_and_close(&paths, MIGRATIONS, &clock).await;
    for n in 1..=5 {
        clock.0.fetch_add(1_000, Ordering::Relaxed);
        open_and_close(&paths, &upto(n), &clock).await;
    }
    let versions: Vec<i64> = list_backups(&paths)
        .unwrap()
        .iter()
        .map(|b| b.version)
        .collect();
    assert_eq!(
        (versions, KEEP_BACKUPS, tmp_files(&paths)),
        (vec![base() + 4, base() + 3, base() + 2], 3, 0)
    );
}

#[tokio::test]
async fn startup_should_back_up_only_when_the_newest_backup_is_over_a_day_old() {
    let (_dir, paths) = storage();
    let clock = SetClock::at(1_000);
    open_and_close(&paths, MIGRATIONS, &clock).await; // fresh: none
    let first = open_and_close(&paths, MIGRATIONS, &clock).await.backup; // none yet: due
    clock.0.store(1_000 + BACKUP_MAX_AGE_MS, Ordering::Relaxed);
    let within_a_day = open_and_close(&paths, MIGRATIONS, &clock).await.backup;
    clock.0.store(1_001 + BACKUP_MAX_AGE_MS, Ordering::Relaxed);
    let after_a_day = open_and_close(&paths, MIGRATIONS, &clock).await.backup;
    assert_eq!(
        (first.is_some(), within_a_day, after_a_day.is_some()),
        (true, None, true)
    );
}

#[tokio::test]
async fn a_failed_migration_should_roll_back_keep_the_version_and_keep_its_backup() {
    let (_dir, paths) = storage();
    let clock = SetClock::at(1_000);
    open_and_close(&paths, MIGRATIONS, &clock).await;
    let broken = [
        MIGRATIONS,
        &[Migration {
            name: "9999_broken.sql",
            sql: "CREATE TABLE half (x INTEGER) STRICT; INSERT INTO nowhere VALUES (1);",
            rebuild: false,
        }],
    ]
    .concat();
    let err = open(&paths, &broken, &clock).err().unwrap();
    let version: i64 = Connection::open(&paths.db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert!(matches!(err, OpenError::Migration { version, .. } if version == base() + 1));
    assert_eq!(
        (
            version,
            err.schema_version(),
            list_backups(&paths).unwrap().len()
        ),
        (base(), Some(base()), 1)
    );
}

#[test]
fn a_backup_that_fails_its_check_should_be_found_by_falling_back_to_an_older_one() {
    let (_dir, paths) = storage();
    let conn = Connection::open(&paths.db).unwrap();
    conn.execute_batch("CREATE TABLE x (a INTEGER); INSERT INTO x VALUES (1);")
        .unwrap();
    backup::write_backup(&conn, &paths, 1, 1_000).unwrap();
    std::fs::write(
        paths.backups.join("switchyard-v1-2000.db"),
        b"not a database",
    )
    .unwrap();
    let chosen = backup::newest_valid_backup(&paths).unwrap().unwrap();
    assert_eq!(chosen.file_name, "switchyard-v1-1000.db");
}

// ---- AC-3: the open sequence's failure mapping, with the file untouched ----

/// A small database in WAL mode at `user_version`, every connection closed.
fn make_db(path: &Path, user_version: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE x (a INTEGER); INSERT INTO x VALUES (1);",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", user_version)
        .unwrap();
}

#[test]
fn a_newer_schema_should_be_refused_without_touching_the_file() {
    let (_dir, paths) = storage();
    make_db(&paths.db, 999);
    let before = std::fs::read(&paths.db).unwrap();
    let err = open(&paths, MIGRATIONS, &SetClock::at(1)).err().unwrap();
    assert_eq!(
        (
            err.clone(),
            err.schema_version(),
            std::fs::read(&paths.db).unwrap() == before
        ),
        (
            OpenError::NewerSchema {
                found: 999,
                known: MIGRATIONS.len() as i64
            },
            Some(999),
            true
        )
    );
}

#[test]
fn a_damaged_header_should_be_corrupt_without_touching_the_file() {
    let (_dir, paths) = storage();
    make_db(&paths.db, 1);
    let mut bytes = std::fs::read(&paths.db).unwrap();
    bytes[..16].copy_from_slice(b"definitely not!!");
    std::fs::write(&paths.db, &bytes).unwrap();
    let err = open(&paths, MIGRATIONS, &SetClock::at(1)).err().unwrap();
    assert!(matches!(err, OpenError::Corrupt(_)), "{err:?}");
    assert_eq!(std::fs::read(&paths.db).unwrap(), bytes);
}

#[test]
fn an_unreadable_file_should_be_unavailable() {
    let (_dir, paths) = storage();
    make_db(&paths.db, 1);
    std::fs::set_permissions(&paths.db, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = open(&paths, MIGRATIONS, &SetClock::at(1)).err().unwrap();
    std::fs::set_permissions(&paths.db, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(err, OpenError::Unavailable(_)), "{err:?}");
}

/// What a crash leaves behind: a WAL mode database whose last writes
/// (`wal_sql`) are still only in `-wal`, never checkpointed. Built in a
/// scratch dir and copied while its connection is still open, so no close in
/// this process folds the WAL back in. Returns the bytes of the three files.
fn make_crash_image(paths: &StoragePaths, wal_sql: &str) -> [Vec<u8>; 3] {
    let scratch = tempfile::tempdir().unwrap();
    let src = scratch.path().join("switchyard.db");
    let conn = Connection::open(&src).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE x (a INTEGER); INSERT INTO x VALUES (1);
         PRAGMA wal_checkpoint(TRUNCATE);",
    )
    .unwrap();
    conn.execute_batch(wal_sql).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("switchyard.db{suffix}");
        std::fs::copy(scratch.path().join(&name), paths.data_dir.join(&name)).unwrap();
    }
    drop(conn);
    crash_files(paths)
}

/// The bytes of `switchyard.db`, `-wal` and `-shm` (empty when missing).
fn crash_files(paths: &StoragePaths) -> [Vec<u8>; 3] {
    ["", "-wal", "-shm"].map(|suffix| {
        std::fs::read(paths.data_dir.join(format!("switchyard.db{suffix}"))).unwrap_or_default()
    })
}

/// The database and its WAL are byte for byte as they were, and `-shm` is
/// still there to set aside. Its bytes may differ: every reader writes its
/// read marks into that shared memory index.
fn assert_untouched(paths: &StoragePaths, before: &[Vec<u8>; 3]) {
    let after = crash_files(paths);
    assert_eq!(
        (
            after[0] == before[0],
            after[1] == before[1],
            paths.data_dir.join("switchyard.db-shm").exists()
        ),
        (true, true, true),
        "(db unchanged, wal unchanged, shm present)"
    );
}

#[test]
fn a_newer_schema_left_in_the_wal_should_leave_the_db_and_wal_untouched() {
    let (_dir, paths) = storage();
    let before = make_crash_image(&paths, "PRAGMA user_version=999;");
    assert!(!before[1].is_empty(), "the image should have a WAL");
    let err = open(&paths, MIGRATIONS, &SetClock::at(1)).err().unwrap();
    assert!(
        matches!(err, OpenError::NewerSchema { found: 999, .. }),
        "{err:?}"
    );
    assert_untouched(&paths, &before);
}

#[test]
fn a_damaged_header_with_a_wal_should_leave_the_db_and_wal_untouched() {
    let (_dir, paths) = storage();
    // A row insert only: page 1 (the header) stays in the main file, so the
    // damage below is what SQLite reads.
    let image = make_crash_image(&paths, "INSERT INTO x VALUES (2);");
    assert!(!image[1].is_empty(), "the image should have a WAL");
    let mut bytes = std::fs::read(&paths.db).unwrap();
    bytes[..16].copy_from_slice(b"definitely not!!");
    std::fs::write(&paths.db, &bytes).unwrap();
    let before = crash_files(&paths);
    let err = open(&paths, MIGRATIONS, &SetClock::at(1)).err().unwrap();
    assert!(matches!(err, OpenError::Corrupt(_)), "{err:?}");
    assert_untouched(&paths, &before);
}

// ---- AC-4: setting the bad files aside ----

#[test]
fn set_aside_should_rename_wal_shm_db_and_runs_and_delete_nothing() {
    let (_dir, paths) = storage();
    for name in ["switchyard.db", "switchyard.db-wal", "switchyard.db-shm"] {
        std::fs::write(paths.data_dir.join(name), name).unwrap();
    }
    std::fs::write(paths.runs.join("a.log"), b"output").unwrap();
    let aside = backup::set_aside(&paths, 42).unwrap();
    let names: Vec<String> = aside
        .moved
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "switchyard.db-wal.bad-42",
            "switchyard.db-shm.bad-42",
            "switchyard.db.bad-42",
            "runs.bad-42"
        ]
    );
    assert_eq!(
        (
            std::fs::read(paths.data_dir.join("runs.bad-42/a.log")).unwrap(),
            mode(&paths.data_dir.join("switchyard.db.bad-42")),
            std::fs::read_dir(&paths.runs).unwrap().count(),
            mode(&paths.runs),
            paths.db.exists(),
        ),
        (b"output".to_vec(), 0o600, 0, 0o700, false)
    );
}

// ---- AC-1: explicit modes, whatever the umask ----

#[tokio::test]
async fn storage_files_should_be_private_even_under_a_permissive_umask() {
    // Explicit mode bits, never the umask: with umask 0 a default create would be 0666.
    let old = rustix::process::umask(rustix::fs::Mode::empty());
    let (_dir, paths) = storage();
    let clock = SetClock::at(1_000);
    open_and_close(&paths, MIGRATIONS, &clock).await;
    let opened = open_and_close(&paths, MIGRATIONS, &clock).await;
    rustix::process::umask(old);
    let backup = &list_backups(&paths).unwrap()[0];
    assert!(opened.backup.is_some());
    assert_eq!(
        (mode(&paths.db), mode(&backup.path), mode(&paths.backups)),
        (0o600, 0o600, 0o700)
    );
}

// ---- AC-13: the degraded write queue over the real store ----

struct Degradable {
    _dir: tempfile::TempDir,
    real: Arc<SqliteStore>,
    store: Arc<QueuedStore>,
    full: Arc<AtomicBool>,
    notices: mpsc::UnboundedReceiver<StorageNotice>,
    committed: mpsc::UnboundedReceiver<Event>,
}

impl Degradable {
    fn new() -> Self {
        let (dir, paths) = storage();
        let clock: Arc<dyn Clock> = SetClock::at(5_000);
        let (tx, committed) = mpsc::unbounded_channel();
        let real = Arc::new(
            SqliteStore::open_with(&paths.db, MIGRATIONS, Arc::clone(&clock), tx).unwrap(),
        );
        let full = Arc::new(AtomicBool::new(false));
        let switch = Arc::clone(&full);
        let faulty = Arc::new(FaultyStore::new(
            Arc::clone(&real) as Arc<dyn Store>,
            Arc::new(move || switch.load(Ordering::Relaxed)),
        ));
        let (ntx, notices) = mpsc::unbounded_channel();
        Self {
            _dir: dir,
            real,
            store: QueuedStore::new(faulty, clock, ntx),
            full,
            notices,
            committed,
        }
    }

    fn fill_disk(&self, full: bool) {
        self.full.store(full, Ordering::Relaxed);
    }

    /// A project, task and workspace, plus one agent run moved to `running`.
    async fn running(&self) -> (RunId, WorkspaceId) {
        let project = self
            .store
            .create_project(
                Actor::User,
                NewProject {
                    name: "p".into(),
                    repo_path: format!("/repo/{}", RunId::new()),
                    base_branch: "main".into(),
                },
            )
            .await
            .unwrap()
            .value;
        let ws = WorkspaceId::new();
        self.store
            .create_task(
                Actor::User,
                NewTask {
                    project_id: project.id,
                    title: "t".into(),
                    prompt: String::new(),
                    base_ref: "main".into(),
                    parent_task_id: None,
                    workspaces: vec![NewWorkspace {
                        id: ws.clone(),
                        agent_id: "synthetic".into(),
                        agent_options: "{}".into(),
                        branch: format!("b/{ws}"),
                        worktree_path: format!("/w/{ws}"),
                        base_commit: String::new(),
                        state: InitialWorkspaceState::Ready,
                    }],
                },
            )
            .await
            .unwrap();
        let run = self.new_run(&ws).await.unwrap();
        for status in [RunStatus::Starting, RunStatus::Running] {
            self.store
                .transition_run(Actor::User, &run, RunTransition::to(status))
                .await
                .unwrap();
        }
        (run, ws)
    }

    async fn new_run(&self, ws: &WorkspaceId) -> Result<RunId, StoreError> {
        let id = RunId::new();
        self.store
            .create_run(
                Actor::User,
                NewRun {
                    id: id.clone(),
                    workspace_id: ws.clone(),
                    kind: RunKind::Agent,
                    input: None,
                    resumes_run_id: None,
                },
            )
            .await?;
        Ok(id)
    }

    fn notices(&mut self) -> Vec<StorageNotice> {
        let mut out = Vec::new();
        while let Ok(n) = self.notices.try_recv() {
            out.push(n);
        }
        out
    }

    fn drain_committed(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.committed.try_recv() {
            out.push(e);
        }
        out
    }
}

const STOPPED: RunTransition = RunTransition::end(
    RunStatus::Stopped,
    RunEnd {
        reason: EndReason::UserStop,
        exit_code: None,
        log_bytes: 7,
    },
);

fn status_of(n: &StorageNotice) -> Option<(RunStatus, RunStatus)> {
    match n {
        StorageNotice::Unjournaled(EventKind::RunStatusChanged { from, status, .. }) => {
            Some((*from, *status))
        }
        _ => None,
    }
}

#[tokio::test]
async fn a_failed_transition_should_queue_announce_unjournaled_and_land_in_order_after_recovery() {
    let mut d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.drain_committed();
    d.fill_disk(true);

    let waiting = d
        .store
        .transition_run(
            Actor::User,
            &run,
            RunTransition::to(RunStatus::WaitingInput),
        )
        .await
        .unwrap();
    let stopped = d
        .store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    let notices = d.notices();
    let degraded = matches!(
        &notices[0],
        StorageNotice::Health(WriteHealth::Degraded { .. })
    );
    let announced: Vec<_> = notices.iter().filter_map(status_of).collect();
    let row_while_degraded = d.real.get_run(&run).await.unwrap().status;
    assert!(degraded);
    assert_eq!(
        (
            waiting.events.len() + stopped.events.len(),
            stopped.value.status,
            announced,
            row_while_degraded,
            d.store.get_run(&run).await.unwrap().status,
            d.store.queued().await,
        ),
        (
            0,
            RunStatus::Stopped,
            vec![
                (RunStatus::Running, RunStatus::WaitingInput),
                (RunStatus::WaitingInput, RunStatus::Stopped)
            ],
            RunStatus::Running,
            RunStatus::Stopped,
            2,
        )
    );

    // Still full: the retry keeps everything queued.
    assert!(!d.store.retry().await);
    d.fill_disk(false);
    assert!(d.store.retry().await);
    let replayed: Vec<RunStatus> = d
        .drain_committed()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::RunStatusChanged { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    let row = d.real.get_run(&run).await.unwrap();
    let probe = d.real.get_setting(STORAGE_PROBE_KEY).await.unwrap();
    assert_eq!(
        (
            replayed,
            row.status,
            row.log_bytes,
            probe.is_some(),
            d.store.health(),
            d.notices().last().cloned(),
        ),
        (
            vec![RunStatus::WaitingInput, RunStatus::Stopped],
            RunStatus::Stopped,
            7,
            true,
            WriteHealth::Ok,
            Some(StorageNotice::Health(WriteHealth::Ok)),
        )
    );
}

#[tokio::test]
async fn while_degraded_creates_should_fail_with_full_and_never_be_queued() {
    let d = Degradable::new();
    let (run, ws) = d.running().await;
    d.fill_disk(true);
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    d.fill_disk(false);
    // The disk has room again, but the queue isn't drained: creates still refuse.
    let refused = d.new_run(&ws).await.unwrap_err();
    assert_eq!((refused, d.store.queued().await), (StoreError::Full, 1));
    assert!(d.store.retry().await);
    assert!(d.new_run(&ws).await.is_ok());
}

#[tokio::test]
async fn a_create_that_hits_a_full_disk_should_fail_with_full_and_degrade() {
    let d = Degradable::new();
    let (_run, ws) = d.running().await;
    d.fill_disk(true);
    let err = d.new_run(&ws).await.unwrap_err();
    assert_eq!(
        (
            err,
            d.store.queued().await,
            matches!(d.store.health(), WriteHealth::Degraded { .. })
        ),
        (StoreError::Full, 0, true)
    );
}

#[tokio::test]
async fn the_status_machine_should_still_refuse_a_bad_transition_while_degraded() {
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    let refused = d
        .store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Running))
        .await;
    assert!(matches!(refused, Err(StoreError::InvalidTransition(_))));
    assert_eq!(d.store.queued().await, 1);
}

#[tokio::test]
async fn a_failed_log_append_should_degrade_and_mark_the_log_incomplete_once_writes_land() {
    let mut d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.store.log_failed(&run, &RunLogError::Full).await;
    let degraded = matches!(d.store.health(), WriteHealth::Degraded { .. });
    // Only the log failed: the database takes writes, so the retry lands it and the probe passes.
    assert!(d.store.retry().await);
    let row = d.real.get_run(&run).await.unwrap();
    assert_eq!(
        (degraded, row.log_state, d.store.health(), d.notices().len()),
        (true, LogState::Incomplete, WriteHealth::Ok, 2)
    );
}

#[tokio::test]
async fn past_the_cap_new_journal_events_should_be_dropped_and_other_writes_still_queued() {
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    for i in 0..QUEUE_CAP {
        d.store
            .set_setting(Actor::User, "k", JsonValue::Int(i as i64))
            .await
            .unwrap();
    }
    d.store
        .append_event(
            Actor::Agent,
            None,
            &run,
            sy_core::model::AdapterEvent::CommandRun {
                argv: vec!["ls".into()],
                exit_code: None,
            },
        )
        .await
        .unwrap();
    let after_event = d.store.queued().await;
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    assert_eq!(
        (after_event, d.store.queued().await),
        (QUEUE_CAP, QUEUE_CAP + 1)
    );
}

#[tokio::test]
async fn reads_should_keep_working_while_degraded() {
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    let page = d
        .store
        .events_since(0, 0, EventFilter::default())
        .await
        .unwrap();
    assert!(!page.events.is_empty());
}

// ---- AC-13: what queues, what refuses, and what the queue projects ----

#[tokio::test]
async fn a_refusal_while_ok_should_pass_through_and_never_degrade() {
    // covers: AC-13 (only storage failures degrade; a refusal would fail again)
    let mut d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.notices();

    let unknown = d
        .store
        .transition_run(Actor::User, &RunId::new(), STOPPED)
        .await;
    let backwards = d
        .store
        .transition_run(Actor::User, &run, RunTransition::to(RunStatus::Queued))
        .await;

    assert_eq!(unknown.unwrap_err(), StoreError::NotFound);
    assert!(matches!(backwards, Err(StoreError::InvalidTransition(_))));
    assert_eq!(
        (d.store.health(), d.store.queued().await, d.notices()),
        (WriteHealth::Ok, 0, Vec::new())
    );
}

#[tokio::test]
async fn many_failed_writes_should_degrade_once_with_a_message_free_of_row_data() {
    // covers: AC-13 (one storage.status per change; the message holds no row data)
    let mut d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.notices();
    d.fill_disk(true);

    d.store
        .set_setting(
            Actor::User,
            "agent.secret_hint",
            JsonValue::String("hunter2".into()),
        )
        .await
        .unwrap();
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    d.store.log_failed(&run, &RunLogError::Full).await;

    let health: Vec<_> = d
        .notices()
        .into_iter()
        .filter_map(|n| match n {
            StorageNotice::Health(h) => Some(h),
            StorageNotice::Unjournaled(_) => None,
        })
        .collect();
    let [WriteHealth::Degraded { message }] = health.as_slice() else {
        panic!("expected exactly one degraded notice, got {health:?}");
    };
    assert!(message.contains("disk is full"), "{message}");
    for row_data in ["hunter2", "agent.secret_hint", run.as_str()] {
        assert!(
            !message.contains(row_data),
            "message leaks {row_data}: {message}"
        );
    }
    assert_eq!(
        d.store.health(),
        WriteHealth::Degraded {
            message: message.clone()
        }
    );
}

#[tokio::test]
async fn a_same_status_report_while_degraded_should_queue_and_announce_nothing() {
    // covers: AC-13, AC-8 (same status is a no op, degraded or not)
    let mut d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    d.store
        .transition_run(
            Actor::User,
            &run,
            RunTransition::to(RunStatus::WaitingInput),
        )
        .await
        .unwrap();
    d.notices();

    let again = d
        .store
        .transition_run(
            Actor::User,
            &run,
            RunTransition::to(RunStatus::WaitingInput),
        )
        .await
        .unwrap();

    assert_eq!(
        (
            again.value.status,
            again.events.len(),
            d.store.queued().await,
            d.notices()
        ),
        (RunStatus::WaitingInput, 0, 1, Vec::new())
    );
}

#[tokio::test]
async fn queued_project_settings_should_report_the_revision_they_land_at() {
    // covers: AC-13 (settings queue; settings_revision still counts every change)
    let d = Degradable::new();
    let (_run, ws) = d.running().await;
    let project = d.store.get_workspace(&ws).await.unwrap().project_id;
    let base = d
        .store
        .set_project_setting(Actor::User, &project, "a", JsonValue::Int(1))
        .await
        .unwrap()
        .value;
    d.fill_disk(true);

    let second = d
        .store
        .set_project_setting(Actor::User, &project, "a", JsonValue::Int(2))
        .await
        .unwrap();
    let third = d
        .store
        .set_project_setting(Actor::User, &project, "b", JsonValue::Bool(true))
        .await
        .unwrap();
    let while_degraded = d.real.project_settings(&project).await.unwrap().revision;
    d.fill_disk(false);
    assert!(d.store.retry().await);
    let landed = d.real.project_settings(&project).await.unwrap();

    assert_eq!(
        (
            second.value,
            third.value,
            second.events.len(),
            while_degraded
        ),
        (base + 1, base + 2, 0, base)
    );
    assert_eq!(landed.revision, base + 2);
    assert_eq!(
        (landed.values.get("a"), landed.values.get("b")),
        (Some(&JsonValue::Int(2)), Some(&JsonValue::Bool(true)))
    );
}

#[tokio::test]
async fn a_queued_workspace_change_should_read_back_and_still_follow_the_lifecycle() {
    // covers: AC-13, AC-7 (the lifecycle applies in memory while degraded)
    let d = Degradable::new();
    let (run, ws) = d.running().await;
    // End the run first, so this test is only about the workspace lifecycle.
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    d.fill_disk(true);

    let discarded = d
        .store
        .set_workspace_state(Actor::User, &ws, WorkspaceChange::Discarded)
        .await
        .unwrap();
    let read_back = d.store.get_workspace(&ws).await.unwrap();
    let back_to_ready = d
        .store
        .set_workspace_state(Actor::User, &ws, WorkspaceChange::Ready)
        .await;
    let row_while_degraded = d.real.get_workspace(&ws).await.unwrap().state;
    d.fill_disk(false);
    assert!(d.store.retry().await);
    let landed = d.real.get_workspace(&ws).await.unwrap();

    assert_eq!(
        (
            discarded.value.state,
            discarded.events.len(),
            read_back.state
        ),
        (WorkspaceState::Discarded, 0, WorkspaceState::Discarded)
    );
    assert!(read_back.closed_at.is_some());
    assert!(matches!(
        back_to_ready,
        Err(StoreError::InvalidWorkspaceTransition(_))
    ));
    assert_eq!(row_while_degraded, WorkspaceState::Ready);
    assert_eq!(landed.state, WorkspaceState::Discarded);
}

#[tokio::test]
async fn queued_run_process_and_session_ref_should_read_back_then_land() {
    // covers: AC-13 (every set_* queues behind the failure)
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    let process = RunProcess {
        pid: 4242,
        pgid: 4240,
        pid_started_at: Some(1_700_000),
    };

    d.store.set_run_process(&run, process).await.unwrap();
    d.store.set_agent_session_ref(&run, "sess-1").await.unwrap();
    let projected = d.store.get_run(&run).await.unwrap();
    let real_while_degraded = d.real.get_run(&run).await.unwrap();
    d.fill_disk(false);
    assert!(d.store.retry().await);
    let landed = d.real.get_run(&run).await.unwrap();

    let fields =
        |r: &sy_core::model::Run| (r.pid, r.pgid, r.pid_started_at, r.agent_session_ref.clone());
    let expected = (
        Some(4242),
        Some(4240),
        Some(1_700_000),
        Some("sess-1".to_owned()),
    );
    assert_eq!(fields(&projected), expected);
    assert_eq!(fields(&real_while_degraded), (None, None, None, None));
    assert_eq!(fields(&landed), expected);
}

#[tokio::test]
async fn only_the_main_process_row_should_queue_while_degraded() {
    // covers: spec 0004 AC-1 (the main row joins the queue) and AC-7 (other
    // run_process writes are best effort)
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    let row = |pid, name: &str| sy_core::ports::TrackedProcess {
        pid,
        started_at: 1_000,
        name: name.to_owned(),
        first_seen_at: 5_000,
    };
    d.store
        .track_processes(&run, &[row(10, "agent")], true)
        .await
        .unwrap();
    let child = d
        .store
        .track_processes(&run, &[row(11, "node")], false)
        .await;
    let untrack = d.store.untrack_processes(&run, &[(10, 1_000)]).await;
    d.fill_disk(false);
    assert!(d.store.retry().await);
    let landed = d.real.list_tracked_processes().await.unwrap();
    assert_eq!(child.unwrap_err(), StoreError::Full);
    assert_eq!(untrack.unwrap_err(), StoreError::Full);
    assert_eq!(landed, vec![(run, row(10, "agent"))]);
}

#[tokio::test]
async fn a_replayed_change_the_store_now_refuses_should_be_dropped_and_the_rest_still_land() {
    // covers: AC-13 (a refused replay would fail forever, so it is dropped, not retried)
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    d.fill_disk(true);
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    d.store
        .set_setting(Actor::User, "after", JsonValue::Int(1))
        .await
        .unwrap();
    // Behind the queue's back the run already ended another way, so `stopped` is now a bad transition.
    d.real
        .transition_run(
            Actor::Engine,
            &run,
            RunTransition::end(
                RunStatus::Failed,
                RunEnd {
                    reason: EndReason::Exited,
                    exit_code: Some(1),
                    log_bytes: 3,
                },
            ),
        )
        .await
        .unwrap();
    d.fill_disk(false);

    let recovered = d.store.retry().await;

    assert_eq!(
        (
            recovered,
            d.store.queued().await,
            d.store.health(),
            d.real.get_run(&run).await.unwrap().status,
            d.real.get_setting("after").await.unwrap(),
        ),
        (
            true,
            0,
            WriteHealth::Ok,
            RunStatus::Failed,
            Some(JsonValue::Int(1)),
        )
    );
}

#[tokio::test]
async fn while_degraded_archive_delete_trim_and_recovery_writes_should_fail_with_full() {
    // covers: AC-13 (only transitions, set_* and append_event queue; every other write refuses)
    let d = Degradable::new();
    let (run, ws) = d.running().await;
    let task = d.store.get_workspace(&ws).await.unwrap().task_id;
    let project = d.store.get_workspace(&ws).await.unwrap().project_id;
    d.fill_disk(true);
    d.store
        .transition_run(Actor::User, &run, STOPPED)
        .await
        .unwrap();
    // The disk has room again, but the queue hasn't drained: these must still refuse.
    d.fill_disk(false);

    let refused = [
        d.store.archive_task(Actor::User, &task).await.map(drop),
        d.store
            .archive_project(Actor::User, &project)
            .await
            .map(drop),
        d.store.delete_task(&task).await.map(drop),
        d.store.delete_project(&project).await.map(drop),
        d.store
            .mark_log_trimmed(Actor::Engine, &run)
            .await
            .map(drop),
        d.store
            .mark_live_runs_lost(Actor::Engine, 1)
            .await
            .map(drop),
        d.store
            .discard_preparing_workspaces(Actor::Engine, 1)
            .await
            .map(drop),
        d.store.reconcile_log_bytes(Vec::new()).await.map(drop),
    ];

    assert_eq!(refused, std::array::from_fn(|_| Err(StoreError::Full)));
    assert_eq!(d.store.queued().await, 1);
    assert!(d.real.get_task(&task).await.unwrap().archived_at.is_none());
}

#[tokio::test]
async fn retry_while_ok_should_succeed_at_once_and_announce_nothing() {
    // covers: AC-13 (the engine's 5 second retry is harmless when nothing failed)
    let mut d = Degradable::new();
    d.running().await;
    d.notices();

    let ok = d.store.retry().await;

    assert_eq!(
        (
            ok,
            d.notices(),
            d.real.get_setting(STORAGE_PROBE_KEY).await.unwrap()
        ),
        (true, Vec::new(), None)
    );
}

#[tokio::test]
async fn approvals_while_degraded_should_refuse_creates_queue_decisions_then_land_in_order() {
    // covers: spec 0003 AC-9 (no row, no approval) and AC-17 (the run end's cancel queues)
    use sy_core::ids::ApprovalId;
    use sy_core::model::{
        ApprovalDecision, ApprovalKind, ApprovalStatus, DECIDED_IN_INBOX, NewApproval,
    };
    use sy_core::ports::{ApprovalFilter, ListQuery};
    let d = Degradable::new();
    let (run, _ws) = d.running().await;
    let new = |summary: &str| NewApproval {
        id: ApprovalId::new(),
        run_id: run.clone(),
        kind: ApprovalKind::Command,
        summary: summary.into(),
        detail: serde_json::json!({ "command": "ls", "raw": null }),
    };
    let first = d
        .store
        .create_approval(Actor::Agent, None, new("first"))
        .await
        .unwrap()
        .value;
    let second = d
        .store
        .create_approval(Actor::Agent, None, new("second"))
        .await
        .unwrap()
        .value;
    d.fill_disk(true);
    let refused = d
        .store
        .create_approval(Actor::Agent, None, new("refused"))
        .await;
    let decided = d
        .store
        .decide_approval(
            Actor::User,
            &first.id,
            ApprovalDecision::by_user(true, DECIDED_IN_INBOX, None),
        )
        .await
        .unwrap();
    let cancelled = d
        .store
        .cancel_pending_approvals(Actor::Engine, &run, None)
        .await
        .unwrap();
    let pending_now = d
        .store
        .list_approvals(
            ApprovalFilter {
                status: ApprovalStatus::Pending,
                run_id: Some(run.clone()),
            },
            ListQuery::default(),
        )
        .await
        .unwrap();
    let real_while_degraded = d.real.get_approval(&first.id).await.unwrap().status;
    d.fill_disk(false);
    assert!(d.store.retry().await);
    assert_eq!(
        (
            refused.unwrap_err(),
            (decided.value.status, decided.events.len()),
            cancelled
                .value
                .iter()
                .map(|a| (a.id.clone(), a.status))
                .collect::<Vec<_>>(),
            cancelled.events.len(),
            pending_now.items.len(),
            real_while_degraded,
            d.real.get_approval(&first.id).await.unwrap().status,
            d.real.get_approval(&second.id).await.unwrap().status,
        ),
        (
            StoreError::Full,
            (ApprovalStatus::Approved, 0),
            vec![(second.id.clone(), ApprovalStatus::Cancelled)],
            0,
            0,
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Cancelled,
        )
    );
}
