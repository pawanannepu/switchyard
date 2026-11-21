# Foundations

The ground every slice stands on. Back to the [index](index.md).

### 1. Stack & architecture
Decide the desktop app stack, how the UI talks to the orchestration core, and how the core stays independent of any one agent, then scaffold a runnable app.
**Done when:** the stack and module boundaries are recorded in a spec (core, adapters, UI kept separate so a new agent never touches the core), and the empty app opens a window on macOS and passes build, with nothing in the core tied to one OS.
spec [0001](../specs/0001-stack-architecture/index.md)
- [x] Decide the stack (spec): `/architect stack & architecture`
- [ ] Scaffold from the decision: `/develop stack & architecture`

### 2. Coding standards & tooling
Capture conventions from the real scaffolded project, then install lint, format, type checks, pre commit hooks and CI, so every later feature follows them.
**Done when:** root `AGENTS.md` reflects the real stack, and lint, format, type checks and CI run clean on the scaffold. (basis: cross cutting tooling comes before the code that must follow it)
- [ ] Capture conventions + tooling choices: `/audit`
- [ ] Install the tooling: `/develop tooling`

### 3. Data model · needs a decision
The local records everything builds on: projects, workspaces, tasks, runs, agents, output and events, approvals, comparisons, usage.
**Done when:** the entities support every planned slice (compare, approvals, cost, history, resume) without a breaking migration, everything is stored locally, and work is saved continuously so a hard kill of the app loses nothing already shown. (basis: the data model is the costliest thing to redo)
- [ ] Design it (spec): `/architect data model`

### 4. Agent adapter contract & fake agent · needs a decision
The one interface every agent plugs into: detect, launch through its official CLI, stream output, report status, relay approvals, stop. Plus a fake agent that tests drive.
**Done when:** an adapter covers detection, launch, streaming, the four statuses (running, waiting for input, finished, failed), approval requests and stopping; adding an adapter needs no core change; no adapter ever reads, stores or reuses login tokens; a scriptable fake agent runs orchestration tests fast and deterministically. (basis: your modularity requirement; decide the interface before its first implementation)
- [ ] Design it (spec): `/architect agent adapter contract & fake agent`

### 5. Agent process supervisor · needs a decision
Runs each agent in its own process with its own terminal, outside the UI, and keeps a crashed, hung or runaway agent from affecting anything else.
**Done when:** each agent runs in its own process with a terminal; a crash, hang or runaway (time or output limits) is detected and that agent is stopped cleanly; the app and other agents keep running; tests prove it with the fake agent.
- [ ] Design it (spec): `/architect agent process supervisor`

### 6. Logging & diagnostics · needs a decision
One logging convention for the whole app, with secrets redacted, and a diagnostics bundle you can export and share by hand.
**Done when:** errors are logged clearly with enough context to debug; secrets and env values never appear in logs; you can export a diagnostics bundle and see what's in it before sharing; nothing is sent anywhere automatically. (basis: a logging and redaction convention is cheaper before code exists than retrofitted)
- [ ] Design it (spec): `/architect logging & diagnostics`

### 7. Design system & UI foundation · needs a decision
Visual language, layout and base components for a polished, consistent, keyboard first app in dark and light themes.
**Done when:** `design.md` covers type, color, spacing and components for both themes; base components meet WCAG 2.2 AA (contrast, visible focus, screen reader labels); every action is reachable by keyboard and a command palette; all UI strings live in one catalog, ready for translation.
- [ ] Design it (spec): `/architect design system & UI foundation`
