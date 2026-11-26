//! switchyard's domain and orchestration.
//!
//! The domain model and its rules ([`ids`], [`model`]), the ports other crates
//! implement ([`ports`]: `Store`, `RunLog`, `Clock`), the adapter registry
//! (the seam a new agent plugs into) and the run hub (terminal output fan out
//! with replay, saved before it is delivered), plus the storage use cases:
//! startup [`recovery`], reading saved output ([`run_logs`]), and permanent
//! delete plus the log cap ([`retention`]), and the degraded write queue
//! that keeps the engine running on a full disk ([`write_queue`]), and agent
//! runs ([`agents`], spec 0003) with their own ports (`Pty`, `RuntimeDir`,
//! `LoginEnv`, `TokenSource`). More ports (`Git`, `Secrets`) arrive with the
//! features that need them, implemented in `sy-infra`.

pub mod adapters;
pub mod agents;
#[cfg(feature = "faults")]
pub mod faults;
pub mod ids;
pub mod model;
pub mod ports;
pub mod recovery;
pub mod retention;
pub mod run_logs;
pub mod runs;
pub mod write_queue;
