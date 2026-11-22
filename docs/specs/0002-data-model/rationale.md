# 0002. Data model: decision record

The reasoning behind [index.md](index.md). `/develop` builds from the index and skips this file.

## Context

switchyard's engine (`switchyardd`, spec 0001) is the only owner of agents, terminals, git and the local database. Every later feature reads or writes local records: the core task loop (8) needs projects, tasks, worktrees and runs; the board (9) and notifications (10) need live status; approvals (12), queue limits (26) and cost (27) need their own records; history (30) needs each task's conversation, commands and changed files; resume (23) needs exact status and process details after a crash; compare (22), PRs (28), CI (29), templates (31) and extensions (32) each add their own nouns. The scope sets the bar for this feature: support every planned slice without a breaking migration, keep everything local, and make sure a hard kill loses nothing the app already showed.

Some forces are already fixed. Spec 0001 chose SQLite through rusqlite (bundled), WAL with `synchronous=NORMAL`, plain SQL migrations, exactly one connection on a dedicated thread, and one owner (the engine), and clients never open the file. It also fixed the terminal stream: per run byte offsets, a 2 MiB replay ring buffer, 16 ms batches, and a producer that never waits on a client. And it left one question open for this feature: where and how long the full output lives on disk. The ports and adapters rules (`sy-core` defines ports, `sy-infra` implements them, no rusqlite in the core) decide where the code sits.

Volume is modest but bursty: one person, a dozen agents streaming at once, each producing anywhere from kilobytes to hundreds of megabytes of terminal output over hours, plus a steady trickle of status changes and structured events. The data is personal and local, with no telemetry. It can include secrets an agent happens to print, while secret values the user configures belong in the Keychain.

Not deciding now is the expensive path. The data model is the costliest thing to redo, and every slice from 1 onward writes into it. A model shaped only for slice 1 would force a destructive migration of real user history by slice 5 (compare) or slice 6 (resume).

## Options considered

### Option 1: Relational state rows, an append only event journal, and flat per run log files; the full design, with the spine migrated now

Normalised tables hold current state (project, task, workspace, run and the later tables). Each change also writes a typed row to one `event` table in the same transaction. Raw terminal bytes go to `runs/<run_id>.log`. The spec designs every entity through slice 12, but migration 1 creates only the spine the tracer bullet exercises. Later features add their designed tables additively.

**Pros**:
- The current state is a plain indexed query (the board, the approvals inbox, the queue), and history is a plain ordered query over the journal.
- Relational constraints (FKs, partial unique indexes) enforce the important invariants in the database itself.
- Output stays out of SQLite, so appending, trimming and deleting are file operations, and offsets match the protocol exactly.
- No table sits in the schema untested until its feature arrives.

**Cons**:
- Two stores to keep consistent after a crash (rows and files), so startup reconciliation is required.
- Every change is written twice (row and event), and the journal grows without trimming.
- Later tables are designed ahead of their features and may need additive corrections.

### Option 2: Event sourcing

Events are the only source of truth. Current state is a projection kept in memory or in derived tables, rebuilt from the journal at startup.

**Pros**:
- A complete, perfectly consistent audit trail by construction.
- Any new read model can be built from history later.

**Cons**:
- Every read model, schema change and crash recovery becomes a projection problem (rebuild time grows with history, and snapshots are needed).
- Constraints like "one live agent run per workspace" can't be a unique index. They become application logic racing against itself.
- A large up front cost for a solo build, with failure modes few contributors know.

### Option 3: Minimal spine now, evolve per feature

Design and create only what slice 1 needs (project, task, run, output), and let each feature reshape the schema as it lands.

**Pros**:
- The smallest spec and the fastest first build.
- No risk of designing tables for features whose needs change.

**Cons**:
- Compare (one task, many worktrees), follow ups and resume each restructure the core tables, and that's exactly the breaking migration the scope forbids, on real user history.
- Each feature designs its piece in isolation, so the model drifts and status and history end up in several shapes.

