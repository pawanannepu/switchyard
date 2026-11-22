//! Reading a run's saved output (spec 0002, AC-12, `run.log_read`).

use crate::model::{LogState, Run};
use crate::ports::{RunLog, RunLogError};

/// One read of a run's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    /// Where `data` starts (the requested offset).
    pub offset: u64,
    /// The bytes read, at most the requested maximum.
    pub data: Vec<u8>,
    /// True when `offset + data.len()` reaches `log_bytes`. For a live run
    /// that only means "the end of the output so far".
    pub eof: bool,
    /// Bytes in the file now (for a live run, without its unflushed batch).
    pub log_bytes: u64,
}

/// Why a log read failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogReadError {
    /// The run's log was trimmed or deleted, or the file vanished.
    #[error("this run's log is no longer available")]
    Unavailable,
    /// Any other I/O failure.
    #[error(transparent)]
    Io(RunLogError),
}

/// Read up to `max` bytes of `run`'s log from `offset`.
///
/// `live_log_len` is the hub's count of bytes appended to the file for a live
/// run (`None` when the run isn't live). A run with no file yet (it never
/// wrote output: nothing appended while live, and `log_bytes` 0 once ended)
/// reads as empty with `eof`. A missing file for a run that did write output
/// means the file vanished, so it is [`LogReadError::Unavailable`], as is a
/// `trimmed` or `deleted` log. Blocks on the file, so async callers use
/// `spawn_blocking`.
pub fn read_run_log(
    log: &dyn RunLog,
    run: &Run,
    live_log_len: Option<u64>,
    offset: u64,
    max: usize,
) -> Result<LogChunk, LogReadError> {
    if matches!(run.log_state, LogState::Trimmed | LogState::Deleted) {
        return Err(LogReadError::Unavailable);
    }
    let empty = |log_bytes| LogChunk {
        offset,
        data: Vec::new(),
        eof: true,
        log_bytes,
    };
    let len = match log.len(&run.id) {
        Ok(len) => len,
        Err(RunLogError::NotFound) if live_log_len.unwrap_or(run.log_bytes) == 0 => {
            return Ok(empty(0));
        }
        Err(RunLogError::NotFound) => return Err(LogReadError::Unavailable),
        Err(err) => return Err(LogReadError::Io(err)),
    };
    if offset >= len {
        return Ok(empty(len));
    }
    // Read no further than `len`, so `eof` and `log_bytes` agree even while a live run grows.
    let want = usize::try_from(len - offset).map_or(max, |left| left.min(max));
    let data = match log.read(&run.id, offset, want) {
        Ok(data) => data,
        Err(RunLogError::NotFound) => return Err(LogReadError::Unavailable),
        Err(err) => return Err(LogReadError::Io(err)),
    };
    let end = offset + data.len() as u64;
    Ok(LogChunk {
        offset,
        data,
        eof: end >= len,
        log_bytes: len,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::ids::{RunId, WorkspaceId};
    use crate::model::{RunKind, RunStatus};

    /// Log files in memory.
    #[derive(Default)]
    struct MemLog(Mutex<HashMap<RunId, Vec<u8>>>);

    impl MemLog {
        fn with(run: &RunId, bytes: &[u8]) -> Self {
            let log = Self::default();
            log.0.lock().unwrap().insert(run.clone(), bytes.to_vec());
            log
        }
    }

    impl RunLog for MemLog {
        fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
            let mut files = self.0.lock().unwrap();
            let file = files.entry(run.clone()).or_default();
            let at = offset as usize;
            file.resize(file.len().max(at + bytes.len()), 0);
            file[at..at + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
        fn truncate(&self, run: &RunId, len: u64) -> Result<(), RunLogError> {
            if let Some(file) = self.0.lock().unwrap().get_mut(run) {
                file.truncate(len as usize);
            }
            Ok(())
        }
        fn read(&self, run: &RunId, offset: u64, max: usize) -> Result<Vec<u8>, RunLogError> {
            let files = self.0.lock().unwrap();
            let file = files.get(run).ok_or(RunLogError::NotFound)?;
            let start = (offset as usize).min(file.len());
            Ok(file[start..(start + max).min(file.len())].to_vec())
        }
        fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
            let files = self.0.lock().unwrap();
            files
                .get(run)
                .map(|f| f.len() as u64)
                .ok_or(RunLogError::NotFound)
        }
        fn delete(&self, run: &RunId) -> Result<(), RunLogError> {
            self.0.lock().unwrap().remove(run);
            Ok(())
        }
        fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
            Ok(Vec::new())
        }
    }

    fn run(status: RunStatus, log_bytes: u64, log_state: LogState) -> Run {
        Run {
            id: RunId::new(),
            workspace_id: WorkspaceId::new(),
            kind: RunKind::Agent,
            input: None,
            resumes_run_id: None,
            agent_session_ref: None,
            status,
            exit_code: None,
            end_reason: None,
            pid: None,
            pgid: None,
            pid_started_at: None,
            queued_at: 1,
            started_at: None,
            ended_at: None,
            log_bytes,
            log_state,
        }
    }

    #[test]
    fn read_should_return_a_page_and_say_when_it_reaches_the_end() {
        let r = run(RunStatus::Finished, 10, LogState::Present);
        let log = MemLog::with(&r.id, b"0123456789");
        let first = read_run_log(&log, &r, None, 0, 4).unwrap();
        let last = read_run_log(&log, &r, None, 6, 4).unwrap();
        assert_eq!(
            (first.data, first.eof, last.data, last.eof, last.log_bytes),
            (b"0123".to_vec(), false, b"6789".to_vec(), true, 10)
        );
    }

    #[test]
    fn offset_at_or_past_the_end_should_be_empty_with_eof() {
        let r = run(RunStatus::Finished, 3, LogState::Present);
        let log = MemLog::with(&r.id, b"abc");
        let at_end = read_run_log(&log, &r, None, 3, 10).unwrap();
        let past = read_run_log(&log, &r, None, 99, 10).unwrap();
        assert_eq!(
            (
                at_end.data.len(),
                at_end.eof,
                past.data.len(),
                past.eof,
                past.log_bytes
            ),
            (0, true, 0, true, 3)
        );
    }

    #[test]
    fn run_with_no_file_yet_should_read_empty_with_eof() {
        let queued = run(RunStatus::Queued, 0, LogState::Present);
        let live_silent = run(RunStatus::Running, 0, LogState::Present);
        let log = MemLog::default();
        assert_eq!(
            (
                read_run_log(&log, &queued, None, 0, 10).map(|c| (c.eof, c.log_bytes)),
                read_run_log(&log, &live_silent, Some(0), 0, 10).map(|c| c.eof),
            ),
            (Ok((true, 0)), Ok(true))
        );
    }

    #[test]
    fn vanished_file_of_a_run_that_wrote_output_should_be_unavailable() {
        let ended = run(RunStatus::Finished, 12, LogState::Present);
        let live = run(RunStatus::Running, 0, LogState::Present);
        let log = MemLog::default();
        assert_eq!(
            (
                read_run_log(&log, &ended, None, 0, 10),
                read_run_log(&log, &live, Some(5), 0, 10),
            ),
            (
                Err(LogReadError::Unavailable),
                Err(LogReadError::Unavailable)
            )
        );
    }

    #[test]
    fn trimmed_or_deleted_log_should_be_unavailable_even_if_a_file_is_left() {
        for state in [LogState::Trimmed, LogState::Deleted] {
            let r = run(RunStatus::Finished, 3, state);
            let log = MemLog::with(&r.id, b"abc");
            assert_eq!(
                read_run_log(&log, &r, None, 0, 10),
                Err(LogReadError::Unavailable),
                "{state}"
            );
        }
    }

    #[test]
    fn incomplete_log_should_read_its_good_prefix() {
        let r = run(RunStatus::Finished, 4, LogState::Incomplete);
        let log = MemLog::with(&r.id, b"good");
        let chunk = read_run_log(&log, &r, None, 0, 100).unwrap();
        assert_eq!((chunk.data, chunk.eof), (b"good".to_vec(), true));
    }
}
