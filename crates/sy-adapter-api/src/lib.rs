//! The one interface every agent plugs into.
//!
//! Placeholder: the real contract (detect, launch, stream, status, approvals,
//! stop) is decided by the spec for feature 4. Until then an adapter only names
//! itself, which is enough to prove the registration seam: adding an adapter
//! touches `crates/sy-adapters/<name>/` plus one line in `sy-daemon`, and
//! nothing in `sy-core`.

/// An agent adapter.
pub trait AgentAdapter: Send + Sync + 'static {
    /// Stable machine id, for example `claude`.
    fn id(&self) -> &'static str;

    /// Name shown to people, for example `Claude Code`.
    fn display_name(&self) -> &'static str;
}
