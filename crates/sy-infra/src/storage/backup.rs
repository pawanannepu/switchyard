//! Crash safe backups (spec 0002, AC-2) and the file moves behind
//! `storage.recover` (AC-4).
//!
//! A backup is written with `VACUUM INTO` to
//! `backups/switchyard-v<user_version>-<unix ms>.db.tmp`, checked with
//! `PRAGMA quick_check`, and only then renamed to drop `.tmp`. So a file
//! without `.tmp` is always a complete, checked copy, and a `.tmp` left by a
//! crash is swept at the next start. The 3 newest backups are kept.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::{FILE_MODE, StoragePaths, ensure_private_dir, narrow, open_private_file};

/// How many backups are kept.
pub const KEEP_BACKUPS: usize = 3;

/// A startup backup is written when the newest one is older than this (24 hours).
pub const BACKUP_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

const PREFIX: &str = "switchyard-v";
const SUFFIX: &str = ".db";

/// One finished backup in `backups/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// The file name, `switchyard-v<version>-<unix ms>.db`.
    pub file_name: String,
    /// The schema version it was taken at.
    pub version: i64,
    /// When it was taken, Unix ms UTC (from its name).
    pub at_ms: i64,
    /// Its full path.
    pub path: PathBuf,
}

/// Why a backup could not be written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackupError {
    /// The copy failed (`VACUUM INTO`, or a file operation).
    #[error("the backup could not be written: {0}")]
    Write(String),
    /// The copy was written but failed its `quick_check`; it was removed.
    #[error("the backup failed its integrity check")]
    Check,
}

/// The name parts of `name`, when it is a finished backup's file name.
fn parse_name(name: &str) -> Option<(i64, i64)> {
    let rest = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let (version, at) = rest.split_once('-')?;
    Some((version.parse().ok()?, at.parse().ok()?))
}

/// Every finished backup (never a `.tmp`), newest first. A missing
/// `backups/` holds none.
pub fn list_backups(paths: &StoragePaths) -> io::Result<Vec<Backup>> {
    let entries = match fs::read_dir(&paths.backups) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if let Some((version, at_ms)) = parse_name(&file_name)
            && entry.file_type()?.is_file()
        {
            backups.push(Backup {
                path: entry.path(),
                file_name,
                version,
                at_ms,
            });
        }
    }
    backups.sort_by(|a, b| (b.at_ms, &b.file_name).cmp(&(a.at_ms, &a.file_name)));
    Ok(backups)
}

/// The newest backup's file name, for `engine.info.storage.latest_backup`.
pub fn latest_backup(paths: &StoragePaths) -> Option<String> {
    list_backups(paths)
        .ok()?
        .into_iter()
        .next()
        .map(|b| b.file_name)
}

/// True when `path` opens read only as a database and passes `quick_check`.
pub fn passes_quick_check(path: &Path) -> bool {
    let check = || -> rusqlite::Result<bool> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        quick_check(&conn)
    };
    check().unwrap_or(false)
}

/// `PRAGMA quick_check` on an open connection: true when it reports `ok`.
pub fn quick_check(conn: &Connection) -> rusqlite::Result<bool> {
    let result: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    Ok(result == "ok")
}

/// Write a backup of `conn`'s database taken at schema `version`: `VACUUM
/// INTO` a `0600` `.tmp`, `quick_check` it, rename it, then prune to the
/// [`KEEP_BACKUPS`] newest. Returns the new backup's file name.
pub fn write_backup(
    conn: &Connection,
    paths: &StoragePaths,
    version: i64,
    now_ms: i64,
) -> Result<String, BackupError> {
    let write = |e: &dyn std::fmt::Display| BackupError::Write(e.to_string());
    ensure_private_dir(&paths.backups).map_err(|e| write(&e))?;
    let file_name = format!("{PREFIX}{version}-{now_ms}{SUFFIX}");
    let done = paths.backups.join(&file_name);
    let tmp = paths.backups.join(format!("{file_name}.tmp"));
    // `VACUUM INTO` accepts an empty file, so creating it first fixes its mode.
    let _ = fs::remove_file(&tmp);
    open_private_file(&tmp).map_err(|e| write(&e))?;
    let target = tmp
        .to_str()
        .ok_or_else(|| write(&"a backup path that isn't UTF-8"))?;
    if let Err(err) = conn.execute("VACUUM INTO ?1", [target]) {
        let _ = fs::remove_file(&tmp);
        return Err(write(&err));
    }
    if !passes_quick_check(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(BackupError::Check);
    }
    narrow(&tmp, FILE_MODE).map_err(|e| write(&e))?;
    fs::rename(&tmp, &done).map_err(|e| write(&e))?;
    match prune_backups(paths, KEEP_BACKUPS) {
        Ok(0) => {}
        Ok(pruned) => tracing::info!(pruned, "old backups pruned"),
        Err(err) => tracing::warn!(%err, "could not prune old backups"),
    }
    tracing::info!(backup = %file_name, version, "backup written");
    Ok(file_name)
}

