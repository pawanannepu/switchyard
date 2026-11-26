# 0003. Rationale: agent adapter contract and fake agent

Decision record for [index.md](index.md). `/develop` doesn't need this file.

## Context

> ⚠️ Premise note: this feature carries more than "a trait plus a test double". Proving the contract end to end needs things nothing has built yet. Spec 0001's login environment capture and PTY port don't exist in code, there's no launcher, and approvals have nowhere to land. They're pulled in here as four milestones, because a contract proven only in memory would be redesigned when feature 8 meets a real agent. Supervision (limits, hangs, orphans) stays in feature 5. Per agent specifics stay in features 8 and 15 to 19.

switchyard drives the coding agents you already use (Claude Code, Codex, Gemini CLI, OpenCode, and any CLI through a generic adapter) through their own installed, signed in CLIs. The engine (spec 0001) owns every process, PTY and database write. Its crate rules say adapters depend only on `sy-adapter-api` and never on `sy-core` or `sy-infra`, so adding an agent never touches the core. Spec 0002 already fixed the run state machine (`queued`, `starting`, `running`, `waiting_input`, `finished`, `failed`, `stopped`, `lost`), the adapter reported journal kinds (`message`, `command_run`, `file_changed`), `agent_session_ref`, and the `approval` and `agent` tables for later features.

The forces:
- **Fidelity.** You watch agents live and type into them. Spec 0001's whole streaming design (xterm.js, `run.output` offsets, raw PTY logs) assumes the agent's own terminal UI.
- **Structured signals.** The scope's done when line needs four statuses and approval requests from every first class agent, and an approvals inbox later (feature 12). Reading those off a TUI screen is fragile.
- **Heterogeneity.** The four CLIs expose different integration surfaces. A check of the installed versions (Claude Code 2.1.325, Codex 0.162.1, Gemini CLI 0.43.0, OpenCode 1.18.3) shows that all four can be configured per launch (`--settings`, `-c key=value`, a `hooks` command, plugins), and all four have session ids and resume. Only Gemini CLI (`--acp`) and OpenCode (`opencode acp`) speak the Agent Client Protocol natively. Claude and Codex reach it through third party wrapper packages, according to search snippets.
- **Credentials.** No adapter may read, store or reuse login tokens, and that must survive many contributors writing adapters.
- **Testability.** Orchestration tests must be fast and deterministic, but still go through the real process and socket paths (AGENTS.md: infra is tested against real systems, with the fake agent).
- **Generic agents.** Feature 18 adds agents from config with no code, so the contract can't need anything a config can't express.

Without a decision, feature 8 would invent this interface for one agent, and features 15 to 18 would bend it four more ways.

## Options considered

### Option 1: PTY TUI plus a hook side channel, pure adapters (chosen)

The agent's real TUI runs in a PTY. The adapter configures the CLI's own hooks for this launch only, and they call `sy-signal`, which reports to the engine. Adapters are synchronous translators with no I/O. Output patterns and a quiet timer are the fallback.

**Pros**: exact fidelity, with the user's own installed CLI and nothing wrapped. Structured status and approvals where hooks exist. Fits spec 0001 and 0002 as built. Adapters test as plain functions, and the token rule is checkable.
**Cons**: a different hook system per agent, each liable to change between CLI versions. Not every hook can hold an approval, so some adapters fall back to keystrokes. One extra helper binary to ship and sign.

### Option 2: Agent Client Protocol as the primary transport

Run each agent as an ACP server (JSON-RPC over stdio) and render a structured transcript. Permission requests, tool calls and file edits arrive typed.

**Pros**: one uniform protocol for every agent. Approvals, tool calls and diffs are first class.
**Cons**: Claude Code and Codex need third party wrapper packages instead of your installed CLI, against the product rule. The UI becomes a transcript, not the agent's own terminal, which replaces 0001's terminal streaming. Wrapper and protocol churn becomes our problem.

### Option 3: Each CLI's headless JSON mode

