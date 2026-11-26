//! Launch dirs: `<data dir>/agent-runtime/<run_id>/`, private to you.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sy_core::agents::ports::RuntimeDir;
use sy_core::ids::RunId;

/// The launch dirs under `base` (`<data dir>/agent-runtime`).
#[derive(Debug, Clone)]
pub struct FsRuntimeDir {
    base: PathBuf,
}

impl FsRuntimeDir {
    /// Launch dirs under `<data_dir>/agent-runtime`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            base: data_dir.join("agent-runtime"),
        }
    }

    /// Create `agent-runtime/` itself (`0700`), where probes run.
    pub fn ensure_root(&self) -> std::io::Result<()> {
        private_dir(&self.base, true)
    }

    fn dir(&self, run: &RunId) -> PathBuf {
        self.base.join(run.as_str())
    }
}

fn private_dir(path: &Path, recursive: bool) -> std::io::Result<()> {
    DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(path)
}

impl RuntimeDir for FsRuntimeDir {
    fn root(&self) -> String {
        self.base.to_string_lossy().into_owned()
    }

    fn path(&self, run: &RunId) -> String {
        self.dir(run).to_string_lossy().into_owned()
    }

    fn create(&self, run: &RunId) -> std::io::Result<String> {
        private_dir(&self.base, true)?;
        private_dir(&self.dir(run), false)?;
        Ok(self.path(run))
    }

    fn write_file(&self, run: &RunId, rel_path: &str, contents: &[u8]) -> std::io::Result<()> {
        let path = self.dir(run).join(rel_path);
        if let Some(parent) = path.parent() {
            private_dir(parent, true)?;
        }
        // `create_new` is O_CREAT | O_EXCL: it never follows a symlink there.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(contents)?;
        file.sync_all()
    }

    fn remove(&self, run: &RunId) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.dir(run)) {
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn sweep(&self) -> std::io::Result<usize> {
        let entries = match std::fs::read_dir(&self.base) {
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(0),
            other => other?,
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
            removed += 1;
        }
        Ok(removed)
    }
}
