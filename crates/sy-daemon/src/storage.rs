//! The engine's storage mode (spec 0002, AC-3, AC-4, AC-13).
//!
//! Storage is either open, behind the degraded write queue, or failed to
//! open (**degraded storage mode**: `newer_schema`, `corrupt` or
//! `unavailable`). In the failed mode the engine still serves the stable
//! methods and `storage.recover`, and nothing writes to the database file
//! except `storage.recover`, which sets the bad files aside and opens again
//! with no engine restart.

use std::sync::{Arc, PoisonError, RwLock};

use sy_core::model::Event;
use sy_core::ports::{Clock, RunLog, Store};
use sy_core::recovery;
use sy_core::write_queue::{QueuedStore, StorageNotice, WriteHealth};
use sy_infra::storage::{OpenError, Opened, SqliteStore, StoragePaths, backup};
use sy_protocol::{RecoverAction, StorageInfo, StorageStatus};
use tokio::sync::mpsc;

/// Storage that is open.
#[derive(Clone)]
struct OpenStorage {
    store: Arc<QueuedStore>,
    sqlite: Arc<SqliteStore>,
    schema_version: i64,
}

/// Why storage didn't open.
#[derive(Debug, Clone)]
struct Failure {
    status: StorageStatus,
    message: String,
    schema_version: Option<i64>,
}

impl From<&OpenError> for Failure {
    fn from(err: &OpenError) -> Self {
        let status = match err {
            OpenError::NewerSchema { .. } => StorageStatus::NewerSchema,
            OpenError::Corrupt(_) => StorageStatus::Corrupt,
            OpenError::Unavailable(_) | OpenError::Migration { .. } => StorageStatus::Unavailable,
        };
        Self {
            status,
            message: err.to_string(),
            schema_version: err.schema_version(),
        }
    }
}

#[derive(Clone)]
enum Mode {
    Open(OpenStorage),
    Failed(Failure),
}

/// Why `storage.recover` refused or failed.
#[derive(Debug)]
pub enum RecoverError {
    /// Storage is `ok` or `degraded_writes`.
    NotNeeded,
    /// `restore_backup`, but no backup passes `quick_check`.
    BackupMissing,
    /// Moving the files failed.
    Io(String),
}

/// The engine's storage: how to open it, and where it stands.
pub struct Storage {
    paths: StoragePaths,
    clock: Arc<dyn Clock>,
    run_log: Arc<dyn RunLog>,
    /// One sender for every store opened, so the broadcaster has one ordered channel.
    committed: mpsc::UnboundedSender<Event>,
    notices: mpsc::UnboundedSender<StorageNotice>,
    mode: RwLock<Mode>,
    /// One recover at a time.
    recovering: tokio::sync::Mutex<()>,
}

impl Storage {
    /// Open storage at startup (AC-2, AC-3). Never fails: a failed open is
    /// degraded storage mode, logged and reported by `engine.info`.
    pub fn open(
        paths: StoragePaths,
        clock: Arc<dyn Clock>,
        run_log: Arc<dyn RunLog>,
        committed: mpsc::UnboundedSender<Event>,
        notices: mpsc::UnboundedSender<StorageNotice>,
    ) -> Arc<Self> {
        let storage = Self {
            paths,
            clock,
            run_log,
            committed,
            notices,
            mode: RwLock::new(Mode::Failed(Failure {
                status: StorageStatus::Unavailable,
                message: "not opened yet".into(),
                schema_version: None,
            })),
            recovering: tokio::sync::Mutex::new(()),
        };
        let restore_tmp = storage.paths.db.with_extension("db.restore.tmp");
        let _ = std::fs::remove_file(restore_tmp);
        let mode = storage.open_mode();
        storage.set(mode);
        Arc::new(storage)
    }

    fn open_mode(&self) -> Mode {
        match SqliteStore::open(&self.paths, Arc::clone(&self.clock), self.committed.clone()) {
            Ok(opened) => Mode::Open(self.wrap(opened)),
            Err(err) => {
                tracing::error!(%err, "storage did not open; running in degraded storage mode");
                Mode::Failed(Failure::from(&err))
            }
        }
    }

    fn wrap(&self, opened: Opened) -> OpenStorage {
        let Opened {
            store,
            schema_version,
            backup,
        } = opened;
        tracing::info!(schema_version, backup = ?backup, "storage open");
        let sqlite = Arc::new(store);
        let inner = crate::faults::store(Arc::clone(&sqlite) as Arc<dyn Store>);
        OpenStorage {
            store: QueuedStore::new(inner, Arc::clone(&self.clock), self.notices.clone()),
            sqlite,
            schema_version,
        }
    }

