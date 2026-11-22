# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Local storage for the engine. Projects, tasks, workspaces, runs and a journal of every change now live in `switchyard.db` (SQLite in WAL mode) inside the data dir, and each run's raw terminal output goes to its own file, `runs/<run_id>.log`. The data dir and its folders are `0700` and every file is `0600`, so only you can read them. (see spec 0002)
- Nothing you saw is lost on a hard kill. Run output is written to its log file before any client receives it, and a run's status change is committed before the `run.status` notification goes out. After a restart, runs that were still live are marked `lost`, and leftover files that belong to no run are cleaned up. (see spec 0002)
- `event.since` method: read the change journal from a cursor, filtered by project, task, workspace or run, 200 events per page by default (500 at most), with `next_cursor` and `has_more`.
- `run.log_read` method: read a run's saved output by byte offset, during the run or after it ends, up to 256 KiB per call.
- `run.status` notification, sent to every connected client after `session.hello`, with the id of the journal event it came from.
- `storage.status` notification, plus an optional `storage` field on `engine.info` that reports `ok`, `degraded_writes`, `newer_schema`, `corrupt` or `unavailable`.
- Safe recovery from a broken database. If the database is corrupt, from a newer version, or can't be opened, the engine still starts and explains why, and every method except the stable ones and `storage.recover` returns `StorageUnavailable`. `storage.recover` then restores the newest backup that passes a check, or starts fresh. It moves the bad files aside under a `.bad-<time>` name and never deletes them.
- Automatic backups in `backups/`: one before each schema migration and one a day, checked before they are kept. The 3 newest are kept.
- The engine keeps working when the disk is full or a write fails. Agents keep running and their output keeps streaming live. Database writes wait in an in memory queue and are retried every 5 seconds, and storage status shows `degraded_writes` until they succeed.
- A size cap on run logs, set by the `storage.log_cap_bytes` setting (5 GiB by default). When the cap is passed, the engine removes logs of ended runs in archived tasks only, oldest first. Logs of active tasks and live runs are never removed.
- New protocol error codes: `RunLogUnavailable` (`-32043`), `StorageUnavailable` (`-32050`), `StorageRecoverNotNeeded` (`-32051`) and `BackupMissing` (`-32052`).

### Changed
- The error code range `-32050` to `-32059` is now the storage block. It was reserved before.
- In `devtools` builds, `dev.synthetic_run` now creates real fixture rows (a project, a task, and a workspace plus run for each synthetic run) and saves their output, and it returns `task_id` and `workspace_ids` too. `dev.stop_runs` marks those runs `stopped`.
