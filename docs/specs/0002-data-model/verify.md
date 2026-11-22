# Verify: Data model · spec 0002 · updated 2026-11-22 (Milestone 4 added)
_Steps derived from spec 0002 acceptance criteria. `/check verify` runs these; `/test` locks the durable ones._

## Milestone 1: persisted thread end to end

### Manual (engine with devtools, `SWITCHYARD_DATA_DIR=/tmp/sy-check`)
- [x] Start the engine on an empty data dir → `switchyard.db` appears, `PRAGMA journal_mode` is `wal`, `PRAGMA user_version` is `1`, and `backups/` is empty (a fresh database makes no backup) → AC-1
- [x] `stat -f %Lp` on the data dir, `runs/` and `backups/` gives `700`, and on `switchyard.db`, `-wal`, `-shm` and each `runs/*.log` gives `600` → AC-1
- [x] Widen the data dir to `755`, restart the engine → it is `700` again → AC-1
- [x] Open the database (sqlite-mcp or `sqlite3`): tables `project`, `project_setting`, `setting`, `task`, `workspace`, `run`, `event` exist, all `STRICT`, with the indexes `uq_project_repo_path`, `uq_run_live_agent`, `uq_workspace_branch`, `uq_workspace_winner`, `uq_workspace_worktree_path` → AC-1, AC-7
- [x] Connect two clients, both `session.hello`. On client A call `dev.synthetic_run { count: 3, bytes_per_sec: 50000, echo: false }` and attach to each run. Client B never attaches → B still gets `run.status` `starting`, `running` (and later `stopped`) for all 3 runs, each with an `event_id`, in rising `event_id` order → AC-18
- [x] The `dev.synthetic_run` result carries `task_id` and 3 `workspace_ids`, and each `run_ids` entry looks like `run_<32 lowercase hex>` → AC-15, AC-16
- [x] Database after that call: one project named `Dev fixture` with `repo_path = <data dir>/dev-fixture` and nothing created at that path on disk; one task `Synthetic runs` with `prompt = ''` and `base_ref = 'main'`; 3 `ready` workspaces with `agent_id = 'synthetic'`, `branch = 'dev/<wsp id>'`, `worktree_path = <data dir>/dev-fixture/<wsp id>`; 3 `agent` runs in `running` with `pid` and `pgid` NULL → AC-15
- [x] Call `dev.synthetic_run` a second time → still one project (found by its path), now 2 tasks → AC-15
- [x] Call `dev.stop_runs` on A → for each run, the last `run.output` A receives comes before that run's `stopped` status; the run row is `stopped` with `end_reason = 'user_stop'` and `ended_at` set, and `log_bytes` equals the size of `runs/<run_id>.log` → AC-5
- [x] Compare each `runs/<run_id>.log` with what A showed (attach replay plus live chunks) → byte for byte equal, at the same offsets → AC-5
- [x] For each `run.status` B received, `SELECT kind FROM event WHERE id = <event_id>` gives `run_status_changed`, with `run_id`, `workspace_id`, `task_id` and `project_id` all filled in → AC-5, AC-18
- [x] Stream 3 runs at 200000 B/s with A attached, note the highest `offset + length` A got per run, then `kill -9` the engine → each log file is at least that long and starts with exactly the bytes A saw; every `event_id` A saw is in `event`; the engine starts again cleanly on the same data dir → AC-5
- [x] Time to first byte and smoothness: run spec 0001's load check (12 streams in the Load check screen, p95 under 100 ms) with persist before broadcast on → AC-5 (spec 0002 Consequences)

### Commands
- [x] `cargo test -p sy-core` → passes with no infrastructure: ids, the status machine (every listed transition accepted, every other pair and `lost` refused, same status is a no op), display status, the hub ordering tests → AC-8, AC-14, AC-16
- [x] `cargo test -p sy-infra` → passes against real temp files: pragmas, schema, file modes, every AC-7 constraint the Milestone 1 methods reach, the migration gap check, a failing migration rolling back, the `-- rebuild` rule keeping every event row → AC-1, AC-7, AC-14
- [x] `cargo test -p sy-daemon --features devtools` → passes, including `synthetic_runs_should_be_saved_and_every_client_should_get_run_status_in_event_order` and `kill_9_should_lose_nothing_a_client_already_saw` → AC-5, AC-15, AC-18
- [x] `pnpm boundaries:check` → passes; `cargo tree -p sy-core -e normal` shows no `rusqlite` → AC-14
- [x] `pnpm protocol:check` → the schema and TS types match (`run.status`, `RunStatusChanged`, `TaskId`, `WorkspaceId`) → AC-18

