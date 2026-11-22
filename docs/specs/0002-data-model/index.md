# 0002. Local data model: relational state, an event journal and per run output logs

**Date**: 2026-11-22
**Status**: In Progress

## Summary

This spec decides how switchyard stores everything it knows on your machine: projects, tasks, the workspaces (git worktrees) each task runs in, every agent run, and the full history of what happened. Current state lives in ordinary SQLite tables, every change is also written to an append only journal (a log you only ever add to), and each run's raw terminal output goes to its own file. The engine saves a change before it tells any window about it, so a hard kill never loses something you already saw. The whole model is designed now for every planned slice, but this feature only creates the core tables. Later features add theirs exactly as written here, so none of them needs a breaking change.

## Requirements

**User stories**:
- As a switchyard user, I want everything I've seen in the app to still be there after a crash or a `kill -9`, so I never lose track of what an agent did.
- As a switchyard user, I want old tasks to stay searchable after I close them, and to be able to delete them for good when I choose, so history helps without filling my disk.
- As a switchyard user, I want a clear explanation and a safe way out when my database is corrupt or from a newer version, so a storage problem never silently eats my history.
- As the switchyard developer, I want one storage design that already fits comparisons, approvals, cost, PRs, CI, templates and extensions, so later features only add tables and never rewrite old ones.

