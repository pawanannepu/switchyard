//! Where the engine's data, socket, lock and logs live.
//!
//! `SWITCHYARD_DATA_DIR` overrides the data dir (tests, running the engine by
//! hand); logs then go to `<data dir>/logs` so a test never writes to the
//! real log folder.

use std::io;
use std::path::{Path, PathBuf};

/// The bundle id. Keychain items, notification permission and the data folder hang off it.
pub const BUNDLE_ID: &str = "dev.switchyard.app";

/// Environment variable that overrides the data dir.
pub const DATA_DIR_ENV: &str = "SWITCHYARD_DATA_DIR";

/// Longest socket path the OS accepts, in bytes, without the trailing NUL.
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH_BYTES: usize = 103;
#[cfg(not(target_os = "macos"))]
const MAX_SOCKET_PATH_BYTES: usize = 107;

/// Why the engine paths could not be resolved or prepared.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The OS reported no home or data directory.
    #[error("could not find the home or data directory for this user")]
    NoHome,
    /// A directory exists but another user owns it, or others can read it.
    #[error("{path} is not a private directory owned by this user")]
    NotPrivate {
        /// The directory that failed the check.
        path: PathBuf,
    },
    /// A file system call failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being worked on.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

/// Every path the engine and its clients agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnginePaths {
    /// The engine's data dir (database, run output).
    pub data_dir: PathBuf,
    /// Where `engine.log` and `engine.stderr.log` go.
    pub logs_dir: PathBuf,
    /// The engine socket (`engine.sock`).
    pub socket: PathBuf,
    /// The single instance lock (`engine.lock`).
    pub lock: PathBuf,
    /// True when the socket lives in the `/tmp/switchyard-<uid>` fallback dir.
    pub socket_in_fallback_dir: bool,
}

impl EnginePaths {
    /// Resolve from `SWITCHYARD_DATA_DIR` or the OS default.
    pub fn resolve() -> Result<Self, PathError> {
        match std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
            Some(dir) => Ok(Self::for_data_dir(PathBuf::from(dir))),
            None => {
                let data_dir = dirs::data_dir().ok_or(PathError::NoHome)?.join(BUNDLE_ID);
                let logs_dir = default_logs_dir(&data_dir)?;
                Ok(Self::build(data_dir, logs_dir))
            }
        }
    }

    /// Paths for an explicit data dir, with logs inside it.
    pub fn for_data_dir(data_dir: PathBuf) -> Self {
        let logs_dir = data_dir.join("logs");
        Self::build(data_dir, logs_dir)
    }

    fn build(data_dir: PathBuf, logs_dir: PathBuf) -> Self {
        let preferred = data_dir.join("engine.sock");
        let (socket, socket_in_fallback_dir) = if path_len(&preferred) <= MAX_SOCKET_PATH_BYTES {
            (preferred, false)
        } else {
            (fallback_socket_dir().join("engine.sock"), true)
        };
        let lock = data_dir.join("engine.lock");
        Self {
            data_dir,
            logs_dir,
            socket,
            lock,
            socket_in_fallback_dir,
        }
    }

    /// Create the data dir, logs dir and (if used) the fallback socket dir, all `0700`.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        ensure_private_dir(&self.data_dir)?;
        ensure_private_dir(&self.logs_dir)?;
        if self.socket_in_fallback_dir
            && let Some(parent) = self.socket.parent()
        {
            ensure_private_dir(parent)?;
        }
        Ok(())
    }
}

fn path_len(path: &Path) -> usize {
    path.as_os_str().len()
}

#[cfg(target_os = "macos")]
fn default_logs_dir(_data_dir: &Path) -> Result<PathBuf, PathError> {
    Ok(dirs::home_dir()
        .ok_or(PathError::NoHome)?
        .join("Library/Logs")
        .join(BUNDLE_ID))
}

#[cfg(not(target_os = "macos"))]
fn default_logs_dir(data_dir: &Path) -> Result<PathBuf, PathError> {
    Ok(data_dir.join("logs"))
}

#[cfg(unix)]
fn fallback_socket_dir() -> PathBuf {
    PathBuf::from(format!(
        "/tmp/switchyard-{}",
        crate::transport::current_uid()
    ))
}

#[cfg(not(unix))]
fn fallback_socket_dir() -> PathBuf {
    std::env::temp_dir().join("switchyard")
}

/// Create `path` (and parents) and make it `0700`. An existing dir must be owned by this user.
#[cfg(unix)]
pub fn ensure_private_dir(path: &Path) -> Result<(), PathError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let io_err = |source| PathError::Io {
        path: path.to_path_buf(),
        source,
    };
    std::fs::create_dir_all(path).map_err(io_err)?;
    let meta = std::fs::symlink_metadata(path).map_err(io_err)?;
    if !meta.is_dir() || meta.uid() != crate::transport::current_uid() {
        return Err(PathError::NotPrivate {
            path: path.to_path_buf(),
        });
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_err)?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_private_dir(path: &Path) -> Result<(), PathError> {
    std::fs::create_dir_all(path).map_err(|source| PathError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_data_dir_should_keep_socket_inside_it() {
        let paths = EnginePaths::for_data_dir(PathBuf::from("/tmp/sy-abc"));
        assert_eq!(paths.socket, PathBuf::from("/tmp/sy-abc/engine.sock"));
    }

    #[test]
    fn long_data_dir_should_fall_back_to_tmp_socket() {
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(120)));
        let paths = EnginePaths::for_data_dir(long);
        assert!(
            paths.socket_in_fallback_dir
                && paths
                    .socket
                    .to_string_lossy()
                    .starts_with("/tmp/switchyard-")
        );
    }

    #[test]
    fn lock_should_stay_in_data_dir_even_with_fallback_socket() {
        let long = PathBuf::from(format!("/tmp/{}", "y".repeat(120)));
        let paths = EnginePaths::for_data_dir(long.clone());
        assert_eq!(paths.lock, long.join("engine.lock"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_should_set_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
