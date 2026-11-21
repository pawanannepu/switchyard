//! Writes `app.log` in the logs dir. Format, rotation and redaction are feature 6's.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init(logs_dir: &Path) -> Option<WorkerGuard> {
    let (file, guard) = tracing_appender::non_blocking(tracing_appender::rolling::never(logs_dir, "app.log"));
    let filter = EnvFilter::try_from_env("SWITCHYARD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer().with_ansi(false).with_writer(file);
    let debug_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr));
    tracing_subscriber::registry().with(filter).with(file_layer).with(debug_layer).try_init().ok()?;
    Some(guard)
}
