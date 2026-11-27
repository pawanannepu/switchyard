# Foundations

The ground every slice stands on. Back to the [index](index.md).

### 1. Stack & architecture
Decide the desktop app stack, how the UI talks to the orchestration core, and how the core stays independent of any one agent, then scaffold a runnable app.
**Done when:** the stack and module boundaries are recorded in a spec (core, adapters, UI kept separate so a new agent never touches the core), and the empty app opens a window on macOS and passes build, with nothing in the core tied to one OS.
spec [0001](../specs/0001-stack-architecture/index.md) · code in `crates/`, `apps/desktop/`, `packages/protocol-ts/`, `scripts/`
- [x] Decide the stack (spec): `/architect stack & architecture`
- [x] Scaffold from the decision: `/develop stack & architecture`

### 2. Coding standards & tooling
Capture conventions from the real scaffolded project, then install lint, format, type checks, pre commit hooks and CI, so every later feature follows them.
**Done when:** root `AGENTS.md` reflects the real stack, and lint, format, type checks and CI run clean on the scaffold. (basis: cross cutting tooling comes before the code that must follow it)
- [x] Capture conventions + tooling choices: `/audit`
- [x] Install the tooling: `/develop tooling`

### 3. Data model
The local records everything builds on: projects, workspaces, tasks, runs, agents, output and events, approvals, comparisons, usage.
**Done when:** the entities support every planned slice (compare, approvals, cost, history, resume) without a breaking migration, everything is stored locally, and work is saved continuously so a hard kill of the app loses nothing already shown. (basis: the data model is the costliest thing to redo)
spec [0002](../specs/0002-data-model/index.md) · code in `crates/sy-core/`, `crates/sy-infra/`, `crates/sy-daemon/`
- [x] Design it (spec): `/architect data model`
- [x] Build it: `/develop data model`
  - [x] Persisted thread end to end: domain types, ports, spine migration, run log, hub saves before it broadcasts, dev run through the store (AC-1, AC-5, AC-7, AC-8, AC-14, AC-15, AC-16, AC-18)
  - [x] Recovery and reads: startup reconcile, `event.since`, `run.log_read` (AC-6, AC-11, AC-12)
  - [x] Complete spine store: settings, archive and permanent delete, invariants, log cap trimming (AC-7, AC-9, AC-10)
  - [x] Storage failure handling: crash safe backups, degraded open and `storage.recover`, disk full handling, logging checks (AC-2, AC-3, AC-4, AC-13, AC-17)
- [x] Verify it: `/check verify data model`
- [x] Test it: `/test data model`
- [x] Review it (fresh model): `/check review data model`
- [x] Document it: `/document data model`

### 4. Agent adapter contract & fake agent
The one interface every agent plugs into: detect, launch through its official CLI, stream output, report status, relay approvals, stop. Plus a fake agent that tests drive.
**Done when:** an adapter covers detection, launch, streaming, the four statuses (running, waiting for input, finished, failed), approval requests and stopping; adding an adapter needs no core change; no adapter ever reads, stores or reuses login tokens; a scriptable fake agent runs orchestration tests fast and deterministically. (basis: your modularity requirement; decide the interface before its first implementation)
spec [0003](../specs/0003-agent-adapter-contract-fake-agent/index.md) · code in `crates/sy-adapter-api/`, `crates/sy-core/src/agents/`, `crates/sy-infra/src/agents/`, `crates/sy-adapters/`, `crates/sy-fake-agent/`, `crates/sy-signal/`, `crates/sy-daemon/`
- [x] Design it (spec): `/architect agent adapter contract & fake agent`
- [x] Build it: `/develop agent adapter contract & fake agent`
  - [x] A fake run through a real PTY end to end: contract, ports, launcher, fake agent, `dev.agent_run`, `run.stop` (AC-1, AC-2, AC-4, AC-5, AC-6, AC-12, AC-13, AC-14, AC-17)
  - [x] The signal side channel: `sy-signal`, `signal.hook`, limits, events and session ref, pattern fallback (AC-6, AC-7, AC-8, AC-17)
  - [x] Approvals: migration 2, the store, `approval.*`, holds, races and run end order (AC-6, AC-9, AC-10, AC-11, AC-12, AC-17)
  - [x] Detection, login environment and release gates: `agent.list`/`agent.detect`, `env.refresh`, Claude detection, bundling `sy-signal` (AC-2, AC-3, AC-14, AC-15, AC-16)
- [x] Verify it: `/check verify agent adapter contract & fake agent`
- [x] Test it: `/test agent adapter contract & fake agent`
- [x] Review it (fresh model): `/check review agent adapter contract & fake agent`
- [x] Document it: `/document agent adapter contract & fake agent`

### 5. Agent process supervisor
Runs each agent in its own process with its own terminal, outside the UI, and keeps a crashed, hung or runaway agent from affecting anything else.
**Done when:** each agent runs in its own process with a terminal; a crash, hang or runaway (time or output limits) is detected and that agent is stopped cleanly; the app and other agents keep running; tests prove it with the fake agent.
spec [0004](../specs/0004-agent-process-supervisor/index.md) · code in `crates/sy-core/src/agents/`, `crates/sy-infra/`, `crates/sy-daemon/`, `crates/sy-fake-agent/`, `crates/sy-adapters/fake/`
- [x] Design it (spec): `/architect agent process supervisor`
- [x] Build it: `/develop agent process supervisor`
  - [x] The limit thread end to end: settings, output, idle and active clocks, `run_limit_exceeded`, the limit stop (AC-2, AC-3, AC-4, AC-5, AC-6)
  - [x] Process facts and cleanup: `sysinfo` port, migration 3, `pid_started_at`, held zombie, monitor and sweep, the 2 s scanner and tracking (AC-1, AC-7, AC-8, AC-9, AC-14)
  - [x] Restart cleanup after an engine crash (AC-10)
  - [x] Fault isolation, shutdown and logging: `panic = "unwind"`, per run monitor, `supervisor_error`, the 13 s shutdown, the fd limit (AC-11, AC-12, AC-13, AC-15)
- [x] Verify it: `/check verify agent process supervisor`
- [x] Test it: `/test agent process supervisor`
- [x] Review it (fresh model): `/check review agent process supervisor`
- [x] Document it: `/document agent process supervisor`

### 6. Logging & diagnostics · needs a decision
One logging convention for the whole app, with secrets redacted, and a diagnostics bundle you can export and share by hand.
**Done when:** errors are logged clearly with enough context to debug; secrets and env values never appear in logs; you can export a diagnostics bundle and see what's in it before sharing; nothing is sent anywhere automatically. (basis: a logging and redaction convention is cheaper before code exists than retrofitted)
- [ ] Design it (spec): `/architect logging & diagnostics`

### 7. Design system & UI foundation · needs a decision
Visual language, layout and base components for a polished, consistent, keyboard first app in dark and light themes.
**Done when:** `design.md` covers type, color, spacing and components for both themes; base components meet WCAG 2.2 AA (contrast, visible focus, screen reader labels); every action is reachable by keyboard and a command palette; all UI strings live in one catalog, ready for translation.
- [ ] Design it (spec): `/architect design system & UI foundation`