**Acceptance criteria**:
- **AC-1**: On start, the engine opens (or creates) `<data dir>/switchyard.db` with `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000` and `journal_size_limit=67108864` (64 MiB). It applies every pending migration in order, each in its own transaction, until `PRAGMA user_version` equals the newest migration number. The data dir, `runs/` and `backups/` are `0700` (an existing data dir with wider modes is set to `0700`). `switchyard.db`, its `-wal` and `-shm` files, every backup, every `.bad-*` file and every run log are `0600`. Each file is created with explicit mode bits, and the engine never changes the process umask, so agents it spawns keep your normal umask. On clean shutdown the engine runs `PRAGMA wal_checkpoint(TRUNCATE)`.
- **AC-2**: Before applying any pending migration to an existing database (`user_version` > 0), and at startup whenever the newest backup is more than 24 hours old, the engine writes a backup with `VACUUM INTO` to `backups/switchyard-v<user_version>-<unix ms>.db.tmp`, runs `PRAGMA quick_check` on it, and only then renames it to drop `.tmp`. It keeps the 3 newest backups. A `.tmp` left by a crash is deleted at the next start. Creating a fresh database makes no backup.
- **AC-3**: The engine opens storage in this order: open the file, read `user_version`, run `PRAGMA quick_check`, run migrations. If any step fails, it still takes the lock and binds the socket, in **degraded storage mode**. The mapping is: `user_version` newer than the engine knows gives `newer_schema`; `SQLITE_NOTADB`, `SQLITE_CORRUPT` or a failing `quick_check` gives `corrupt`; `SQLITE_CANTOPEN`, `SQLITE_PERM` or any other open error gives `unavailable`; a migration that fails (and rolls back) gives `unavailable` with the error text in `message`. The stable methods answer, `engine.info` returns `storage { status, message, schema_version, latest_backup }` (a new optional field, additive to the frozen stable method), and `storage.recover` is accepted. Every other method returns `StorageUnavailable`. Nothing writes to the database file in this mode except `storage.recover`.
- **AC-4**: `storage.recover { action: "restore_backup" | "start_fresh", confirm: true }` runs in degraded mode only. It renames the bad `-wal` and `-shm` files first, then the database, each to `<name>.bad-<unix ms>`, and never deletes any of them. It moves `runs/` to `runs.bad-<unix ms>/` (no orphan sweep after a recover). Then it either copies the newest backup that passes `quick_check` into place, through a temp name plus rename, and migrates it, or it creates an empty database. Storage status becomes `ok` with no engine restart. It returns `StorageRecoverNotNeeded` when the status is `ok` or `degraded_writes`, `BackupMissing` for `restore_backup` when no backup passes `quick_check`, and `InvalidParams` without `confirm: true`.
- **AC-5**: **Persist before broadcast.** Every byte a client receives, live in `run.output` or as replay from `run.attach`, was first written to `runs/<run_id>.log` at the same offset. The replay ring is filled only after the append, so `run.attach`'s `end_offset` never exceeds the bytes in the file. Every `run.status` notification is sent only after its `run` row change and `event` row have committed, and it carries that event's `event_id`. When a run ends, its final output is appended and delivered, then `log_bytes` and the terminal status commit together, then `run.status` is sent, so no client sees the end before the last output. After `kill -9` of the engine and a restart, each run's log is at least as long as the highest `offset + length` any client received, and every status any client saw is present in `run` and `event`.
- **AC-6**: On start with storage `ok`, every run in `starting`, `running` or `waiting_input` becomes `lost`, with `end_reason = "engine_restart"`, `ended_at` set to the engine's start time, and one `run_status_changed` event each. Runs in `queued` stay `queued`. Each run's `log_bytes` is set to its log file's real length (0 when there's no file). A run whose `log_state` is `present` or `incomplete`, whose stored `log_bytes > 0`, and whose file is missing becomes `deleted` (a run that never wrote output has no file and stays `present`). Every workspace still in `preparing` becomes `discarded`, with `closed_at` set and one `workspace_state_changed` event. Any file in `runs/` with no `run` row, or whose run is `trimmed` or `deleted`, is removed, and so is any `backups/*.tmp`. This feature never signals leftover agent processes. Features 5 and 23 own that, using the stored `pid`, `pgid` and `pid_started_at`.
- **AC-7**: The database enforces: `repo_path` is unique across projects (`uq_project_repo_path`); at most one `kind = 'agent'` run per workspace is in `queued`, `starting`, `running` or `waiting_input` (`uq_run_live_agent`); a branch is unique per project among workspaces that aren't `discarded` (`uq_workspace_branch`), so a merged workspace keeps its branch name reserved; at most one workspace per task has `outcome = 'winner'` (`uq_workspace_winner`); and `worktree_path` is unique (`uq_workspace_worktree_path`). `add_workspace` and `create_task` copy `workspace.project_id` from the task inside the insert, so a mismatch can't be written. The store maps each violation by its index name to a distinct `StoreError` (`ProjectExists`, `RunAlreadyLive`, `BranchInUse`, `WinnerExists`, `WorktreePathInUse`), never a raw SQLite error.
- **AC-8**: The run status machine in `sy-core` rejects any transition not listed under *State transitions* (for example `finished → running`) with `StoreError::InvalidTransition`, and makes no write and no event. A report of the run's current status is a no op: it returns no event and writes nothing. `transition_run` refuses `lost` as a target (`InvalidTransition`). Only `mark_live_runs_lost` sets it.
- **AC-9**: Archiving a task or project sets `archived_at` and writes one event, and the rows and logs stay. Archiving or permanently deleting a task, or a project, that has a run in `queued`, `starting`, `running` or `waiting_input` fails with `StoreError::HasLiveRuns`. Permanent delete is allowed only on an already archived task or project (otherwise `StoreError::NotArchived`). It deletes the rows in one transaction (cascading to workspaces, runs, events and settings) and returns the removed run ids. The use case then removes those run log files. A crash between those two steps leaves orphan files that AC-6's sweep removes. Neither one ever touches the repo, a worktree or a branch.
- **AC-10**: The total size of run logs is kept under the global setting `storage.log_cap_bytes` (default 5368709120, 5 GiB). The total is the SUM of `log_bytes` over ended runs whose `log_state` is `present` or `incomplete`, plus the bytes written so far by each live run (from the hub). When it's over, the engine trims whole log files from **ended runs of archived tasks** only, where a task counts as archived when `COALESCE(task.archived_at, project.archived_at)` is set. It goes oldest archive time first, then the oldest run, until it's under. For each one, it first commits `log_state = 'trimmed'` and one `run_log_trimmed` event, then unlinks the file. A non archived task's log, and a live run's log, are never trimmed. If only those remain over the cap, the engine logs a warning once per check. The check runs at startup, when any run ends, and after any archive.
- **AC-11**: `event.since { cursor, limit?, project_id?, task_id?, workspace_id?, run_id? }` returns events with `id > cursor` in ascending id order. Filters combine with AND. `limit` defaults to 200 (also when given as 0) and is capped at 500. A negative cursor is `InvalidParams`. The response includes `next_cursor` and `has_more`. Each event has the `Event` shape and a payload as defined in *Journal event kinds*. A `run_status_changed` payload carries the same fields as the `run.status` notification.
- **AC-12**: `run.log_read { run_id, offset, max_bytes? }` returns up to `max_bytes` (default 262144, also when given as 0, and capped at 262144) raw bytes from that run's log file, as `data_b64`, plus `offset`, `eof` and `log_bytes`. `log_bytes` is the bytes in the file now, and `eof` is true when `offset + returned length` reaches it. For a live run that only means "end of the output so far". An offset at or past the end, or a run with no file yet, returns empty data with `eof: true`. A run whose `log_state` is `trimmed` or `deleted`, or whose file disappears during the read, returns `RunLogUnavailable`. An unknown run returns `RunNotFound`.
- **AC-13**: When a log append or database write fails with disk full or an I/O error during normal running, the agent keeps running, and its output keeps streaming live from memory. Storage status becomes `degraded_writes`, reported by `engine.info` and a `storage.status` notification. **Logs**: after a run's first failed append, the engine truncates any partial write back to the last good offset and stops appending to that run for good. The run's `log_state` becomes `incomplete`, and log reads return the good prefix. **Database**: a failed `transition_run`, `set_*` or `append_event` is kept in an in memory queue, in order. Every later command of those kinds joins the queue behind it, so order holds. The queue is capped at 10000 commands, and beyond that new `append_event` commands are dropped (counted). A create command (`create_*`, `add_workspace`) is never queued and fails at once with `StoreError::Full`. Every 5 seconds the engine retries the queue, then probes with a one row write to `setting` key `storage.probe`. Status returns to `ok`, with another `storage.status` notification, once the queue is empty and the probe succeeds. Notifications sent while degraded carry `event_id: null`, and after recovery clients reload their view with `event.since`.
- **AC-14**: `sy-core` defines the domain types and the `Store`, `RunLog` and `Clock` ports, and depends on no `rusqlite`. The entity rules (status machine, invariants checkable in memory, id parsing) are unit tested in `sy-core` with no infrastructure. `SqliteStore` and `FileRunLog` live in `sy-infra` and are integration tested against real SQLite files and real files in a temp dir. `pnpm boundaries:check` passes.
- **AC-15**: In a `devtools` build, `dev.synthetic_run { count, bytes_per_sec, echo }` keeps its current streaming behaviour and creates (on first use) one fixture project, one task per call, and one workspace plus one `agent` run per synthetic run, all through the `Store`. Each run is created with the store's typed `run_…` id, which the hub uses. Each run goes `queued → starting → running` with `pid` and `pgid` NULL, and output is persisted through the `RunLog` before broadcast. `dev.stop_runs` moves those runs to `stopped` with `end_reason = "user_stop"`. This is the end to end thread for AC-5 and AC-6.
- **AC-16**: Every id is `<prefix>_<32 lowercase hex chars of a UUIDv7>` (prefixes in *Data model*), stored as TEXT. Parsing an id with the wrong prefix or bad hex fails. Every `*_at` column is INTEGER Unix milliseconds UTC from the `Clock` port.
- **AC-17**: No table in migration 1 has a column meant for a secret value. Event payloads are typed Rust enums with no field that carries an environment value. Engine logs from the store record only the command name, ids, row counts and duration, never payloads, prompts or output bytes.
- **AC-18**: `run.status` and `storage.status` go to every connection that completed a compatible `session.hello`, whether or not it attached to the run. Committed events reach the broadcaster through one ordered channel fed by the database thread, so notifications leave in `event_id` order. In this feature, `run_status_changed` is the only journal kind with a live notification, and the other kinds are read through `event.since`. A client advances its cursor only from `event.since` results, never from a notification's `event_id`.

## Decision

**Chosen option**: Option 1: relational state rows, an append only event journal, and flat per run log files, with the full model designed and only the spine migrated now.

The engine stores current state in normalised SQLite tables behind a command style `Store` port, records every meaningful change in one `event` table in the same transaction, writes raw terminal output to `runs/<run_id>.log` behind a `RunLog` port, and saves before it broadcasts.

**Implementation skills** (all under `.claude/skills/<skill>/`): `rust-database` (`po4yka/rust-skills`: rusqlite, transactions, migrations, `SQLITE_BUSY`) · `rust-error-handling` (`po4yka/rust-skills`: the `StoreError` enum) · `rust-async-internals` (`po4yka/rust-skills`: the database thread, cancel safety of store calls) · `tokio-patterns` (`geoffjay/claude-plugins`: command channel, retry loop, shutdown) · `rust-best-practices` (`apollographql/skills`)

## Rationale

