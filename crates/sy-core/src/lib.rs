//! switchyard's domain and orchestration.
//!
//! The scaffold holds two pieces: the adapter registry (the seam a new agent
//! plugs into) and the run hub (terminal output fan out with replay).
//! Ports (`Store`, `Git`, `Pty`, `Secrets`, `Env`, `Clock`) are added here as
//! the features that need them land, and implemented in `sy-infra`.

pub mod adapters;
pub mod runs;
