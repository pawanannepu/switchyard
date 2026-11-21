//! The Claude Code adapter. Built out in feature 8 against the feature 4 contract.

use sy_adapter_api::AgentAdapter;

/// Drives Claude Code through its official CLI. Never reads its login tokens.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
}
