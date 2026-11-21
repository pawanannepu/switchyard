//! Engine settings. Defaults come from spec 0001; the env overrides exist for tests.

use std::time::Duration;

/// Exit after this long with no agent running and no client connected.
const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(5 * 60);

/// Test seam: `SWITCHYARD_IDLE_EXIT_SECS=2` makes idle exit testable.
const IDLE_EXIT_ENV: &str = "SWITCHYARD_IDLE_EXIT_SECS";

#[derive(Debug, Clone)]
pub struct Config {
    pub idle_exit: Duration,
    /// How long shutdown waits for clients to receive their last messages.
    pub flush_grace: Duration,
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
        }
    }
}