    fn mode(&self) -> Mode {
        self.mode
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, mode: Mode) {
        *self.mode.write().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// The store, when storage is open (`ok` or `degraded_writes`).
    pub fn store(&self) -> Option<Arc<dyn Store>> {
        self.queued().map(|q| q as Arc<dyn Store>)
    }

    /// The degraded write queue, when storage is open.
    pub fn queued(&self) -> Option<Arc<QueuedStore>> {
        match self.mode() {
            Mode::Open(open) => Some(open.store),
            Mode::Failed(_) => None,
        }
    }

    /// True in degraded storage mode (a failed open), when only the stable
    /// methods and `storage.recover` answer.
    pub fn is_failed(&self) -> bool {
        matches!(self.mode(), Mode::Failed(_))
    }

    /// Storage health, for `engine.info` and `storage.status`.
    pub fn info(&self) -> StorageInfo {
        let latest_backup = backup::latest_backup(&self.paths);
        match self.mode() {
            Mode::Open(open) => {
                let (status, message) = match open.store.health() {
                    WriteHealth::Ok => (StorageStatus::Ok, String::new()),
                    WriteHealth::Degraded { message } => (StorageStatus::DegradedWrites, message),
                };
                StorageInfo {
                    status,
                    message,
                    schema_version: Some(open.schema_version),
                    latest_backup,
                }
            }
            Mode::Failed(failure) => StorageInfo {
                status: failure.status,
                message: failure.message,
                schema_version: failure.schema_version,
                latest_backup,
            },
        }
    }

    /// Run `PRAGMA wal_checkpoint(TRUNCATE)`, at clean shutdown (AC-1).
    pub async fn checkpoint(&self) {
        if let Mode::Open(open) = self.mode()
            && let Err(err) = open.sqlite.checkpoint().await
        {
            tracing::warn!(%err, "WAL checkpoint at shutdown failed");
        }
    }

    /// `storage.recover` (AC-4): only in degraded storage mode. Picks the
    /// backup first (so nothing moves when there is none), sets the bad
    /// files and `runs/` aside, puts the backup in place or starts empty,
    /// then opens storage again and reconciles it. Returns the new health.
    pub async fn recover(&self, action: RecoverAction) -> Result<StorageInfo, RecoverError> {
        let _one = self.recovering.lock().await;
        if !self.is_failed() {
            return Err(RecoverError::NotNeeded);
        }
        let paths = self.paths.clone();
        let now = self.clock.now_ms();
        let moved = tokio::task::spawn_blocking(move || {
            let chosen = match action {
                RecoverAction::RestoreBackup => match backup::newest_valid_backup(&paths) {
                    Ok(Some(chosen)) => Some(chosen),
                    Ok(None) => return Err(RecoverError::BackupMissing),
                    Err(err) => return Err(RecoverError::Io(err.to_string())),
                },
                RecoverAction::StartFresh => None,
            };
            let aside =
                backup::set_aside(&paths, now).map_err(|e| RecoverError::Io(e.to_string()))?;
            if let Some(chosen) = &chosen {
                backup::restore(&paths, chosen).map_err(|e| RecoverError::Io(e.to_string()))?;
            }
            Ok((aside, chosen.map(|c| c.file_name)))
        })
        .await
        .map_err(|e| RecoverError::Io(e.to_string()))??;
        tracing::info!(
            ?action,
            set_aside = ?moved.0.moved,
            restored = ?moved.1,
            "storage recovered: bad files set aside"
        );

        let mode = {
            // The open sequence is blocking (checks, backup, migrations).
            let (paths, clock, committed) = (
                self.paths.clone(),
                Arc::clone(&self.clock),
                self.committed.clone(),
            );
            tokio::task::spawn_blocking(move || SqliteStore::open(&paths, clock, committed))
                .await
                .map_err(|e| RecoverError::Io(e.to_string()))?
        };
        match mode {
            Ok(opened) => {
                let open = self.wrap(opened);
                // `runs/` is new and empty, so this marks lost runs and flags
                // their logs `deleted`, and sweeps nothing.
                match recovery::recover_at_startup(
                    open.store.as_ref(),
                    Arc::clone(&self.run_log),
                    now,
                )
                .await
                {
                    Ok(done) => tracing::info!(
                        lost_runs = done.lost,
                        discarded_workspaces = done.discarded,
                        deleted_logs = done.deleted_logs,
                        "recovered storage reconciled"
                    ),
                    Err(err) => tracing::warn!(%err, "reconciling recovered storage failed"),
                }
                self.set(Mode::Open(open));
            }
            Err(err) => {
                tracing::error!(%err, "storage still did not open after recover");
                self.set(Mode::Failed(Failure::from(&err)));
            }
        }
        Ok(self.info())
    }
}
