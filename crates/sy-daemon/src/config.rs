//! Engine settings. Defaults come from specs 0001 and 0003; the env overrides exist for tests.

use std::path::PathBuf;
use std::time::Duration;

/// Exit after this long with no agent running and no client connected.
const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(5 * 60);

/// Test seam: `SWITCHYARD_IDLE_EXIT_SECS=2` makes idle exit testable.
const IDLE_EXIT_ENV: &str = "SWITCHYARD_IDLE_EXIT_SECS";

/// Devtools only: where `sy-signal` and `sy-fake-agent` live, instead of
/// beside the engine's own executable.
#[cfg(feature = "devtools")]
const HELPER_DIR_ENV: &str = "SWITCHYARD_HELPER_DIR";

/// Devtools only: `SWITCHYARD_LOGIN_ENV=inherit` uses the engine's own
/// environment as the login environment, so tests don't depend on your shell.
#[cfg(feature = "devtools")]
const LOGIN_ENV_ENV: &str = "SWITCHYARD_LOGIN_ENV";

/// The hook helper's file name, beside the engine.
pub const SIGNAL_HELPER: &str = "sy-signal";

#[derive(Debug, Clone)]
pub struct Config {
    pub idle_exit: Duration,
    /// How long shutdown waits for clients to receive their last messages.
    pub flush_grace: Duration,
    /// The directory holding the helper binaries (spec 0003): `Contents/MacOS`
    /// in the bundle, `target/<profile>/` in development.
    pub helper_dir: PathBuf,
    /// Use the engine's own environment instead of asking the login shell
    /// (devtools `SWITCHYARD_LOGIN_ENV=inherit` only).
    pub inherit_login_env: bool,
}

impl Config {
    pub fn from_env() -> Self {
        let idle_exit = std::env::var(IDLE_EXIT_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(DEFAULT_IDLE_EXIT, Duration::from_secs);
        Self {
            idle_exit,
            flush_grace: Duration::from_secs(2),
            helper_dir: helper_dir(),
            inherit_login_env: inherit_login_env(),
        }
    }

    /// The absolute path of a helper binary.
    pub fn helper(&self, name: &str) -> String {
        self.helper_dir.join(name).to_string_lossy().into_owned()
    }
}

fn helper_dir() -> PathBuf {
    #[cfg(feature = "devtools")]
    if let Some(dir) = std::env::var_os(HELPER_DIR_ENV) {
        let dir = PathBuf::from(dir);
        return std::path::absolute(&dir).unwrap_or(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_default()
}

fn inherit_login_env() -> bool {
    #[cfg(feature = "devtools")]
    if std::env::var(LOGIN_ENV_ENV).is_ok_and(|v| v == "inherit") {
        return true;
    }
    false
}