Reasoning, options and every choice from the design conversation: see [rationale.md](rationale.md).

## Feature design

### Domain shape

```
Project 1─N Task 1─N Workspace 1─N Run
   │          │          │          │
   └──────────┴──────────┴──────────┴──→ Event (journal, every level)
```

- A **task** is the intent (the prompt). A **workspace** is one worktree, branch and agent working on it, and a comparison (feature 22) is a task with several workspaces. A **run** is one process with its own output: an agent launch, a follow up, a resume, or a setup or run script (feature 13).
- A follow up (features 20, 29) is a new `agent` run in the same workspace, with its text in `run.input`. A follow up that should start a new task (a CI failure handed back) sets `task.parent_task_id`.
- A resume (feature 23) is a new run with `resumes_run_id` pointing at the previous one.

### Data model sketch

Conventions for every table: `id` is the TEXT primary key `<prefix>_<uuidv7 hex>`, and every `*_at` is INTEGER Unix ms UTC. A `?` means NULL is allowed, and everything else is `NOT NULL`. Closed enum columns (`run.kind`, `run.status`, `run.log_state`, `workspace.state`, `workspace.outcome`, `event.actor`) are TEXT with a `CHECK (col IN (…))`. Open ended text values (`event.kind`, `run.end_reason`, `event.actor_ref`) have no `CHECK`, and the Rust enums validate them. JSON columns are TEXT with `CHECK (json_valid(col))`. Every FK is `ON DELETE CASCADE` unless noted, so a permanent delete is one `DELETE` on the archived parent. Tables are `STRICT`. Unique indexes carry the names given here, because the store maps constraint errors by index name.

**Migration 1 (the spine, built by this feature)**

| Table | Columns | Keys, constraints, indexes |
|---|---|---|
| `project` (prj_) | `id`, `name`, `repo_path` (stored exactly as given; feature 8 canonicalises before calling), `base_branch`, `settings_revision` INTEGER default 0, `created_at`, `archived_at?` | `uq_project_repo_path` on `repo_path` |
| `project_setting` | `project_id`, `key`, `value` JSON, `updated_at` | PK (`project_id`, `key`); FK project |
| `setting` | `key`, `value` JSON, `updated_at` | PK `key` (global settings) |
| `task` (tsk_) | `id`, `project_id`, `title`, `prompt`, `base_ref`, `parent_task_id?`, `created_at`, `archived_at?` | FK project; FK `parent_task_id → task(id) ON DELETE SET NULL`; `UNIQUE(id, project_id)` (target of the workspace composite FK); index (`project_id`, `archived_at`, `created_at`) |
| `workspace` (wsp_) | `id`, `task_id`, `project_id` (copied from the task inside the insert, never from the caller), `agent_id`, `agent_options` JSON, `branch`, `worktree_path`, `base_commit`, `state` (preparing, ready, merged, discarded), `outcome?` (winner, not_chosen), `merged_commit?`, `created_at`, `closed_at?` | FK (`task_id`, `project_id`) → task(`id`, `project_id`); `uq_workspace_worktree_path` on `worktree_path`; `uq_workspace_branch` partial unique (`project_id`, `branch`) `WHERE state <> 'discarded'`; `uq_workspace_winner` partial unique (`task_id`) `WHERE outcome = 'winner'`; index (`task_id`) |
| `run` (run_) | `id`, `workspace_id`, `kind` (agent, setup, run_script), `input?`, `resumes_run_id?`, `agent_session_ref?`, `status`, `exit_code?`, `end_reason?`, `pid?`, `pgid?`, `pid_started_at?` (the process start time, so features 5 and 23 can tell a live orphan from a reused pid), `queued_at`, `started_at?`, `ended_at?`, `log_bytes` INTEGER default 0, `log_state` (present, incomplete, trimmed, deleted) default present | FK workspace; FK `resumes_run_id → run(id) ON DELETE SET NULL`; `uq_run_live_agent` partial unique (`workspace_id`) `WHERE kind = 'agent' AND status IN ('queued','starting','running','waiting_input')`; index (`workspace_id`, `queued_at`); index (`status`) |
| `event` | `id` INTEGER PRIMARY KEY AUTOINCREMENT (the cursor, never reused), `project_id?` (NULL only for global events such as `setting_changed`), `task_id?`, `workspace_id?`, `run_id?`, `at`, `actor` (user, engine, policy, agent), `actor_ref?` (for `agent`: the acting workspace id, feature 33), `kind`, `payload` JSON | FKs to each scope table; indexes (`project_id`, `id`), (`task_id`, `id`), (`workspace_id`, `id`), (`run_id`, `id`). Every event fills in every ancestor id it knows (a run event sets `run_id`, `workspace_id`, `task_id` and `project_id`). |

**Later tables (designed now; each feature adds its own migration, additive only)**

| Table | Feature | Columns and constraints |
|---|---|---|
| `approval` (apr_) | 12 | `id`, `run_id` FK, `kind`, `summary`, `detail` JSON, `status` (pending, approved, denied, expired, cancelled), `requested_at`, `decided_at?`, `decided_by?` (user, policy, agent), `decided_by_ref?`, `response?` JSON. Partial index (`requested_at`) `WHERE status = 'pending'` (the inbox). |
| `run_usage` | 27 | `run_id` PK and FK, `input_tokens?`, `output_tokens?`, `cache_read_tokens?`, `cache_write_tokens?`, `cost_micros?`, `currency?` (ISO 4217), `updated_at`. NULL means the agent never reported it, shown as unknown. Totals are SUMs and time comes from `run` timestamps. |
| `script` (scr_) | 13 | `id`, `project_id` FK, `kind` (setup, run_script), `name`, `argv` JSON array, `cwd_rel?`, `position`, `source` (local, repo_file), `created_at`, `updated_at`. `UNIQUE(project_id, name)`. Also adds `run.script_id` with `ALTER TABLE run ADD COLUMN script_id TEXT REFERENCES script(id) ON DELETE SET NULL` (no table rebuild). |
| `env_var` (env_) | 14 | `id`, `project_id` FK, `name`, `value?`, `secret_ref?` (a Keychain account name, never the value), `targets` (scripts, agents, all), `created_at`, `updated_at`. `CHECK ((value IS NULL) <> (secret_ref IS NULL))`, `UNIQUE(project_id, name)`. |
| `prompt_template` (tpl_) | 31 | `id`, `project_id` FK, `name`, `body`, `variables` JSON, `created_at`, `updated_at`. `UNIQUE(project_id, name)`. Also adds `task.template_id` with `ALTER TABLE task ADD COLUMN template_id TEXT REFERENCES prompt_template(id) ON DELETE SET NULL` (no table rebuild). |
| `extension` (ext_) | 32 | `id`, `project_id` FK, `kind` (mcp_server, plugin), `name`, `config` JSON (secrets only as `secret_ref`), `enabled` INTEGER 0/1, `created_at`, `updated_at`. `UNIQUE(project_id, kind, name)`. |
| `agent` | 18, 19 | `id` TEXT PK (the adapter id like `claude`, or `gen_<uuidv7 hex>` for a generic agent), `kind` (builtin, generic), `config?` JSON, `detection?` JSON (installed, signed_in, missing, version), `detected_at?`. `workspace.agent_id` refers to it by value, with no FK. |
| `review_comment` (rvc_) | 20 | `id`, `workspace_id` FK, `path`, `line_start`, `line_end`, `side` (old, new), `commit`, `body`, `state` (draft, sent, resolved), `sent_in_run_id?` (FK run, SET NULL), `created_at`, `updated_at`. |
| `pull_request` (pr_) | 28 | `id`, `workspace_id` FK, `remote`, `number`, `url`, `title`, `body`, `state` (open, merged, closed), `created_at`, `updated_at`. `UNIQUE(workspace_id, remote, number)`. |
| `ci_check` (chk_) | 29 | `id`, `pull_request_id` FK, `name`, `status`, `conclusion?`, `details_url?`, `updated_at`. `UNIQUE(pull_request_id, name)`. |

