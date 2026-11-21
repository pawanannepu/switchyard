# Workspaces

Running tasks, their environments, recovery, history and prompts. Back to the [index](index.md).

## Slice 1: Core task loop

### 8. Core task loop · needs a decision
The thinnest real thread through every layer: add a local repo as a project, run one task with Claude Code in its own worktree, watch it live, review the diff, merge or discard. This is also the walking skeleton.
**Done when:** you add a git repo as a project; a task creates a new worktree and branch and starts Claude Code there through its installed CLI; output streams live into the workspace terminal with its status; you can read the full diff, then merge into the base branch or discard, both behind a confirmation; the agent never writes to your main checkout.
- [ ] Design it (spec): `/architect core task loop`

## Slice 2: Run many safely

### 9. Parallel workspace board · needs a decision
Many workspaces side by side, each with its own terminal and live status, so you can see everything running at a glance.
**Done when:** you can run several tasks at once, each in its own workspace; a board shows every workspace's status (running, waiting for input, finished, failed); you can switch between them by keyboard; the UI stays responsive with a dozen agents streaming at once.
- [ ] Design it (spec): `/architect parallel workspace board`

### 10. Attention notifications · needs a decision
Tell you the moment an agent needs you, so you don't have to watch every terminal.
**Done when:** you get an OS notification when an agent is waiting for input, finishes or fails; clicking it opens that workspace; the board shows an attention badge; you can mute notifications per project.
- [ ] Design it (spec): `/architect attention notifications`

## Slice 3: Project environment

### 13. Setup & run scripts · needs a decision
Reusable per project scripts, so every fresh worktree is ready to run before the agent starts.
**Done when:** a project's setup script runs automatically in each new worktree before the agent starts, and its failure is shown clearly and stops the task; saved run scripts (a dev server, tests) start from the workspace with one action; scripts are versioned with the project's settings.
- [ ] Design it (spec): `/architect setup & run scripts`

### 14. Environment variables & secrets · needs a decision
Per project environment variables and secrets, available to scripts and agents, kept safe.
**Done when:** per project variables are passed to setup scripts, run scripts and agent processes; secrets live in the OS secure storage, never in plain text on disk; secrets never appear in logs or diagnostics and are masked in the UI.
- [ ] Design it (spec): `/architect environment variables & secrets`

## Slice 6: Recovery and first release

### 23. Crash safe resume · needs a decision
After an app crash or restart, you pick up exactly where you were.
**Done when:** after a crash or restart, every workspace reappears with its correct status; agent processes left behind are found and reattached or stopped cleanly; you can resume or restart any task; no worktree or branch is left broken.
- [ ] Design it (spec): `/architect crash safe resume`

## Slice 9: History

### 30. Task history & lifecycle controls · needs a decision
A persistent, searchable record of every task, plus full control over a task's life.
**Done when:** every task keeps its conversation, commands run and files changed; you can search and reopen past tasks; you can pause, stop, resume and restart a task from its workspace or the board.
- [ ] Design it (spec): `/architect task history & lifecycle controls`

## Slice 10: Prompts

### 31. Prompt templates · needs a decision
Saved prompts per project, so common tasks start in one step.
**Done when:** you can save, edit and delete prompt templates per project, with variables you fill in when starting a task; starting a task (or a comparison) from a template works from the command palette.
- [ ] Design it (spec): `/architect prompt templates`