/// Remove all but the `keep` newest backups. Returns how many were removed.
pub fn prune_backups(paths: &StoragePaths, keep: usize) -> io::Result<usize> {
    let backups = list_backups(paths)?;
    let mut removed = 0;
    for old in backups.iter().skip(keep) {
        fs::remove_file(&old.path)?;
        removed += 1;
    }
    Ok(removed)
}

/// Whether startup should write a backup of a database at `version`: never
/// for a fresh one (`version` 0); before any pending migration; and when the
/// newest backup is older than [`BACKUP_MAX_AGE_MS`] (or there is none).
pub fn backup_due(
    paths: &StoragePaths,
    version: i64,
    migrations_pending: bool,
    now_ms: i64,
) -> bool {
    if version <= 0 {
        return false;
    }
    if migrations_pending {
        return true;
    }
    match list_backups(paths) {
        Ok(backups) => backups
            .first()
            .is_none_or(|newest| now_ms.saturating_sub(newest.at_ms) > BACKUP_MAX_AGE_MS),
        Err(err) => {
            tracing::warn!(%err, "could not list backups");
            false
        }
    }
}

/// What `storage.recover` set aside, for the engine log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAside {
    /// Each file or directory renamed to `.bad-<unix ms>`, in rename order.
    pub moved: Vec<PathBuf>,
}

/// Rename `path` to `<path>.bad-<now_ms>` when it exists, narrowing a file to
/// `0600` (so a file made unreadable stays yours to inspect). Never deletes.
fn set_aside_one(path: &Path, now_ms: i64, moved: &mut Vec<PathBuf>) -> io::Result<()> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let bad = path.with_file_name(format!("{name}.bad-{now_ms}"));
    match fs::rename(path, &bad) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    if fs::metadata(&bad)?.is_file() {
        narrow(&bad, FILE_MODE)?;
    }
    moved.push(bad);
    Ok(())
}

/// The first half of `storage.recover` (AC-4): rename the database's `-wal`
/// and `-shm` files, then the database, each to `<name>.bad-<now_ms>`, and
/// move `runs/` to `runs.bad-<now_ms>/`, then create an empty `runs/`.
/// Nothing is deleted.
pub fn set_aside(paths: &StoragePaths, now_ms: i64) -> io::Result<SetAside> {
    let mut moved = Vec::new();
    let db_name = paths
        .db
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("switchyard.db");
    for suffix in ["-wal", "-shm"] {
        set_aside_one(
            &paths.db.with_file_name(format!("{db_name}{suffix}")),
            now_ms,
            &mut moved,
        )?;
    }
    set_aside_one(&paths.db, now_ms, &mut moved)?;
    set_aside_one(&paths.runs, now_ms, &mut moved)?;
    ensure_private_dir(&paths.runs)?;
    Ok(SetAside { moved })
}

/// The newest backup that passes `quick_check`, trying older ones in turn.
pub fn newest_valid_backup(paths: &StoragePaths) -> io::Result<Option<Backup>> {
    Ok(list_backups(paths)?
        .into_iter()
        .find(|b| passes_quick_check(&b.path)))
}

/// Copy `backup` into place as the database, through `<db>.restore.tmp` plus
/// a rename, so a crash never leaves a half copied database.
pub fn restore(paths: &StoragePaths, backup: &Backup) -> io::Result<()> {
    let tmp = paths.db.with_extension("db.restore.tmp");
    let _ = fs::remove_file(&tmp);
    let result = (|| {
        let mut source = fs::File::open(&backup.path)?;
        let mut target = open_private_file(&tmp)?;
        target.set_len(0)?;
        io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        fs::rename(&tmp, &paths.db)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_should_parse_only_finished_backups() {
        assert_eq!(
            (
                parse_name("switchyard-v3-1700000000000.db"),
                parse_name("switchyard-v3-1700000000000.db.tmp"),
                parse_name("switchyard.db"),
                parse_name("switchyard-vx-1.db"),
            ),
            (Some((3, 1_700_000_000_000)), None, None, None)
        );
    }

    #[test]
    fn a_fresh_database_should_never_need_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StoragePaths::new(dir.path());
        assert!(!backup_due(&paths, 0, true, 0));
    }

    #[test]
    fn a_backup_should_be_due_when_none_exists_or_the_newest_is_over_a_day_old() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StoragePaths::new(dir.path());
        paths.ensure_dirs().unwrap();
        let none = backup_due(&paths, 1, false, 10);
        fs::write(paths.backups.join("switchyard-v1-1000.db"), b"").unwrap();
        let fresh = backup_due(&paths, 1, false, 1000 + BACKUP_MAX_AGE_MS);
        let stale = backup_due(&paths, 1, false, 1001 + BACKUP_MAX_AGE_MS);
        let pending = backup_due(&paths, 1, true, 1001);
        assert_eq!((none, fresh, stale, pending), (true, false, true, true));
    }
}
