# Agents

Adapters, onboarding and extensions. Every adapter builds on the contract in [feature 4](foundations.md). Back to the [index](index.md).

## Slice 4: More agents

### 15. Codex adapter · needs a decision
Run Codex tasks exactly like Claude Code ones, through the user's installed and signed in CLI.
**Done when:** Codex tasks run, stream and report all four statuses and approval requests; detection tells installed from signed in without reading any token; the adapter adds no core code.
- [ ] Design it (spec): `/architect codex adapter`

### 16. Gemini CLI adapter · needs a decision
Run Gemini CLI tasks through the user's installed and signed in CLI.
**Done when:** Gemini CLI tasks run, stream and report all four statuses and approval requests; detection never reads a token; no core code changes.
- [ ] Design it (spec): `/architect gemini cli adapter`

### 17. OpenCode adapter · needs a decision
Run OpenCode tasks through the user's installed and signed in CLI.
**Done when:** OpenCode tasks run, stream and report all four statuses and approval requests; detection never reads a token; no core code changes.
- [ ] Design it (spec): `/architect opencode adapter`

### 18. Generic CLI adapter · needs a decision
Add any command line agent through a small config, no code needed. Your own agent pk is the first proof.
**Done when:** you can add an agent by describing its command, arguments and how to tell its status; it runs, streams and reports status like a first class agent (with fewer details where the CLI gives less); pk runs this way; contributors can share configs.
- [ ] Design it (spec): `/architect generic cli adapter`

### 19. Agent detection & onboarding · needs a decision
First launch finds your agents and gets you to a first running task in a couple of minutes.
**Done when:** first launch lists every supported agent as installed, signed in or missing (asking each CLI, never reading tokens), with a hint to fix missing ones; you add a first project and start a first task in about two minutes; you can rerun detection any time.
- [ ] Design it (spec): `/architect agent detection & onboarding`

## Slice 11: Extensions

### 32. MCP servers & agent plugins · needs a decision
Configure MCP servers and agent plugins once per project, and pass them to each agent that supports them.
**Done when:** you can add, enable and disable MCP servers and plugins per project; each agent that supports them gets them through its official config; agents that don't are clearly marked; secrets in those configs use the secure storage from feature 14.
- [ ] Design it (spec): `/architect mcp servers & agent plugins`

### 33. Agent managed workspaces · needs a decision
A local control surface that lets an agent create, check on and close workspaces itself.
**Done when:** an agent can create a workspace, start a task, read its status and close it through a local interface; every agent made action obeys the same limits, sandbox and approvals as yours; nothing is exposed beyond your machine.
- [ ] Design it (spec): `/architect agent managed workspaces`

## Deferred
- **pk & Cursor agent first class adapters**: richer status and approvals than the generic adapter gives · needs a decision