Run `claude -p --output-format stream-json`, `codex exec --json`, `gemini -o stream-json`, `opencode run --format json`, and parse each stream.

**Pros**: structured output without wrappers, through the official CLIs.
**Cons**: a different format per agent, and the headless modes are weak at approvals (Claude needs a permission prompt tool) and at multi turn follow ups. There's no TUI to watch or type into.

### Option 4: Both PTY and ACP drivers now

Build Option 1 and Option 2 together, and let each adapter pick a driver.

**Pros**: the best transport per agent, with structured ACP where it's native (Gemini, OpenCode).
**Cons**: roughly double the engine work and test matrix now, for a need no adapter has yet. Two UI rendering paths.

## Rationale

The product promise is "the agents you already use, as installed", and the engine was built around their terminals (0001's PTY streaming, 0002's raw run logs). Option 1 keeps both and adds structure only where the CLI offers it, through the CLI's own documented hooks. ACP (Option 2) fits Gemini and OpenCode, but for Claude and Codex it means shipping someone else's wrapper instead of your CLI, and swapping the terminal for a transcript. Headless modes (Option 3) lose the interactive terminal and handle approvals poorly. Option 4 is Option 1 plus Option 2's cost before any adapter needs it. The contract's `Report` vocabulary is transport neutral, so an ACP driver can be added later behind it.

Pure adapters follow from the credentials force: if adapters can't do I/O, "never read tokens" becomes a dependency and source check plus a program allowlist, not a promise. It also makes adapters trivially unit testable and keeps one place (the engine) that spawns processes with argument arrays. The fallback rule (an unreachable engine means the agent's own prompt, never allow) puts safety above convenience. The cost of a missed hook is a prompt you answer in the terminal, while the cost of a wrong allow is an unreviewed command.

A real fake binary, rather than an in process double, is what makes the fake worth having: it goes through the PTY, `sy-signal` and the socket exactly as Claude will, so a green test means the production path works.

### Sub decisions

| Decision | Pick | Why | Runner up |
|---|---|---|---|
| Adapter shape | Pure, synchronous, plans and reports | Deterministic tests, checkable token rule, one I/O owner | Async with injected handles |
| Status ownership | Split by who knows | Only the adapter knows what an exit means for its agent, and the engine keeps the stored machine | Exit code alone decides |
| Hook helper | Separate `sy-signal` binary (client crates only) | Can't touch the database by construction, and boundary checked like `sy-devbridge` | `switchyardd signal` subcommand |
| Hook socket | `engine.sock` as a `signal` client plus a token | Same uid agents can reach any socket, so a second one adds a listener without a boundary | Separate `signal.sock` |
| Hook call | One blocking `signal.hook` returning stdout and exit code | Agent agnostic: the adapter writes whatever its CLI expects | Report, then await |
| Fallback reply | The agent's own prompt | Never auto approve when switchyard can't see the request | Deny |
| Fallback status | Output patterns plus a quiet timer | The generic adapter needs it, and hooks win when both fire | Hooks only |
| ANSI stripping | `vte` | Proven streaming parser (Alacritty), handles sequences split across reads | `strip-ansi-escapes` |
| Feature 4 boundary | Contract plus minimal launcher | Tracer Bullet: prove it against a real process now | Contract only |
| Approvals | Table, notifications, respond, list | Done when needs a real relay, and feature 12 then only adds UI and policy | In memory only |
| Approval kind | Open text plus a Rust enum | New kinds need no table rebuild | CHECK constrained |
| Approval detail | Typed per kind plus `raw` | Policies read typed fields and nothing is lost | Typed only |
| Unanswered approvals | End with their run, expire at start and on hook timeout | No silent timers that deny what you meant to approve | A fixed expiry timer |
| Many pending | Allowed, and the run waits for all | Matches parallel tool calls without reordering | One at a time |
| Signal limits | 256 KiB, 100/s with a burst of 500, holds exempt | A looping hook can't swamp the journal | Size cap only |
| Running trigger | Decoder report or first output byte | An agent still loading doesn't look busy, and exit only adapters still show running | On spawn |
| Stop | Adapter input, then SIGHUP, then SIGKILL, and deny held hooks first | Clean TUI exits keep session state, and no tool runs during shutdown | Signals only |
| Detection storage | In memory, via `agent.list` and `agent.detect` | Cheap to rerun, and the `agent` table is features 18 and 19's | Persist now |
| Launch API | `dev.agent_run` plus a real `run.stop` | Doesn't pin feature 8's task API, and stop is agent agnostic | A real `workspace.start_run` |
| Fake agent | Real binary, JSON steps, devtools only | Production path fidelity, self contained tests | In process fake |
| JSON in the contract | Add `serde_json` | The standard value type, already in the workspace | Our own JSON type |
| Launch files | `<data dir>/agent-runtime/<run_id>/` | Outside the worktree, outside the run log sweep, cleaned at start | Inside the worktree |
| Claude adapter now | Detection only | Proves detection on a real CLI without pulling in feature 8 | Unregister it |
| Generic fit | Contract expressible by config, runtime registered instances | Feature 18 becomes one adapter reading many configs | Rust only for now |