### Option 4: Document store inside SQLite

One table per aggregate holding a JSON document (a task with its workspaces and runs nested), queried with SQLite's JSON functions.

**Pros**:
- Shape changes need no migration.
- One read fetches a whole task tree.

**Cons**:
- The domain is clearly relational (cross task queues, a cross agent inbox, totals per project), so JSON queries and expression indexes do a worse job than real columns.
- No FK or unique constraints across documents, so invariants move into code.
- Concurrent updates to one task document (several runs streaming status) cause read, modify, write conflicts on the single writer.

## Rationale

The pull between "support every slice without a breaking migration" and Tracer Bullet's "thin real thread first" is resolved by separating design from creation. Option 1 designs the whole model now, so slices 2 to 12 have a home whose shape is already agreed, and migrates only what the first thread exercises. Option 3 gets the thread right but forces breaking restructures, and creating every table now (the other variant of Option 1) leaves months of untested schema that drifts from what its feature really needs. The "additive only" rule turns the scope's no breaking migration bar into something a reviewer can check on every later migration.

Relational state plus a journal, rather than event sourcing (Option 2) or documents (Option 4), follows from the shape of the reads. The board, inbox and queue ask questions across tasks and agents, and the invariants that matter most (one live agent per workspace, a unique branch, a single winner) are exactly what partial unique indexes enforce for free on SQLite's single writer. The journal gives feature 30 its history and the protocol its reconnect cursor, without making every read a projection. Writing the row and event in one transaction, and broadcasting only afterwards, is what makes "a hard kill loses nothing already shown" true and testable. WAL with `synchronous=NORMAL` (spec 0001) already survives a process kill, so no fsync is needed.

Output goes to flat files because it's the one high volume, append only, byte addressed stream. Keeping it out of SQLite keeps the single writer thread free for state changes, lets trimming and deletion be a file unlink, and makes a file position equal the protocol offset. The price is reconciling two stores after a crash, which is a small, well defined startup pass (AC-6).

## Choices made in the design conversation

Each line is the pick, why, and the runner up.

- **Domain shape**: Task → Workspaces → Runs. A comparison is N workspaces, and a follow up or resume is a new run. Runner up: workspace first with a separate comparison entity.
- **Build breadth**: full design, spine migrated. Runner up: create every table now.
- **Durability**: persist before broadcast. Runner up: fsync everything (rejected as too slow for an accepted power loss risk).
- **Deletion**: archive, then explicit permanent delete, for tasks and projects alike. Runner up: delete removes everything.
- **History**: state rows plus one event journal. Runner up: full event sourcing.
- **Log retention**: keep, with a 5 GiB cap that trims archived tasks' logs only. Runner up: keep until deleted.
- **Settings home**: SQLite rows, versioned by a per project revision and the journal. A repo file source is left to feature 13 (`script.source`). Runner up: a repo file as the source of truth.
- **Status model**: only `run` stores status, and parents derive it. Runner up: stored on every level.
- **Ids**: prefixed UUIDv7 TEXT. Runner up: plain UUIDv7.
- **Journal scope**: one table at every level, with a global id cursor. Runner up: per run only.
- **Conversation**: `message` events reported by adapters, with the raw log as the fallback. Runner up: a dedicated message table.
- **Agent reference**: `agent_id` plus an options snapshot, with no FK. Runner up: an FK to an agent row.
- **Approvals**: their own table plus the journal. Runner up: journal events only.
- **Usage**: one `run_usage` row per run, where NULL means unknown. Runner up: usage samples over time.
- **Money**: integer micro units plus an ISO 4217 currency. Runner up: REAL dollars.
- **Timestamps**: INTEGER Unix ms UTC. Runner up: ISO 8601 TEXT.
- **Database invariants**: one live agent run per workspace, branch unique per project, one winner per task, repo path unique (all four chosen).
- **Remove project**: archive, then explicit delete. Runner up: always delete permanently.
- **Log layout**: one flat file per run. Runner up: fixed size segments.
- **Payloads**: JSON keyed by kind. Runner up: typed columns per kind.
- **Migrations**: a small in house runner on `user_version`. Runner up: the `rusqlite_migration` crate (it pins a rusqlite major).
- **Newer schema**: refuse to write, and explain. Runner up: open read only.
- **Backups**: `VACUUM INTO` before migrating, keep 3. Runner up: no backup.
- **References**: none.
- **Protocol surface**: wire the store into the daemon, prove it through `dev.synthetic_run`, and add `event.since` and `run.log_read`. Runner up: no protocol change.
- **Store port**: command methods, one transaction each, returning the committed event. Runner up: repositories plus a unit of work.
- **Storage error at start**: a degraded engine that reports it. Runner up: exit and log.
- **Notifications**: the named notifications carry `event_id`. Runner up: one generic `event.appended`.
- **Secrets in logs**: store raw, lock down the files, mask on use. Runner up: mask on write.
- **At rest**: FileVault plus file modes. Runner up: SQLCipher.
- **Disk full**: keep the agent running, flag it, and retry. Runner up: pause the agent.
- **Corrupt database**: degraded mode, and you choose to restore or start fresh. Runner up: restore the newest backup automatically.

