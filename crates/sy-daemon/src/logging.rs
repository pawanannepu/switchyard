//! Writes `engine.log` in the logs dir. Format, rotation and redaction are feature 6's.

use std::io::IsTerminal;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// `SWITCHYARD_LOG=debug` (or any `EnvFilter` directive) raises the level.
const FILTER_ENV: &str = "SWITCHYARD_LOG";

/// Keep the guard alive until exit so buffered lines are flushed.
pub fn init(logs_dir: &Path) -> anyhow::Result<WorkerGuard> {
    let (file, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(logs_dir, "engine.log"));
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer().with_ansi(false).with_writer(file);
    // When run by hand, mirror to the terminal. When the app launches the engine,
    // stderr is engine.stderr.log, which only catches output from before logging starts.
    let terminal_layer = std::io::stderr()
        .is_terminal()
        .then(|| fmt::layer().with_writer(std::io::stderr));
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(terminal_layer)
        .try_init()?;
    Ok(guard)
}
