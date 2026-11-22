//! Local storage (spec 0002): `switchyard.db` (SQLite, WAL), `runs/<run_id>.log`
//! and `backups/`, all under the engine's data dir.
//!
//! Files are created with explicit mode bits (`0600` files, `0700` dirs). The
//! process umask is never changed, because the agents the engine spawns
//! inherit it.

pub mod backup;
mod migrate;
mod run_log;
mod sqlite;

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use migrate::{MIGRATIONS, Migration};
pub use run_log::FileRunLog;
pub use sqlite::{Opened, SqliteStore, UNIQUE_INDEXES, open_connection};

/// Mode bits for every storage directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode bits for every storage file.
pub const FILE_MODE: u32 = 0o600;

/// Where storage lives inside the data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    /// The data dir itself.
    pub data_dir: PathBuf,
    /// `switchyard.db`.
    pub db: PathBuf,
    /// `runs/`, one log file per run.
    pub runs: PathBuf,
    /// `backups/`.
    pub backups: PathBuf,
}

impl StoragePaths {
    /// The storage paths under `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            db: data_dir.join("switchyard.db"),
            runs: data_dir.join("runs"),
            backups: data_dir.join("backups"),
        }
    }

    /// Create the data dir, `runs/` and `backups/` as `0700`, narrowing any
    /// that already exist with wider modes.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [&self.data_dir, &self.runs, &self.backups] {
            ensure_private_dir(dir)?;
        }
        Ok(())
    }
}

/// Remove every `backups/*.tmp` a crash left behind (AC-2, AC-6). Returns how
/// many were removed. A missing `backups/` is not an error.
pub fn sweep_backup_temps(paths: &StoragePaths) -> io::Result<usize> {
    let entries = match std::fs::read_dir(&paths.backups) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let is_tmp = entry.path().extension().is_some_and(|ext| ext == "tmp");
        if is_tmp && entry.file_type()?.is_file() {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Create `dir` (and parents) as `0700`, or narrow it to `0700` if it exists.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    narrow(dir, DIR_MODE)
}

/// Open `path` for reading and writing, creating it as `0600`, and narrow an
/// existing file to `0600`.
pub fn open_private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(FILE_MODE)
        .open(path)?;
    narrow(path, FILE_MODE)?;
    Ok(file)
}

fn narrow(path: &Path, mode: u32) -> io::Result<()> {
    let current = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if current != mode {
        std::fs::set_permissions(path, Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Why storage could not be opened. Each maps to a degraded storage status (AC-3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// The database was written by a newer engine (`newer_schema`).
    #[error("the database is schema version {found}, newer than this engine's {known}")]
    NewerSchema {
        /// `PRAGMA user_version` in the file.
        found: i64,
        /// The newest migration this engine knows.
        known: i64,
    },
    /// The file is not a database, or it failed its integrity check (`corrupt`).
    #[error("the database is damaged: {0}")]
    Corrupt(String),
    /// The file could not be opened or read (`unavailable`).
    #[error("the database could not be opened: {0}")]
    Unavailable(String),
    /// A migration failed and was rolled back (`unavailable`).
    #[error("migration {version} failed: {message}")]
    Migration {
        /// The migration's number.
        version: i64,
        /// The error text.
        message: String,
    },
}

impl OpenError {
    /// The schema version the failed open knows of, for `engine.info`:
    /// the file's version when it is newer, the version before a failed
    /// migration, and `None` when the file couldn't be read.
    pub const fn schema_version(&self) -> Option<i64> {
        match self {
            Self::NewerSchema { found, .. } => Some(*found),
            Self::Migration { version, .. } => Some(*version - 1),
            Self::Corrupt(_) | Self::Unavailable(_) => None,
        }
    }
}
