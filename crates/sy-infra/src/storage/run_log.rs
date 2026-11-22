//! `FileRunLog`: one `0600` file per run in `runs/`, written with `pwrite` at
//! the exact offset (no fsync; persist before broadcast needs the bytes in
//! the OS, which survives an engine `kill -9`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sy_core::ids::RunId;
use sy_core::ports::{RunLog, RunLogError};

use super::open_private_file;

/// Open append handles kept at once. Past this the cache is emptied, so a long
/// lived engine never holds handles for runs that ended long ago.
const MAX_OPEN: usize = 128;

/// Run output logs in a directory.
pub struct FileRunLog {
    dir: PathBuf,
    open: Mutex<HashMap<RunId, Arc<File>>>,
}

fn map_err(err: &io::Error) -> RunLogError {
    match err.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => RunLogError::Full,
        ErrorKind::NotFound => RunLogError::NotFound,
        _ => RunLogError::Io(err.to_string()),
    }
}

impl FileRunLog {
    /// Logs in `dir`, which must exist (see `StoragePaths::ensure_dirs`).
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            open: Mutex::new(HashMap::new()),
        }
    }

    /// The file for `run`.
    pub fn path(&self, run: &RunId) -> PathBuf {
        self.dir.join(format!("{run}.log"))
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<RunId, Arc<File>>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn writer(&self, run: &RunId) -> io::Result<Arc<File>> {
        if let Some(file) = self.cache().get(run) {
            return Ok(Arc::clone(file));
        }
        let file = Arc::new(open_private_file(&self.path(run))?);
        let mut cache = self.cache();
        if cache.len() >= MAX_OPEN {
            cache.clear();
        }
        cache.insert(run.clone(), Arc::clone(&file));
        Ok(file)
    }
}

impl RunLog for FileRunLog {
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
        let file = self.writer(run).map_err(|e| map_err(&e))?;
        file.write_all_at(bytes, offset).map_err(|e| map_err(&e))
    }

    fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError> {
        let file = self.writer(run).map_err(|e| map_err(&e))?;
        file.set_len(len).map_err(|e| map_err(&e))
    }

    fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError> {
        let file = File::open(self.path(run)).map_err(|e| map_err(&e))?;
        let mut buf = vec![0; max];
        let mut filled = 0;
        while filled < max {
            match file.read_at(&mut buf[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(map_err(&e)),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
        std::fs::metadata(self.path(run))
            .map(|m| m.len())
            .map_err(|e| map_err(&e))
    }

    fn delete(&self, run: &RunId) -> Result<(), RunLogError> {
        self.cache().remove(run);
        match std::fs::remove_file(self.path(run)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(map_err(&e)),
        }
    }

    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
        let mut logs = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(|e| map_err(&e))? {
            let entry = entry.map_err(|e| map_err(&e))?;
            let name = entry.file_name();
            let Some(run) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".log"))
                .and_then(|n| RunId::parse(n).ok())
            else {
                continue;
            };
            let len = entry.metadata().map_err(|e| map_err(&e))?.len();
            logs.push((run, len));
        }
        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn log() -> (tempfile::TempDir, FileRunLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = FileRunLog::new(dir.path());
        (dir, log)
    }

    #[test]
    fn append_should_write_at_the_offset_and_read_back() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"hello ").unwrap();
        log.append(&run, 6, b"world").unwrap();
        assert_eq!(log.read(&run, 0, 64).unwrap(), b"hello world");
    }

    #[test]
    fn retried_append_should_not_shift_bytes() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"abc").unwrap();
        log.append(&run, 3, b"def").unwrap();
        log.append(&run, 3, b"def").unwrap();
        assert_eq!(
            (log.read(&run, 0, 64).unwrap(), log.len(&run).unwrap()),
            (b"abcdef".to_vec(), 6)
        );
    }

    #[test]
    fn new_log_file_should_be_0600() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"x").unwrap();
        let mode = std::fs::metadata(log.path(&run))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn read_should_cap_at_max_and_be_empty_past_the_end() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"0123456789").unwrap();
        assert_eq!(
            (
                log.read(&run, 2, 3).unwrap(),
                log.read(&run, 10, 3).unwrap()
            ),
            (b"234".to_vec(), Vec::new())
        );
    }

    #[test]
    fn truncate_should_cut_back_to_the_last_good_byte() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"good-partial").unwrap();
        log.truncate(&run, 4).unwrap();
        assert_eq!(log.read(&run, 0, 64).unwrap(), b"good");
    }

    #[test]
    fn missing_file_should_be_not_found_and_delete_should_not_mind() {
        let (_dir, log) = log();
        let run = RunId::new();
        assert_eq!(
            (log.len(&run), log.read(&run, 0, 1), log.delete(&run)),
            (
                Err(RunLogError::NotFound),
                Err(RunLogError::NotFound),
                Ok(())
            )
        );
    }

    #[test]
    fn delete_should_remove_the_file() {
        let (_dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"x").unwrap();
        log.delete(&run).unwrap();
        assert!(!log.path(&run).exists());
    }

    #[test]
    fn list_should_return_run_logs_with_sizes_and_skip_other_files() {
        let (dir, log) = log();
        let run = RunId::new();
        log.append(&run, 0, b"12345").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::write(dir.path().join("run-1.log"), "x").unwrap();
        assert_eq!(log.list().unwrap(), vec![(run, 5)]);
    }
}
