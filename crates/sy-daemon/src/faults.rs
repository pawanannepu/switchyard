//! Storage fault injection for the socket tests (spec 0002, *Disk full*).
//!
//! Only a `devtools` build started with `SWITCHYARD_DEV_FAULTS=<dir>` wraps
//! its store and run log in sy-core's failing doubles. While `<dir>/store`
//! exists every database write fails with "disk full"; while `<dir>/run_log`
//! exists every run log append fails part way. A release build, or a
//! devtools build without the variable, uses the real ones untouched.

use std::sync::Arc;

use sy_core::ports::{RunLog, Store};

#[cfg(feature = "devtools")]
const FAULTS_ENV: &str = "SWITCHYARD_DEV_FAULTS";

#[cfg(feature = "devtools")]
fn switch(name: &str) -> Option<sy_core::faults::Switch> {
    let dir = std::env::var_os(FAULTS_ENV)?;
    let path = std::path::PathBuf::from(dir).join(name);
    tracing::warn!(path = %path.display(), "storage fault injection is on");
    Some(Arc::new(move || path.exists()))
}

/// The store the engine uses.
pub fn store(inner: Arc<dyn Store>) -> Arc<dyn Store> {
    #[cfg(feature = "devtools")]
    if let Some(full) = switch("store") {
        return Arc::new(sy_core::faults::FaultyStore::new(inner, full));
    }
    inner
}

/// The run log the engine uses.
pub fn run_log(inner: Arc<dyn RunLog>) -> Arc<dyn RunLog> {
    #[cfg(feature = "devtools")]
    if let Some(full) = switch("run_log") {
        return Arc::new(sy_core::faults::FaultyRunLog::new(inner, full));
    }
    inner
}
