# Agent CLI integration research (as of 2026-11-22)

## Status of this research

- Budget used: 5 of 5 web searches; 8 of 8 page fetch attempts.
- Every page fetch failed with "Unable to verify if domain ... is safe to fetch" (code.claude.com, docs.claude.com, github.com, opencode.ai, agentclientprotocol.com). No official page was opened.
- All findings below come from search-result snippets (secondary, undated, and in places conflicting). Treat them as leads to verify, not as confirmed facts.
- Confirmed (opened) URLs: NONE.
- Labels: SNIPPET = seen in a search snippet, not opened. UNCONFIRMED = no source found, or sources conflict.

## 1. Claude Code (Anthropic)

- Hook events: Stop, Notification, PreToolUse, SessionStart and others are described in the hooks reference. SNIPPET (code.claude.com/docs/en/hooks, seen in search only).
- Blocking / approval: PreToolUse returns `hookSpecificOutput.permissionDecision` with values `allow`, `deny`, `ask`, `defer`. `defer` requires non-interactive `-p` mode. Multiple matching hooks: most restrictive wins (deny, defer, ask, allow). A deny holds even under bypassPermissions. SNIPPET. Deprecated top-level `decision`/`reason` for PreToolUse map approve->allow and block->deny. SNIPPET.
- Stop: exit code 2 or top-level `decision: "block"` keeps the agent running. SNIPPET.
- Per-launch hooks file: UNCONFIRMED (a `--settings` flag is expected but no source was opened).
- Session id: UNCONFIRMED in hook payload (`session_id` expected; not verified).
- Resume flags: UNCONFIRMED (`--resume` / `--continue` expected; not verified).
- ACP: a registry snippet lists `claude-acp` ("Claude Agent") with npm package `@agentclientprotocol/claude-agent-acp`, versions 0.63.0 to 0.75.1. SNIPPET (cdn.agentclientprotocol.com registry snapshots). This is a third-party wrapper, not native. The name `@zed-industries/claude-code-acp` (older memory) was not found in any source here. Conflicting: UNCONFIRMED.
- Headless structured output: `-p` with `--output-format stream-json` expected; UNCONFIRMED. `-p` with `defer` is confirmed by the PreToolUse snippet above.
- Auth status command: UNCONFIRMED (`claude auth status` not verified).
- Version: UNCONFIRMED (`claude --version` not verified).

## 2. OpenAI Codex CLI

- Notify: config.toml key `notify` (array<string>), runs a command with a JSON payload. When unset, disabled. SNIPPET (developers.openai.com/codex/config-reference).
- Notify payload event type and session id field: UNCONFIRMED (a community cheat sheet mentions `agent-turn-complete` and `thread-id`; not verified).
- Hooks: enabled by `features.hooks`; loaded from `hooks.json` or inline `[hooks]`. SNIPPET (developers.openai.com/codex/hooks, config reference).
- Events: SessionStart (matcher `startup|resume`) and PermissionRequest. A community issue says there are no SessionEnd, Notification or *Failure events. SNIPPET (the latter is community, not official).
- Approval blocking: a PermissionRequest hook can approve or deny by returning a JSON decision. SNIPPET (developers.openai.com/codex/hooks). Exact reply schema: UNCONFIRMED.
- TUI notifications: `tui.notifications` (bool or list of event types). SNIPPET (config reference). This is display only, not a hook.
- Approval policy: `approval_policy` = `on-request`, `never`, or `granular` table. `untrusted` unsupported; `on-failure` deprecated. SNIPPET. Use `never` for non-interactive runs (per snippet).
- Per-launch config override: UNCONFIRMED (`-c key=value` and `CODEX_HOME` not verified).
- Session id / resume: `codex resume [SESSION_ID|--last]` (id is a UUID or name). `codex exec resume` for non-interactive. SNIPPET (developers.openai.com/codex/cli/reference). Session id in output: UNCONFIRMED.
- ACP: a registry snippet lists `codex-acp` ("Codex") with npm package `@agentclientprotocol/codex-acp`, versions 1.1.7 to 1.4.0. SNIPPET (cdn.agentclientprotocol.com). Third-party/adapter, not native. Package-name conflict with an older memory of `@zed-industries/codex-acp`: UNCONFIRMED.
- Headless structured output: `codex exec` streams to stdout or JSONL. SNIPPET. Whether approvals are issued in exec mode: UNCONFIRMED.
- Auth status: `codex login` (stable) is official SNIPPET. `codex login status` appears only in a third-party cheat sheet. UNCONFIRMED.
- Version: UNCONFIRMED (`codex --version` expected).