### Value sourcing rows (Milestone 1)
- [x] Data dir path: start with `SWITCHYARD_DATA_DIR` set to two different dirs → each gets its own `switchyard.db` and `runs/`
- [x] Newest migration number: `user_version` equals the length of `MIGRATIONS` (1 now); add a test only migration 2 → `user_version` becomes 2
- [x] `run.status.event_id`: equals the committed `event.id` for that change (not a counter of its own)
- [x] `run.status.from`: equals the run's status before the change (`queued` for the first `starting`)
- [x] Ids and `*_at`: every id is `<prefix>_<uuidv7 hex>`; every `*_at` is Unix ms UTC, close to the wall clock
- [x] `workspace.project_id`: always equals its task's `project_id`, whatever the caller passes
- [x] Event ancestor ids: a run event has its run, workspace, task and project ids; a task event has only task and project
- [x] Event `actor`: `user` for every row written by `dev.*` requests
- [x] Dev fixture fields: as in the manual step above, and `dev.stop_runs` sets `end_reason = 'user_stop'`

## Milestone 2: recovery and reads

### Manual (engine with devtools, `SWITCHYARD_DATA_DIR=/tmp/sy-check`)
- [x] Stream 3 synthetic runs at 200000 B/s with a client attached, note the highest `offset + length` it got per run, then `kill -9` the engine and start it again → each run row is `lost` with `end_reason = 'engine_restart'`, all 3 share one `ended_at` (the new engine's start time), and `log_bytes` equals the size of `runs/<run_id>.log` → AC-6
- [x] After that restart, `event.since { cursor: 0, run_id: <run> }` → the last event is `run_status_changed` with `actor: "engine"`, `from: "running"`, `status: "lost"`, `end_reason: "engine_restart"`; exactly one such event per lost run → AC-6, AC-11
- [x] After that restart, page `run.log_read { run_id, offset, max_bytes: 65536 }` from offset 0 until `eof: true` → the bytes equal what the client saw, at the same offsets, and the last page's `log_bytes` equals the file size → AC-5, AC-12
- [x] With the engine stopped, add `runs/run_<valid uuidv7 hex>.log` (no row), `runs/notes.txt` and `backups/switchyard-v1-1.db.tmp`, then start → the orphan log and the `.tmp` are gone, `notes.txt` is still there → AC-6
- [x] With the engine stopped, delete the log file of a `stopped` run that had output, then start → its row has `log_state = 'deleted'` and `log_bytes = 0`; `run.log_read` on it gives `RunLogUnavailable` (-32043) → AC-6, AC-12
- [x] With the engine stopped, set one workspace to `preparing` and add one `queued` run (sqlite3), then start → the workspace is `discarded` with `closed_at` = the start time and one `workspace_state_changed` event (`preparing → discarded`, actor `engine`); the queued run is still `queued` with `log_state = 'present'` → AC-6
- [x] Start the engine a second time with nothing changed → no new events, no rows change → AC-6
- [x] `event.since { cursor: -1 }` → `InvalidParams` (-32602); a filter id that isn't a valid id → `InvalidParams`; a cursor past the last id → `{ events: [], next_cursor: <that cursor>, has_more: false }` → AC-11
- [x] After two `dev.synthetic_run { count: 64 }` calls (over 500 events): `limit` absent or 0 → 200 events and `has_more: true`; `limit: 10000` → 500 events; ids ascend and `next_cursor` is the last one → AC-11
- [x] `event.since` with `task_id` of call 1 plus `run_id` of one of its runs → only that run's `run_created` and two `run_status_changed`, each with all four scope ids; with `task_id` of call 1 plus a run of call 2 → empty (filters AND) → AC-11
- [x] For each `run.status` a client got, the `event.since` event with that `id` has `kind: "run_status_changed"` and a `payload` equal to the notification without `event_id` → AC-11, AC-18
- [x] Stop a run that wrote over 256 KiB: `run.log_read` with `max_bytes` 0, absent, or 4294967295 → exactly 262144 bytes and `eof: false`; an offset past the end → `data_b64: ""`, `eof: true`, `log_bytes` = file size → AC-12
- [x] While a run is still live, `run.log_read` from 0 → `log_bytes > 0` and a later read shows more (`eof` meant "end so far") → AC-12
- [x] `run.log_read` for a well formed id with no row, and for `run-1` → `RunNotFound` (-32040); `event.since` and `run.log_read` before `session.hello` → `HelloRequired` → AC-12

### Commands
- [x] `cargo test -p sy-core` → passes with no infrastructure: `Run::lose`, the workspace lifecycle table, `LogState::reconcile`, `read_run_log` (no file yet, vanished file, trimmed or deleted, incomplete prefix, paging and `eof`), hub `log_len` → AC-6, AC-12, AC-14
- [x] `cargo test -p sy-infra` → the `recovery_should_*` tests and `backup_temps_should_be_swept_and_finished_backups_kept` pass against real SQLite and real files → AC-6
- [x] `cargo test -p sy-daemon --features devtools` → passes, including `kill_9_should_lose_nothing_a_client_already_saw`, `restart_should_sweep_orphan_logs_and_unfinished_backups`, `event_since_should_default_to_200_cap_at_500_and_and_its_filters` and `log_read_should_cap_its_page_and_refuse_a_vanished_log` → AC-6, AC-11, AC-12
- [x] `pnpm protocol:check` → matches, with `event.since`, `run.log_read`, `storage.status`, `Event`, `EventPayload`, `StorageInfo` and the codes `RunLogUnavailable`, `StorageUnavailable`, `StorageRecoverNotNeeded`, `BackupMissing` → AC-11, AC-12, AC-18

### Value sourcing rows (Milestone 2)
- [x] Mark lost `ended_at`: `Clock::now_ms()` taken once at start → every run lost in one restart has the same `ended_at`, and it is at or after the restart time, not the time of the last output
- [x] Mark lost live states: only `starting`, `running`, `waiting_input` → a `queued` run stays `queued`; a `finished` run is untouched
- [x] Reconcile `log_bytes`: the file length wins → a run killed while live (stored `log_bytes = 0`) gets its file's real size
- [x] Reconcile `deleted`: file missing AND `log_state` present or incomplete AND stored `log_bytes > 0` → a run that never wrote output and has no file stays `present`
- [x] Discard stale workspaces: `state = 'preparing'` at startup → a `ready` workspace is untouched
- [x] `event.since` `next_cursor` and `has_more`: the last returned id (or the input cursor when empty); `has_more` true exactly when one more event matches
- [x] `run.log_read` `log_bytes` and `eof`: the file length at read time → for a live run, a second read soon after reports a larger `log_bytes`

## Milestone 3: the complete spine store

### Manual (engine with devtools, `SWITCHYARD_DATA_DIR=/tmp/sy-check`)
- [x] `dev.synthetic_run { count: 1 }`, then `dev.stop_runs` → the run is `stopped` and `runs/<id>.log` is not empty → AC-10 (setup)
- [x] In the database (sqlite-mcp or `sqlite3`): set that run's `task.archived_at`, and insert `setting` `storage.log_cap_bytes` = `1` → no change yet, the check runs on the next trigger → AC-10
- [x] Run and stop a second synthetic run → the first run becomes `log_state = trimmed` with `log_bytes = 0`, its log file is gone, and `event.since { run_id }` has a `run_log_trimmed` from `engine`. The second run (its task is not archived) keeps `present` and its file → AC-10
- [x] Stop the engine, archive the second run's task in the database, start the engine → the startup check trims it → AC-10
- [x] `event.since { cursor: 0 }` → every event has a known `kind` (the setting kinds `project_setting_changed` and `setting_changed` included) and the run events carry every ancestor id → AC-7, AC-11

### Commands
- [x] `cargo test -p sy-infra --test store` → passes: a second winner is `WinnerExists`, an outcome on a one workspace task is `NotAComparison`, merged → ready is refused with no event, settings bump `settings_revision` and journal `{ key, value, settings_revision }`, `storage.probe` writes no event, archive with a queued or live run is `HasLiveRuns`, delete before archive is `NotArchived`, delete removes rows, events and logs (never the repo or worktree), an orphan left by a crash is swept at the next start, keyset pages cover every row once, and the log cap trims in order and commits `trimmed` before each unlink → AC-7, AC-9, AC-10
- [x] `cargo test -p sy-core` → passes: the workspace lifecycle table, the outcome rule, the JSON value round trip, and which events trigger the log cap → AC-7, AC-10, AC-14
- [x] `cargo test -p sy-daemon --features devtools --test engine log_cap` → passes: a run end and a restart each trigger the check over the real engine → AC-10
- [x] `pnpm protocol:check` → matches, with `project_setting_changed` and `setting_changed` in `EventPayload` and `value: unknown` in the TS types → AC-11

### Value sourcing rows (Milestone 3)
- [x] Log cap: `setting` `storage.log_cap_bytes`, else 5368709120 → unset, or set to a non numeric value, gives the 5 GiB default; set to `650` gives 650
- [x] Log cap current total: SUM of `log_bytes` over ended `present` or `incomplete` runs, plus the hub's appended bytes per live run → a live run's bytes push the total over, but its log is never trimmed
- [x] Log cap candidates and order: `COALESCE(task.archived_at, project.archived_at)` ascending, then `queued_at`, then `id` → a task archived before its sibling's project was archived goes first; a task of an archived project counts as archived even with its own `archived_at` NULL
- [x] `add_workspace`, `create_task` `workspace.project_id`: copied from the task → a workspace always reports its task's project (already covered in Milestone 1, still holds after delete and archive)

## Milestone 4: storage failure handling

### Manual (engine with devtools, `SWITCHYARD_DATA_DIR=/tmp/sy-check`)
- [x] Start the engine on an empty data dir, then stop it → `backups/` is empty (a fresh database makes no backup) → AC-2
- [x] Start it again, then stop it → `backups/switchyard-v1-<unix ms>.db` exists, is `0600`, opens in `sqlite3` with `PRAGMA quick_check` = `ok`, and no `.tmp` is left; `switchyard.db-wal` is empty after the clean stop → AC-1, AC-2
- [x] Leave a `backups/junk.db.tmp`, start the engine → it is gone → AC-2
- [x] `engine.info` (before `session.hello`) → `storage { status: "ok", message: "", schema_version: 1, latest_backup: "switchyard-v1-…db" }` → AC-3
- [x] Stop the engine, `sqlite3 switchyard.db 'PRAGMA user_version=999'`, note `shasum switchyard.db`, start it → `engine.info.storage.status` is `newer_schema` with `schema_version: 999`; after `session.hello`, `event.since`, `run.attach` and `dev.synthetic_run` each return `StorageUnavailable` (-32050); `shasum` is unchanged → AC-3
- [x] `storage.recover { action: "restore_backup", confirm: false }` → `InvalidParams` → AC-4
- [x] `storage.recover { action: "restore_backup", confirm: true }` → the response has `storage.status: "ok"`, a `storage.status` notification arrives, and `event.since` works with no restart. The data dir now has `switchyard.db.bad-<ms>` (same `shasum` as before, `0600`), the `-wal` and `-shm` set aside the same way when they existed, and `runs.bad-<ms>/` with the old logs; `runs/` is new and empty → AC-4
- [x] Call `storage.recover` again → `StorageRecoverNotNeeded` (-32051) → AC-4
- [x] Stop, overwrite the first 16 bytes of `switchyard.db`, start → `corrupt`; `storage.recover { action: "start_fresh", confirm: true }` → `ok` with an empty database at `schema_version: 1` → AC-3, AC-4
- [x] Stop, `chmod 000 switchyard.db`, start → `unavailable`; `restore_backup` → `ok`, and the set aside file is `0600` → AC-3, AC-4
- [x] On a data dir with no backups, corrupt the file, start, `restore_backup` → `BackupMissing` (-32052) and nothing was renamed → AC-4
- [x] Start with `SWITCHYARD_DEV_FAULTS=/tmp/sy-faults` (an empty dir), `dev.synthetic_run { count: 1, bytes_per_sec: 200000 }`, attach, then `touch /tmp/sy-faults/store /tmp/sy-faults/run_log` → a `storage.status` with `degraded_writes` and a message arrives, and `run.output` keeps streaming → AC-13
- [x] While degraded: `dev.synthetic_run` fails with a "disk is full" message (creates are never queued); `dev.stop_runs` answers, and a second connection that never attached gets `run.status` `stopped` with `event_id: null` → AC-13
- [x] `rm /tmp/sy-faults/*` → within about 5 seconds `storage.status` returns to `ok`, no second `stopped` notification arrives, `event.since { run_id }` lists `starting`, `running`, `stopped` in order, and the run row is `stopped`, `log_state = incomplete`, with `log_bytes` equal to the log file's length (a prefix of what the client saw) → AC-13
- [x] Again with only `run_log` failing → `degraded_writes`, then `ok` at the next retry while the file is still there (the probe writes the database); a later stop carries a numeric `event_id` and the run is `incomplete` → AC-13
- [x] Start with `SWITCHYARD_LOG=trace`, an echo synthetic run, send `run.input` with a marker string, stop → `engine.log` holds neither the marker nor its base64 → AC-17

### Commands
- [x] `cargo test -p sy-infra --test storage` → passes: no backup for a fresh database, a checked `0600` backup before a pending migration with no `.tmp`, only the 3 newest after 5, the 24 hour rule, a failed migration rolls back and keeps its backup, an older valid backup is chosen over a bad newer one, newer schema, damaged header and unreadable file map to their errors with the file untouched, the set aside order, private modes under umask 0, and the write queue (in order replay, unjournaled announcements, creates refused with `Full`, the status machine still applies, the log only failure, the 10000 cap, reads keep working) → AC-1, AC-2, AC-3, AC-4, AC-13
- [x] `cargo test -p sy-daemon --test engine` → passes the release build storage tests: ok `engine.info.storage`, the shutdown checkpoint, recover refusals, newer schema, corrupt, unreadable, backup missing, and no code changes the umask → AC-1, AC-3, AC-4
- [x] `cargo test -p sy-daemon --features devtools --test engine` → passes the disk full, log only and no secret logging socket tests → AC-13, AC-17
- [x] `pnpm protocol:check` → matches, with `storage.recover`, `RecoverAction` and the optional `EngineInfo.storage` in the TS types → AC-3, AC-4
- [x] `pnpm release:check` → a release engine still answers no `dev.*` method (fault injection exists only in devtools) → AC-13

### Value sourcing rows (Milestone 4)
- [x] Backup name: `user_version` at backup time plus `Clock::now_ms()` → a backup taken before migrating from version 1 is named `switchyard-v1-…`, not v2
- [x] Startup backup "older than 24 hours": the `<unix ms>` in the newest backup's name → touching the file's mtime changes nothing; renaming it to an older ms makes the next start write one
- [x] `engine.info.storage` `status`, `message`: the open sequence and the write path → each failed open state and `degraded_writes` show their own status, with a message that holds no row data
- [x] `engine.info.storage` `schema_version`: `PRAGMA user_version` → 999 for a newer file, absent for a corrupt or unreadable one, the version before a failed migration
- [x] `engine.info.storage` `latest_backup`: the highest `<unix ms>` in `backups/` (not `.tmp`) → a `.tmp` with a higher ms is never reported
- [x] `storage.recover` which backup: the newest that passes `quick_check`, trying older ones → with a garbage newest file, the older valid one is restored
- [x] Degraded writes retry interval: the constant 5 seconds → `ok` returns within about 5 seconds of freeing space, never instantly
- [x] Degraded writes queue cap: the constant 10000 → past it `append_event` is dropped while transitions still queue
- [x] Degraded writes probe: an upsert of `setting` `storage.probe`, never journaled → after recovery the row exists and `event.since` has no `setting_changed` for it
- [x] `run.status` `event_id`: `null` while `degraded_writes` → only the notifications sent while degraded are null

## Acceptance criteria coverage
- AC-1 … manual steps 1 to 4, `sy-infra` tests (open, pragmas, migrations, modes), Milestone 4 manual step 2, the umask 0 test, and the shutdown checkpoint and no umask socket tests
- AC-5 … manual steps 9, 10, 11, 12, 13, `sy-core` hub tests, `sy-daemon` socket tests
- AC-7 … manual step 4, `sy-infra` invariant tests (winner through `set_workspace_outcome` since Milestone 3), Milestone 3 manual step 5
- AC-8 … `sy-core` status machine tests, `sy-infra` transition tests
- AC-14 … `sy-core` and `sy-infra` tests, `pnpm boundaries:check`
- AC-15 … manual steps 6, 7, 8, socket tests
- AC-16 … manual step 6, `sy-core` id tests
- AC-18 … manual steps 5 and 11, socket tests, `pnpm protocol:check`
- AC-6 … Milestone 2 manual steps 1, 2, 4 to 7, `sy-core` and `sy-infra` recovery tests, socket restart tests
- AC-11 … Milestone 2 manual steps 2, 8 to 11, socket tests, `pnpm protocol:check`
- AC-12 … Milestone 2 manual steps 3, 5, 12 to 14, `sy-core` `read_run_log` tests, socket tests
- AC-9 … Milestone 3 `sy-infra` archive and delete tests (no protocol method archives until feature 8)
- AC-10 … Milestone 3 manual steps 1 to 4, `sy-infra` log cap tests, `sy-daemon` `log_cap` socket test
- AC-2 … Milestone 4 manual steps 1 to 3, `sy-infra` backup tests
- AC-3 … Milestone 4 manual steps 4, 5, 9, 10, `sy-infra` open mapping tests, `sy-daemon` degraded open socket tests
- AC-4 … Milestone 4 manual steps 6 to 11, `sy-infra` set aside test, `sy-daemon` recover socket tests
- AC-13 … Milestone 4 manual steps 12 to 15, `sy-infra` write queue tests, `sy-daemon` disk full and log only socket tests
- AC-17 … Milestone 4 manual step 16, the `sy-daemon` trace logging socket test