Needs no table: the queue (feature 26) is runs in `queued` ordered by (`queued_at`, `id`), with limits in `setting` (`limits.global_running`) and `project_setting` (`limits.running`). Notification mute (feature 10) is `project_setting` key `notifications.muted`. The attention badge's "seen" state (feature 10) is an additive nullable `workspace.attention_seen_at`. The default permission mode (feature 12) is `project_setting` key `approvals.default_mode`. Search (feature 30) adds an FTS5 virtual table over `message` events, also additive. A new run status (for example feature 30's `paused`) is a new enum value, added through the rebuild rule below.

**"Additive only" means:** a later migration may add tables, indexes, nullable columns, columns with defaults (including a nullable FK column through `ALTER TABLE ADD COLUMN … REFERENCES … ON DELETE SET NULL`), and new values for a closed enum. It never renames or drops a column, never drops a table that holds user data, and never changes a column's meaning. A change that needs any of that goes back through `/architect`.

**The rebuild rule** (the only way to widen a `CHECK`): SQLite can't alter a `CHECK`, so the table is copied. `DROP TABLE` with foreign keys on would cascade delete every child row, and `PRAGMA foreign_keys` is ignored inside a transaction. So a migration file marked `-- rebuild` makes the runner set `PRAGMA foreign_keys=OFF` before the transaction. Inside it, the migration creates `<table>_new`, copies every row, drops the old table, renames the new one and recreates its indexes. The runner then runs `PRAGMA foreign_key_check`, rolls back on any finding, and sets `foreign_keys=ON` again. A test migration that rebuilds `run` proves that no child row is lost.

**Run output logs.** `runs/<run_id>.log` holds the raw PTY bytes, append only. The byte position in the file equals the protocol's per run `offset`. Each log lives as long as its run row, minus trimming (AC-10).

**The `Event` wire type** (`sy-protocol`, also what `event.since` returns): `{ id: i64, at: i64, actor: "user" | "engine" | "policy" | "agent", actor_ref?: string, project_id?: string, task_id?: string, workspace_id?: string, run_id?: string, kind: string, payload: object }`. `id` lives only on the row, never inside `payload`.

**Journal event kinds in migration 1** (one `EventKind` serde enum variant each; payload fields may only ever be added as optional, and a breaking shape becomes a new kind):