## 3. Google Gemini CLI

- Hooks: configured in `settings.json` keyed by event name; each has a regex matcher and command hooks; default timeout 60000 ms. SNIPPET (github.com/google-gemini/gemini-cli docs/hooks/reference.md; geminicli.com/docs/hooks/writing-hooks/). Snippets are from third-party issues/PRs, not opened docs.
- Events named in snippets: BeforeTool, AfterTool, Notification, SessionStart, SessionEnd, AfterAgent. SNIPPET (names only).
- Blocking: a BeforeTool hook can `deny` (or `block`) with a reason returned to the agent as a tool error. `hookSpecificOutput.tool_input` rewrites args. `continue: false` stops the loop. SNIPPET.
- Approval (allow): a third-party issue says hooks cannot approve a call; in one source's v0.70.0 reading, `allow` is a no-op. Approvals instead come from the policy engine: TOML files in `~/.gemini/policies/` with `allow`, `deny`, `ask_user`. SNIPPET (conflicting, not verified against source). Blocking-until-user-answers via hooks: UNCONFIRMED.
- Notification: fires on errors, warnings and info; a third-party PR says permission prompts arrive as Notification type `ToolPermission`. SNIPPET (third-party).
- Per-launch settings file / env var: UNCONFIRMED.
- Session id in payload: UNCONFIRMED (stdin `session_id` expected).
- Resume: UNCONFIRMED.
- ACP: an aggregator snippet says Gemini CLI is listed in ACP docs. Launch flag (e.g. `--acp`): UNCONFIRMED.
- Headless: `-o stream-json` expected; UNCONFIRMED.
- Auth status: UNCONFIRMED (no command found).
- Version: UNCONFIRMED.

## 4. OpenCode (sst/opencode)

- Plugin events: `permission.asked` fires when permission is required (SNIPPET, community plugin READMEs). `permission.updated` is listed as a v1 SDK event (SNIPPET). `session.idle` is reported both as deprecated-but-emitted and as not emitted in v2 (where `session.status` with `status.type: 'idle'` is used). CONFLICTING: verify per version.
- Permission blocking: one GitHub comment says a `permission.ask` hook exists but is never called. Another project saw `permission.asked` / `permission.replied` not firing. CONFLICTING/UNCONFIRMED whether a plugin can block until allow/deny.
- Per-launch config: `OPENCODE_CONFIG` env var expected; UNCONFIRMED.
- Session id / resume: UNCONFIRMED (`--session`/`--continue` expected; `opencode session list` expected).
- ACP: `opencode acp` expected; no source found. UNCONFIRMED. A third-party registry mirror lists OpenCode with no adapter package. SNIPPET.
- Headless: `opencode run --format json` expected; UNCONFIRMED.
- Auth status: `opencode auth list` expected; UNCONFIRMED.
- Version: UNCONFIRMED.

## Source URLs seen in search results (NOT opened, NOT confirmed)

- https://code.claude.com/docs/en/hooks
- https://code.claude.com/docs/en/agent-sdk/hooks
- https://developers.openai.com/codex/hooks
- https://developers.openai.com/codex/config-reference
- https://developers.openai.com/codex/cli/reference
- https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md
- https://geminicli.com/docs/hooks/writing-hooks/
- https://github.com/google-gemini/gemini-cli/issues/23123
- https://github.com/openai/codex/issues/3052
- https://github.com/joshuadavidthomas/opencode-plugins-manual/blob/main/docs/07-events.md
- https://cdn.agentclientprotocol.com/registry/v1/v2026.09.07-50f99cd/registry.json
- https://cdn.agentclientprotocol.com/registry/v1/v2026.07.27-6a4e802/registry.json

## Next steps to close the gaps

- Re-run with working fetch access to the official pages: code.claude.com hooks + CLI reference; developers.openai.com/codex hooks + config; geminicli.com hooks + headless docs; opencode.ai/docs plugins + CLI; agentclientprotocol.com agents page.
- Verify locally where possible: `claude --help`, `codex --help`, `gemini --help`, `opencode --help`, and `<cli> --version` (no auth files read).