**Implementation details decided while writing** (runner up in parentheses):
- Ids are generated with `uuid::Uuid::now_v7()` in `sy-core`, because they're opaque and no test needs a fixed value (an id generator port).
- `SqliteStore` is one connection on its own OS thread with a bounded command channel and oneshot replies, which is spec 0001's rule (a pool, refused until measured).
- The hub flusher moves to its own OS thread, because the append is a blocking `write(2)` (`spawn_blocking` per batch, too much churn every 16 ms).
- Lists use keyset pagination on `(created_at, id)`, capped at 500 (offset pagination, which slows down as history grows).
- Event kinds are versioned by adding fields as optional only, and a breaking shape becomes a new kind (a `v` field in each payload).
- `run.log_read` returns at most 256 KiB per call, which stays well inside the 1 MiB line limit after base64 (1 MiB raw, which breaks the line limit).
- `PRAGMA quick_check` runs on every start, until measurement says otherwise (only after an unclean shutdown, which needs a marker file).
- Store commands that fail with disk full are retried every 5 seconds, in order, from memory (dropping them, which loses status changes).
- Tables are `STRICT` with `CHECK` constraints on enums (loose typing, which lets bad values in quietly).
- The stored `lost` state is new, beyond the adapter's four statuses. Adapters never report it; only the engine sets it at startup.

## Cross check (2026-11-22)

An independent read only review on a different model found 22 gaps. You chose to apply every recommended fix. The ones that changed the design:
- **Replay and ordering.** The existing hub put bytes in the replay ring before they were flushed, and `remove()` could flush at the same time as the flusher. The ring now fills only after the append, each run has its own flush lock, and `RunLog::append` writes at an explicit offset.
- **No FK rebuilds.** `task.template_id` and `run.script_id` left migration 1. Their features add them with `ALTER TABLE ADD COLUMN … REFERENCES`, because rebuilding a parent table with foreign keys on cascade deletes history. A `-- rebuild` rule covers the one case that still needs a copy (widening a `CHECK`).
- **Degraded writes.** Only transitions, `set_*` calls and adapter events are queued. Creates fail at once, a failed log stops at its last good byte, and a probe write brings status back to `ok`.
- **Recover without loss.** `runs/` is moved aside instead of swept, backups are written to `.tmp` and checked before the rename, and `-wal` and `-shm` are renamed before the database.
- **Smaller settled points.** `event.project_id` is nullable for global events, payloads are defined per kind, `starting` can go straight to `waiting_input`, `finished` or `failed`, `HasLiveRuns` guards archive and delete, `ProjectMismatch` became impossible (the project id is copied from the task), stale `preparing` workspaces are discarded at startup, there's no process umask, and notifications are broadcast to every connection in `event_id` order.