| Kind | Scope ids set | Payload |
|---|---|---|
| `project_created` | project | `{ name, repo_path, base_branch }` |
| `project_archived` | project | `{}` |
| `project_setting_changed` | project | `{ key, value, settings_revision }` (the new revision) |
| `setting_changed` | none (global) | `{ key, value }` (never written for `storage.probe`) |
| `task_created` | project, task | `{ title, base_ref, parent_task_id? }` |
| `task_archived` | project, task | `{}` |
| `workspace_created` | project, task, workspace | `{ agent_id, branch, worktree_path, state }` |
| `workspace_state_changed` | project, task, workspace | `{ from, to }` |
| `workspace_outcome_set` | project, task, workspace | `{ outcome }` |
| `run_created` | project, task, workspace, run | `{ kind, status: "queued", resumes_run_id? }` |
| `run_status_changed` | project, task, workspace, run | `{ run_id, workspace_id, from, status, end_reason?, exit_code?, at }` (the `run.status` notification's fields, without `event_id`) |
| `run_log_trimmed` | project, task, workspace, run | `{ bytes }` |
| `message` | project, task, workspace, run | `{ role: "user" \| "agent" \| "system", text }` (adapter reported, feature 4) |
| `command_run` | project, task, workspace, run | `{ argv: string[], exit_code? }` (adapter reported, feature 4) |
| `file_changed` | project, task, workspace, run | `{ path, change: "added" \| "modified" \| "deleted" \| "renamed", from_path? }` (adapter reported, feature 4) |

Permanent deletes write no event, because the journal rows go with the deleted rows.

### State transitions

**Run** (the only stored status):

| From | Allowed to |
|---|---|
| `queued` | `starting`, `stopped` |
| `starting` | `running`, `waiting_input`, `finished`, `failed`, `stopped` |
| `running` | `waiting_input`, `finished`, `failed`, `stopped` |
| `waiting_input` | `running`, `finished`, `failed`, `stopped` |
| `starting`, `running`, `waiting_input` | `lost`, only through `mark_live_runs_lost` at startup (AC-6) |
| `finished`, `failed`, `stopped`, `lost` | nothing (terminal) |

A report of the current status is a no op (AC-8). Terminal states never change again. Entering one sets `ended_at` and `end_reason` (`exited`, `user_stop`, `engine_restart`, `spawn_failed`, `limit_exceeded`, later features add values), and `exit_code` when the process exited. Entering `starting` sets `started_at`. Every transition writes one `run_status_changed` event in the same transaction.

**Workspace** lifecycle: `preparing → ready → merged | discarded`, and `preparing → discarded` (setup failed, or left in `preparing` by a crash, AC-6). `merged` and `discarded` set `closed_at`. `outcome` is set only on a task with 2 or more workspaces.

**Task**: active while `archived_at` is NULL, archived otherwise. Unarchive is feature 30's.

**Derived display status** (a pure function in `sy-core`, never stored): a workspace whose state isn't `ready` shows its state. A `ready` workspace shows the status of its newest `agent` run (by `queued_at`, then `id`), or `idle` if it has none. A task shows the most urgent of its workspaces, in the order `waiting_input`, `failed`, `running`, `starting`, `queued`, `preparing`, `lost`, `finished`, `stopped`, `idle`, `merged`, `discarded`.

### Ports (in `sy-core`)

- **`Store`** (an `#[async_trait]` trait used as `Arc<dyn Store>`, so the daemon tests can wire in a failing double; implemented by `SqliteStore`). Command methods, each one transaction that writes its rows and its events together and returns the committed events (`Vec<Event>`, with their ids, empty for a no op) for the caller to broadcast. Migration 1 methods:
  - projects: `create_project`, `get_project`, `find_project_by_repo_path`, `list_projects`, `archive_project`, `delete_project` (returns the removed run ids), `set_project_setting`, `project_settings`
  - settings: `set_setting`, `get_setting`
  - tasks: `create_task` (the task plus its first workspaces, atomically), `get_task`, `list_tasks`, `archive_task`, `delete_task` (returns the removed run ids)
  - workspaces: `add_workspace`, `get_workspace`, `list_workspaces` (by task), `set_workspace_state`, `set_workspace_outcome`
  - runs: `create_run` (takes a typed `RunId` that the caller minted, creates it `queued`), `get_run`, `list_runs` (by workspace, or by a status set for reconcile), `transition_run` (takes the target status, plus `end_reason`, `exit_code` and the final `log_bytes` on a terminal transition), `set_run_process` (`pid`, `pgid`, `pid_started_at`), `set_agent_session_ref`, `set_log_state`, `mark_live_runs_lost`, `discard_preparing_workspaces`, `reconcile_log_bytes`
  - journal: `append_event` (adapter reported kinds only), `events_since`
  - trimming: `log_total_ended` (SUM over ended runs), `log_trim_candidates` (ended runs of archived tasks in AC-10's order), `mark_log_trimmed`

  Every `list_*` uses keyset pagination (after `(created_at, id)`, limit capped at 500). Errors are `StoreError` (`thiserror`): `NotFound`, `ProjectExists`, `RunAlreadyLive`, `BranchInUse`, `WinnerExists`, `WorktreePathInUse`, `InvalidTransition`, `NotArchived`, `HasLiveRuns`, `Unavailable`, `Full`, `Io`.
- **`RunLog`** (synchronous, called from the flusher thread and from `spawn_blocking` elsewhere; implemented by `FileRunLog`): `append(run, offset, bytes)` writes at the explicit offset (`pwrite`), so a retried or reordered call can never shift bytes. Also `truncate(run, len)`, `read(run, offset, max) → bytes`, `len(run)`, `delete(run)`, and `list() → run ids with sizes`. Each method maps one I/O error to `RunLogError::{Full, NotFound, Io}`.
- **`Clock`**: `now_ms() → i64`. Ids come from `uuid::Uuid::now_v7()` inside the typed id constructors, because ids are opaque and no test depends on their value.

**Runtime shape.**
- `SqliteStore` owns one `rusqlite::Connection` on a dedicated OS thread, fed by a bounded command channel with a oneshot reply per command (spec 0001's one connection rule). Queries use cached prepared statements. After each commit, the thread pushes the committed events into one ordered channel that the broadcaster drains, so notifications leave in `event_id` order (AC-18).
- The run hub changes to fit persist before broadcast: `runs::RunId` becomes the typed `RunId` from the domain, and `RunHub::create_run` takes it instead of minting `run-N`. The order per run is `Store::create_run` (queued), then the hub entry, then `transition_run` to `starting`. `RunEntry::write` only appends to `pending`. The flush moves bytes to the replay ring **after** `RunLog::append` succeeds, and only then delivers. Each run has its own flush lock, held across take, append, ring and deliver, so the flusher thread and `remove()` can't interleave one run's chunks.
- The flusher moves to a dedicated OS thread, because `append` is a blocking `pwrite` (no fsync). The producer still never waits on a client or on disk.
- At a terminal transition: final flush (append and deliver), then `transition_run` with the final `log_bytes`, then `run.status` (AC-5). `log_bytes` is written only then and by `reconcile_log_bytes` at startup. It's never written per batch.

### API surface (engine protocol additions)

| Method or notification | Kind | Key inputs | Key outputs | Auth | Key errors |
|---|---|---|---|---|---|
| `engine.info` (existing, stable) | request | (none) | adds the optional field `storage: { status: ok \| degraded_writes \| newer_schema \| corrupt \| unavailable, message, schema_version?, latest_backup? }` (additive, so the frozen shape still holds) | none (stable) | (none) |
| `storage.recover` | request | `action: "restore_backup" \| "start_fresh"` (req), `confirm: true` (req) | `{ storage }` (the new status) | compatible `hello`; allowed in degraded mode | `StorageRecoverNotNeeded`, `BackupMissing`, `InvalidParams` |
| `event.since` | request | `cursor: i64` (req, 0 for the start, negative is invalid), `limit?: u32` (0 or absent means 200, max 500), `project_id?`, `task_id?`, `workspace_id?`, `run_id?` (AND) | `{ events: Event[], next_cursor, has_more }` | compatible `hello` | `StorageUnavailable`, `InvalidParams` |
| `run.log_read` | request | `run_id` (req), `offset: u64` (req), `max_bytes?: u32` (0 or absent means 262144, max 262144) | `{ offset, data_b64, eof, log_bytes }` | compatible `hello` | `RunNotFound`, `RunLogUnavailable`, `StorageUnavailable` |
| `run.status` | notification, to every connection after `hello` | (none) | `{ run_id, workspace_id, from, status, end_reason?, exit_code?, at, event_id: i64 \| null }` | (none) | (none) |
| `storage.status` | notification, to every connection after `hello` | (none) | the same `storage` object as `engine.info` | (none) | (none) |
| `dev.synthetic_run` (existing, devtools) | request | unchanged | unchanged, plus `task_id`, `workspace_ids` | devtools build only | unchanged |

New error codes in `sy-protocol` (the reserved block `-32050` to `-32059` becomes **storage**; update the block table in `error.rs`): `StorageUnavailable = -32050`, `StorageRecoverNotNeeded = -32051`, `BackupMissing = -32052`. Run block: `RunLogUnavailable = -32043`. The `StoreError` variants that only feature 8's methods can trigger (`ProjectExists`, `BranchInUse`, `HasLiveRuns` and the rest) get their protocol codes with those methods, not now. `event.since` and `run.log_read` stay legal while storage is `degraded_writes`, and return `StorageUnavailable` only in the three failed open states.

### Value sourcing

| Action | Value | Source |
|---|---|---|
| Open the database | data dir path | `sy-client` resolver (`SWITCHYARD_DATA_DIR` or the OS default), spec 0001 |
| Migrate | newest migration number | the length of a `const MIGRATIONS: &[Migration]` array in `sy-infra`, each entry `include_str!("../migrations/NNNN_name.sql")` plus a `rebuild` flag. A unit test checks that the files in `migrations/` are numbered from 1 with no gaps and that every file is in the array |
| Backup | backup name | `user_version` at the time of the backup, plus `Clock::now_ms()` |
| Startup backup | "newest is older than 24 hours" | the `<unix ms>` in the newest backup's name, compared with `Clock::now_ms()` |
| `engine.info.storage` | `status`, `message` | the daemon's storage health state, set by the open sequence (AC-3) and the write path (AC-13) |
| `engine.info.storage` | `schema_version` | `PRAGMA user_version`, NULL when the file couldn't be opened |
| `engine.info.storage` | `latest_backup` | the file name in `backups/` (not `.tmp`) with the highest `<unix ms>` |
| `storage.recover` | which backup | the newest backup that passes `quick_check`, trying older ones in turn |
| Mark lost (AC-6) | `ended_at` | `Clock::now_ms()`, taken once at engine start (an approximation; the real end is unknown, so feature 27 shows lost runs' time as approximate) |
| Mark lost | live states | the fixed set `starting`, `running`, `waiting_input` (spec decision) |
| Reconcile logs | `log_bytes` | the file length from `RunLog::list()` (0 for no file), which wins over the stored value |
| Reconcile logs | `deleted` | file missing AND `log_state IN ('present','incomplete')` AND stored `log_bytes > 0` |
| Discard stale workspaces | which ones | `workspace.state = 'preparing'` at startup |
| Log cap (AC-10) | the cap | `setting` row `storage.log_cap_bytes`, else the constant 5368709120 |
| Log cap | current total | `Store::log_total_ended` (SUM of `log_bytes` over ended runs with `log_state IN ('present','incomplete')`) plus the hub's appended byte count for each live run |
| Log cap | candidates and order | ended runs whose task has `COALESCE(task.archived_at, project.archived_at)` set and `log_state IN ('present','incomplete')`, ordered by that time ascending, then `run.queued_at`, then `run.id` |
| `run.status` | `event_id` | the committed `event.id` returned by `Store::transition_run`; `null` while `degraded_writes` |
| `run.status` | `from` | the run's status before the transition, read in the same transaction |
| `event.since` | `next_cursor`, `has_more` | the id of the last returned event (or the input cursor when none); `has_more` comes from fetching `limit + 1` rows |
| `run.log_read` | `log_bytes`, `eof` | `RunLog::len(run)` at read time (bytes in the file, which for a live run excludes the unflushed batch); `eof = offset + returned length >= that length` |
| Degraded writes | retry and probe interval | the constant 5 seconds |
| Degraded writes | queue cap | the constant 10000 commands |
| Degraded writes | probe | an upsert of `setting` key `storage.probe` with the current time, never journaled |
| `create_project` | `repo_path` | the caller's value, stored as given. Feature 8 canonicalises it (`std::fs::canonicalize`) before calling |
| Any write | ids, `*_at` | typed id constructors (UUIDv7), and `Clock::now_ms()` |
| `add_workspace`, `create_task` | `workspace.project_id` | `SELECT project_id FROM task WHERE id = ?` inside the same insert |
| Any event | ancestor ids | the row being changed and its parents, read in the same transaction |
| Any event | `actor` | the use case: `user` for client requests, `engine` for startup recovery and trimming, `policy` and `agent` from features 12 and 33 |
| Dev fixture (AC-15) | fixture project | `name = "Dev fixture"`, `repo_path = <data dir>/dev-fixture` (never created on disk, no git), `base_branch = "main"`, found with `find_project_by_repo_path` on later calls |
| Dev fixture | task, workspace, run fields | task `title = "Synthetic runs"`, `prompt = ""`, `base_ref = "main"`; workspace `agent_id = "synthetic"`, `agent_options = {}`, `branch = "dev/<wsp id>"`, `worktree_path = <data dir>/dev-fixture/<wsp id>`, `base_commit = ""`, `state = ready`; run `kind = agent`, `pid` and `pgid` NULL |
| Dev fixture | stop reason | `end_reason = "user_stop"` for `dev.stop_runs` |

### Key invariants

- Only `run` stores a status. Workspace and task status are always derived (the function above).
- A change and its event commit together or not at all, and nothing is broadcast before both commit (AC-5), except while `degraded_writes` (AC-13).
- Log bytes are written at their exact offset before they're delivered or put in the replay ring, and a log file only grows until it's trimmed or deleted (or truncated back to its last good byte after a failed append).
- Terminal run states never change. `lost` is set only by the engine at startup.
- Permanent delete only follows archive, neither is allowed while a run is live, and nothing in this feature changes a repo, worktree or branch.
- A live run's log is never trimmed, and `trimmed` is committed before the file is unlinked.
- Migrations are additive only (the definition above), and the engine refuses to write a database newer than itself.
- The database never holds a secret value. `env_var.secret_ref` and `extension.config` (later) hold Keychain references only.

### Security model

- One user. The socket's peer user check (spec 0001) is the boundary, so every connected client is you. `actor` records who acted, so feature 33's agent actions stay attributable.
- Files: the data dir, `runs/` and `backups/` are `0700`, and the database, WAL, SHM, backups, `.bad-*` files and run logs are `0600`. Each is created with explicit mode bits (`OpenOptionsExt::mode`, `DirBuilderExt::mode`). The database's `-wal` and `-shm` files are covered by the `0700` data dir and checked by a test. The engine never sets a process wide umask, because agents inherit it and would create files in your worktrees with the wrong modes.
- At rest: no SQLCipher. FileVault plus permissions protect the files, and the database holds no secret values.
- Run logs are stored byte exact and may contain a secret an agent printed. Masking on display and export belongs to features 14 and 6, and the diagnostics bundle (feature 6) must leave run logs out unless you opt in per run.
- Store and run log tracing records only the command name, ids, counts and duration (AC-17).
- Compliance scope: none. Everything is local, with no telemetry.

### Observability

- One `tracing` span per store command (`store.command`, fields: name, ids, rows, `duration_ms`). A command over 100 ms logs a `warn`.
- `info` on each migration applied, each backup written or pruned, each recovery, each trim (run id and bytes), and each storage status change. `error` on every failed write, with the SQLite or I/O error code.

### Configuration required

- No new environment variables. `SWITCHYARD_DATA_DIR` (spec 0001) already places the database, `runs/` and `backups/`.
- New global setting: `storage.log_cap_bytes` (integer bytes, default 5 GiB). The UI to change it arrives later. Until then it's set through `Store::set_setting` in tests.

### Critical test scenarios

- Happy path (over the real socket): `dev.synthetic_run { count: 3 }`, attach, read output, `dev.stop_runs`. Rows, events and log files match what the client saw. A second connection that never attached also gets every `run.status`, in `event_id` order, each with an `event_id` that `event.since` returns. The last `run.output` arrives before the `stopped` status. Verifies **AC-5**, **AC-11**, **AC-15**, **AC-18**.
- Hard kill: stream 3 synthetic runs, record the highest offset each client got (live and from `run.attach` replay), `kill -9` the engine, restart. Each log is at least that long, the runs are `lost` with `engine_restart`, and `run.log_read` returns the exact bytes the client saw at the same offsets. Verifies **AC-5**, **AC-6**, **AC-12**.
- Hub ordering (unit, `sy-core`, with a recording `RunLog`): `remove()` racing the flusher on one run never appends out of offset order or delivers a byte before its append. Attach replay never includes unappended bytes. Verifies **AC-5**.
- Migrations: a fresh database reaches the newest version with no backup. A database at version 1, with a test only migration 2, gets a backup that passed `quick_check` and has no `.tmp` left, and with 5 runs only 3 backups remain. A failing migration rolls back, leaves `user_version` unchanged, and gives `unavailable`. A test only `-- rebuild` migration on `run` keeps every event row. File and directory modes are `0600` and `0700`, and a child process the engine spawns sees the umask it started with. Verifies **AC-1**, **AC-2**, **AC-3**.
- Degraded open: set `user_version` to 999, then corrupt the file's header bytes, then make the file unreadable. Each time `engine.info` shows `newer_schema`, `corrupt` or `unavailable`, `event.since` returns `StorageUnavailable`, and the file bytes are unchanged. `storage.recover` with each action restores service. The bad files (database, then `-wal` and `-shm`) and `runs.bad-*` still exist, and no log was swept. Verifies **AC-3**, **AC-4**.
- Invariants (store integration tests on a real temp file): a duplicate repo path, a second live agent run, a branch reused while not discarded (allowed once discarded), a second winner, and a duplicate worktree path each return their `StoreError`. A setup run beside a live agent run is allowed. A workspace always gets its task's project. Verifies **AC-7**.
- Status machine (unit, `sy-core`): every listed transition is accepted, every unlisted pair and `lost` through `transition_run` is rejected with no write, and a same status report returns no event. Verifies **AC-8**.
- Archive and delete: archiving or deleting with a live run fails with `HasLiveRuns`. Deleting an unarchived task fails with `NotArchived`. Archive then delete returns the run ids, and their rows, events and log files are gone. A failure injected after the commit leaves orphan files that the next start removes. Verifies **AC-9**, **AC-6**.
- Startup reconcile: a queued run with no file stays `present`, a finished run whose file vanished becomes `deleted`, and a `preparing` workspace becomes `discarded`. Verifies **AC-6**.
- Log cap: a 1 MiB cap, two archived tasks, one task of an archived project, and one active task, each with 600 KiB of logs. The oldest archived ones are trimmed first, the active one and any live run never are, and each trim commits `trimmed` before the file goes. Verifies **AC-10**.
- Disk full: a `RunLog` and a `Store` test double that return `Full`, wired into the daemon over the real socket. Output keeps streaming, `storage.status` reports `degraded_writes`, and notifications carry `event_id: null`. A `create_run` fails with `Full` and is not queued. The failed run's file is truncated to its last good byte and marked `incomplete`. After the double recovers, the queued commands land in order and the probe returns status to `ok`, including when only the log had failed. Verifies **AC-13**.
- Read edge cases: `event.since` with a negative cursor gives `InvalidParams`, `limit: 0` gives 200, and filters AND together. `run.log_read` on a queued run gives empty data and `eof: true`, and a file unlinked between calls gives `RunLogUnavailable`. Verifies **AC-11**, **AC-12**.
- Boundaries and ids: `pnpm boundaries:check`. Id round trip and wrong prefix rejection unit tests. Verifies **AC-14**, **AC-16**.
- No secrets in logs: run the happy path with a prompt containing a marker string, and assert the marker never appears in `engine.log`. Verifies **AC-17**.
- Auth: there's no per user authorization. A wrong uid peer is already rejected by spec 0001's tested peer check, and nothing here changes it.

## Build plan

Tracer Bullet: first a thin real thread (a dev run whose rows and output survive a `kill -9`, over the real socket), then thicken the store, then the failure handling. The spine migration lands in task 3 and grows only by appending files.

**Milestone 1: persisted thread end to end**
1. `sy-core`: typed ids (prefix plus UUIDv7, parse and display), the `Clock` port, the entities (`Project`, `Task`, `Workspace`, `Run`, `Event`, `EventKind`), the run status machine and the derived display status function, all unit tested. Satisfies **AC-8**, **AC-16**.
2. `sy-core`: the `Store` (`async_trait`) and `RunLog` (synchronous) port traits and `StoreError` and `RunLogError`, with the Milestone 1 methods first (`create_project`, `find_project_by_repo_path`, `create_task`, `add_workspace`, `create_run`, `get_run`, `transition_run`, `events_since`). Satisfies **AC-14**.
3. `sy-infra`: open the connection with the pragmas and explicit file and directory modes, the in house migration runner (the `MIGRATIONS` array, one transaction each, `user_version`, the `-- rebuild` rule, a gap check test), and `0001_spine.sql` with every Migration 1 table, named constraint and index. Then `SqliteStore` on its own thread for the Milestone 1 methods, with constraint errors mapped by index name. Satisfies **AC-1**, **AC-7** (schema), **AC-14**.
4. `sy-infra`: `FileRunLog` (`0600` `pwrite` at offset, truncate, read, len, list, delete). Satisfies **AC-5**, **AC-12**.
5. `sy-core` hub plus `sy-daemon` thread: the typed `RunId` in the hub (`create_run` takes it), append to `pending` only, ring filled after the append, a per run flush lock, and the flusher on its own thread. Compose the store and run log, run the terminal order (final flush, then `transition_run` with `log_bytes`, then notify), and add `run.status` to every connection through one ordered event channel. Make `dev.synthetic_run` and `dev.stop_runs` go through the store with the fixture rows. Hub ordering unit tests, plus socket tests for the happy path and the hard kill. Satisfies **AC-5**, **AC-15**, **AC-18**.

**Milestone 2: recovery and reads**
6. Startup recovery: mark live runs `lost`, reconcile `log_bytes`, flag missing logs as `deleted` (only when `log_bytes > 0`), discard `preparing` workspaces, and sweep orphan, trimmed and deleted run files plus `backups/*.tmp`. Satisfies **AC-6**.
7. `sy-protocol`: the `Event` type and payloads per kind, `event.since`, `run.log_read` with its parameter rules, the `run.status` and `storage.status` types, and the new error codes. Then `pnpm protocol:gen` and the handlers. Satisfies **AC-11**, **AC-12**, **AC-18**.

**Milestone 3: the complete spine store**
8. The rest of the Migration 1 `Store` methods: settings and `settings_revision`, archive and permanent delete (`HasLiveRuns`, `NotArchived`, rows then files from the returned run ids), the get and list methods with keyset pagination, and every remaining constraint mapping. Integration tests for each invariant. Satisfies **AC-7**, **AC-9**.
9. Log cap trimming at startup, at run end and after archive: ended runs of archived tasks or projects only, `trimmed` committed before the unlink. Satisfies **AC-10**.

**Milestone 4: storage failure handling**
10. Crash safe backups (to `.tmp`, `quick_check`, rename, keep 3; before migrating and when the newest is over 24 hours old). Degraded open with the status mapping, `engine.info.storage`, the `StorageUnavailable` gate, and `storage.recover` (renaming `-wal` and `-shm` first, moving `runs/` aside, picking the newest valid backup). Satisfies **AC-2**, **AC-3**, **AC-4**.
11. Write failure handling: `degraded_writes`, stop and truncate on a failed append, the in memory queue for transitions, `set_*` and `append_event` only (cap 10000), creates failing with `Full`, the 5 second retry plus probe, `event_id: null`, and the `storage.status` notifications. Satisfies **AC-13**.
12. Tracing spans and slow command warnings, the WAL checkpoint at shutdown, the no secret logging test, and the file mode and child umask tests. Satisfies **AC-1**, **AC-17**.

## Consequences

**Positive**:
- Compare, approvals, cost, queue, history, resume, PRs, CI, templates and extensions all have a designed home, and every later change is additive.
- Crash safety you can test: what a client saw is on disk, and a `kill -9` test proves it in CI.
- One journal serves history (feature 30), reconnect catch up (`event.since`) and debugging, with no event sourcing machinery.
- Raw output is cheap to append, trim and delete, and file offsets equal protocol offsets.
- Storage failures explain themselves, and your history is never discarded without your say.

**Negative / tradeoffs**:
- Two stores (SQLite and log files) can drift after a crash. Startup reconciliation (AC-6) is required code, not optional.
- Every output batch pays one `write(2)` before delivery, and every status change a commit. That's small at 12 runs, but it sits on the latency path the scaffold's 100 ms check measures, so run that check again after task 5.
- The journal grows with every message, command and file change, and isn't trimmed. It's small next to logs, but feature 30 should measure it.
- The later tables are designed before their features exist, so a feature may find a detail wrong. Fixing it must stay additive, or go through `/architect`.
- `degraded_writes` knowingly breaks persist before broadcast. An engine death while degraded loses the queued changes, a run whose append failed keeps only its log prefix, and creates are refused until space frees.
- A permanent delete of a very large task runs as one transaction on the single database thread, so other store calls wait for it (a short stall, accepted until measured).
- A merged workspace keeps its branch name reserved in that project, even after you delete the branch in git.
- Lost runs' `ended_at` is the engine's restart time, so their duration (feature 27) is an upper bound, not exact.
- The hub change (ring after append, a lock per run) touches spec 0001's tested streaming code, so its tests and the 12 stream load check must pass again.
- Raw logs can hold secrets an agent printed, protected only by file modes and FileVault.
- TEXT UUID keys are larger than integer rowids, a cost accepted for readable, globally unique ids.
- `workspace.agent_id` has no FK, so a removed generic agent leaves old workspaces naming an id that no longer exists (by design, the snapshot keeps history accurate).
- Degraded mode adds a second engine mode that every new method must respect (the storage gate).

**Neutral**:
- `sy-core` gains a `uuid` dependency (with the `v7` feature), and `sy-infra` gains `rusqlite` (`bundled`). Run `cargo deny` when adding them.
- `PRAGMA quick_check` runs on every start. Measure it once real databases grow, and switch to checking only after an unclean shutdown if it gets slow.
- Spec 0001's "run output log layout and retention (feature 3)" follow up is settled here.

## Follow-up

- [ ] Feature 4 (adapter contract): adapters report `message`, `command_run` and `file_changed`, and the agent's resume handle (`agent_session_ref`), through the adapter API, so the engine journals them.
- [ ] Feature 5 (supervisor): if a per agent holder process lands, restate when a run is `lost` versus reattachable, because AC-6 assumes an engine restart ends every live run.
- [ ] Feature 23 (crash safe resume): needs its own `/architect` pass on reattach. Reattaching a run breaks "terminal states never change" and the meaning of `lost`, so decide it as a design change, not a silent migration. Stop or reattach leftover processes using `pid`, `pgid`, `pid_started_at` and `agent_session_ref`.
- [ ] Features 5 and 23: signal or adopt leftover agent processes after an engine crash. Feature 3 records them but never signals them.
- [ ] Feature 13: decide whether `script.source = 'repo_file'` scripts are imported into rows or read live.
- [ ] Feature 8 (core task loop): add `project.*`, `task.*` and `workspace.*` methods, and protocol codes for the `StoreError` variants they surface.
- [ ] Feature 6 (logging): keep run logs out of the diagnostics bundle by default, and mask known secrets on export.
- [ ] Feature 14 (env and secrets): mask known secret values when displaying run output.
- [ ] Feature 30 (history): add the FTS5 table over `message` events, unarchive, and measure journal growth.
- [ ] A settings UI for `storage.log_cap_bytes` (feature 7 or 30), and a disk usage readout.
- [ ] After task 5, run again spec 0001's scaffold load check (12 streams, p95 under 100 ms) with persist before broadcast on.
