//! Single instance: an exclusive, non blocking lock on `engine.lock`.
//!
//! `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on Unix. The kernel drops it
//! when the process dies, so there is no PID file and nothing goes stale.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub enum Outcome {
    /// Keep this file open for the engine's whole life.
    Acquired(File),
    AlreadyRunning,
}

pub fn acquire(path: &Path) -> io::Result<Outcome> {
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Outcome::Acquired(file)),
        Err(TryLockError::WouldBlock) => Ok(Outcome::AlreadyRunning),
        Err(TryLockError::Error(err)) => Err(err),
    }
}
