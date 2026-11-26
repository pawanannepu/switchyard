//! The one interface every agent plugs into (spec 0003).
//!
//! An adapter is a pure translator. [`AgentAdapter`] describes an agent
//! (its programs, its detection probes) and turns a [`LaunchRequest`] into a
//! [`LaunchPlan`] plus a per run [`RunDecoder`]. The decoder turns what the
//! engine saw (terminal output, hook calls, ticks, the exit) into
//! [`Report`]s. The engine does every bit of I/O: it spawns the processes,
//! writes the files and owns the sockets. Nothing in this crate, or in any
//! adapter, opens a file, starts a process, reads the environment or touches
//! the network, which is what makes "an adapter never reads login tokens"
//! true by construction (and checked by `pnpm boundaries:check`).
//!
//! Adding an agent touches `crates/sy-adapters/<name>/` plus one
//! registration line in `sy-daemon`, and nothing in `sy-core`.

mod b64;
mod contract;
mod hook;
mod scanner;

pub use contract::*;
pub use hook::{HookCommand, hook_command, shell_quote};
pub use scanner::OutputScanner;
/// The JSON library the contract's types use (`LaunchRequest::options`,
/// `ApprovalRequest::raw`), re-exported so callers build those values with
/// the same version.
pub use serde_json;

/// The contract version this crate defines. Bumped on any breaking change to
/// the traits or types below.
pub const CONTRACT_VERSION: u32 = 1;