### Evidence: installed CLI surfaces (2026-11-22)

From `--version` and `--help` on this machine. These are help texts, not docs, so features 8 and 15 to 17 confirm the details.

| CLI | Version | Per launch config | Session and resume | ACP | Headless |
|---|---|---|---|---|---|
| Claude Code | 2.1.325 | `--settings` (hooks, `apiKeyHelper`), `--bare` skips hooks | `--resume <session-id>` | not native | `-p` |
| Codex | 0.162.1 | `-c key=value` overrides `~/.codex/config.toml` | `resume`, `fork`, `--last` | not native | `exec` |
| Gemini CLI | 0.43.0 | `gemini hooks` | `--session-id <uuid>`, `-r/--resume` | `--acp` | `-o stream-json` |
| OpenCode | 1.18.3 | `plugin`, config | `-s/--session`, `-c/--continue` | `opencode acp` | `run` |

The web check (search snippets only, no page could be opened, so nothing below is confirmed): Claude Code hooks include `PreToolUse` (reply `permissionDecision: allow | deny | ask`), `Stop`, `Notification` and `SessionStart`. Codex has `notify` and `PermissionRequest` hooks. Gemini CLI has `BeforeTool`, `AfterTool`, `Notification`, `SessionStart` and `SessionEnd`. OpenCode plugins see `permission.asked` and `session.idle`. ACP wrappers for Claude and Codex are published as `@agentclientprotocol/claude-agent-acp` and `@agentclientprotocol/codex-acp`. Full notes are in `docs/.agent-cache/research/agent-cli-integration.md`.

## References

**Project sources** (verifiable, in this repo):
- Spec 0001: the engine owns processes and PTYs, the crate dependency table, terminal streaming, the login environment, the follow up to weigh ACP beside PTY.
- Spec 0002: the run state machine, adapter reported journal kinds, `agent_session_ref`, the planned `approval` and `agent` tables, persist before broadcast, the additive migration rule.
- `AGENTS.md` and `crates/AGENTS.md`: ports and adapters, the no tokens rule, argument arrays only, test against real systems with the fake agent.
- `docs/scope/foundations.md`, feature 4's done when line. `docs/scope/agents.md`, features 15 to 19.
- The installed CLIs' `--version` and `--help` output on 2026-11-22 (table above).
- `docs/.agent-cache/research/agent-cli-integration.md` (unconfirmed web notes).

**Practices & standards**:
- Sans I/O protocol design (pure state machines with the I/O at the edge).
- Ports and adapters (hexagonal architecture).
- Fail safe defaults (an unreachable approver never means allow).
- Least privilege for plugins (an explicit program allowlist).
- Token bucket rate limiting.
- Deterministic, event driven test doubles over sleeps.

**Links**: none verified. The web check couldn't open any page, so no URL is cited. The official CLI docs and the Agent Client Protocol site are named in the scope's References for a human to follow.
