# 0004. Agent process supervisor: engine owned PTYs, run limits, full process tree cleanup and per run fault isolation

**Date**: 2026-11-27
**Status**: In Progress

## Summary

This spec turns feature 4's plain launcher into a supervisor. The engine keeps owning every agent's terminal (no holder process per agent). It now stops a run that goes silent too long, works too long, or prints too much, and ends it `failed` with the reason recorded. When an agent ends, everything it started (dev servers, watchers, background jobs, even ones that left its process group) is stopped too. Those processes are saved to the database, so a restarted engine can finish the cleanup after a crash. A bug in one run's supervision code fails only that run, never the engine or the other agents.

## Requirements

**User stories**:
- As a switchyard user, I want an agent that hangs, loops, or floods its terminal to be stopped on its own, with the reason shown, so I can leave many agents running without watching each one.
- As a switchyard user, I want nothing an agent started to keep running after its run ends, even after an engine crash, so my machine doesn't fill up with stray dev servers and builds.
- As a switchyard user, I want one misbehaving agent, or a bug in how switchyard handles it, to never affect the app or my other agents.
- As a switchyard user, I want to set the limits globally and loosen them for one project, so a big repository can get longer runs.
- As the switchyard developer, I want every limit, cleanup and failure path driven by the fake agent with small limits, so the tests are fast and deterministic.

**Acceptance criteria**:
- **AC-1**: Right after a spawn, the engine reads the agent's start time through the `ProcessTable` port and records `pid`, `pgid` and `pid_started_at` with `set_run_process`. `pid_started_at` is Unix ms, built from the whole seconds the OS reports (`seconds * 1000`). In the same step it inserts the main process as a `run_process` row (`name` = the file name of the resolved program). A main process that has already exited is still an unreaped zombie and still reports a start time, so it is recorded like any other (the sweep removes its row). Only if no start time can be read at all does `pid_started_at` stay NULL with no row, and the run continues as normal. This main row insert alone joins spec 0002's write queue when storage is degraded. Every other `run_process` write is best effort (AC-7).
- **AC-2**: Before `launch`, the engine resolves three limits for the run, each from the first source that has a valid value: the `limits` param of `dev.agent_run` (devtools builds only), then the project's `project_setting`, then the global `setting`, then the default. The keys and defaults are `limits.run_idle_ms` = 1800000 (30 minutes), `limits.run_active_ms` = 14400000 (4 hours) and `limits.run_output_bytes` = 536870912 (512 MiB). A valid value is a JSON integer from 0 to 2^53 (held as `u64`). `0` turns that limit off. Any other value is ignored, and the next source applies. If reading the project's settings fails (the task or project is gone, or a store error), the project source is skipped and the launch never fails because of it. An invalid value or a failed read is warned once per (key, scope, value) for the life of the engine process, not on every launch. The resolved values are fixed for the run's life. A setting changed later applies only to runs started after it.
- **AC-3**: **Idle (hang).** All supervision clocks use a monotonic clock that doesn't advance while the Mac sleeps (`std::time::Instant`), never the wall clock, so sleep or a clock change can never trip a limit. Each tick adds at most 2 s, so a stalled driver can't jump a clock either. The idle clock starts at the spawn and runs only while the run is `starting` or `running`. It goes back to zero on every PTY output byte, counted on the lossless reader path (never the droppable decoder lane), and on every `signal.hook` call that reaches the run's driver (held, answered or rate limited). It also goes back to zero whenever the run leaves `waiting_input`. When it is `>=` the idle limit, the run is limit stopped (AC-6) with `limit = "idle"` and `observed` = the idle ms.
- **AC-4**: **Active time.** The active clock starts at the spawn and adds up the time the run spends in `starting` and `running`. It pauses in `waiting_input`. When it is `>=` the active limit, the run is limit stopped with `limit = "time"` and `observed` = the active ms.
- **AC-5**: **Output.** The output count is every byte the PTY reader read for the run, whether or not its log append succeeded (spec 0002 AC-13). When it is `>=` the output limit, the run is limit stopped with `limit = "output"` and `observed` = the byte count. Output read during the stop sequence still goes through the hub and the log unchanged.
- **AC-6**: **The limit stop.** The driver checks the limits on every 250 ms tick (0003 AC-6), in the order output, time, idle. The first one at or over its limit wins, and a run is checked only while it is live and not already stopping. A limit stop first journals one `run_limit_exceeded` event `{ run_id, limit, threshold, observed }` with `actor = engine` (through 0002's write queue when storage is degraded). It then runs 0003 AC-12's stop sequence: held hooks get the decoder's deny `hold_reply` with the message `"run stopped: <limit> limit"`, those approvals become `cancelled`, then come the adapter's `stop()` input, its grace, SIGHUP to the group, 5 s, and SIGKILL. The stop sequence ends once the exit is observed. From then on, only the sweep (AC-8) signals the group. The run ends `failed` with `end_reason = "limit_exceeded"` (and `exit_code` if it exited with one), even if it exits during the grace. An exit seen by the driver before the limit stop ends the run per 0003 AC-6. A `run.stop` arriving during a limit stop returns `{}` and changes nothing, and a run already stopping for `run.stop` or shutdown is never limit checked. Detection happens within one tick (250 ms) of the limit being reached.
- **AC-7**: **Process tree tracking.** While at least one run is live or sweeping, the engine scans the process table every 2 seconds, one shared scan on a blocking thread for all runs. No scan runs otherwise. Scans never overlap: the `sysinfo` state sits behind one mutex, and a scan that takes longer than 2 s makes the next one skip, never queue. A scan that fails changes nothing (no inserts, no deletes). A scan's results apply to the runs that are live, and not yet closed, when it finishes.

  A run's descendants are every non zombie process owned by the engine's real user that matches either rule. (a) Its session id equals the run's main pid and it started no earlier than the main process. This finds everything that didn't call `setsid`, even after it is reparented to launchd. (b) Its parent chain reaches one of the run's roots, where a root is the main process or a tracked process, matched by (pid, start time), and each child started no earlier than its parent. This finds `setsid` escapees seen before their parent died. A tracked process stays tracked after it is reparented.

  A descendant seen in two scans in a row with the same pid and start time becomes tracked: its `run_process` row is inserted with `INSERT OR IGNORE`, and its `name` is taken from that first sighting. A run tracks at most 256 processes, main process included, counted from the tracker's own in memory set each scan. Past the cap, new descendants are not recorded: they are counted, with one warning per run, and reconsidered when slots free up. A tracked process missing from a scan, or showing a different start time or a zombie state, is checked again directly (`ProcessTable::started_at(pid)`). Only a confirmed mismatch, absence or zombie deletes its row. These writes are best effort: a failed one is skipped, with at most one warning per minute for the whole engine.
- **AC-8**: **The leftover sweep.** When the main process exits, the waiter reports the exit but doesn't reap it. The run ends exactly as 0003 AC-17 says, through step 7 (the launch dir removed), and `run.status` goes out. At the terminal transition the run's tracker is marked `closed`, so the scanner never writes for it again. The run's monitor (see *Runtime shape*) then sweeps, whatever ended the run (exit, `run.stop`, a limit, a supervisor error or shutdown):
  1. One more scan collects every current descendant (AC-7 rules a and b, whatever its age), plus every tracked process.
  2. SIGCONT, then SIGTERM, go to the agent's process group (safe because the unreaped main process keeps the group id reserved) and to each process from step 1 (AC-9). SIGCONT comes first because a stopped process doesn't act on SIGTERM until continued.
  3. If step 1 found nothing besides the main process, the engine skips this wait. Otherwise it waits up to 3 seconds, checking every 100 ms, for them to be gone.
  4. SIGKILL goes to the group and to each survivor, then the engine waits up to 500 ms, checking every 100 ms, for them to be gone. The return value of a group signal is never used to decide anything.
  5. The main process is reaped.
  6. Each row confirmed gone is deleted.

  A row whose process still matches after step 4 (for example `EPERM`, or stuck in the kernel) stays, with a warning, and engine start retries it (AC-10). The sweep never delays `run.status`. Sweeps for different runs run independently. A sweeping run counts as live for the scanner (AC-7) and for shutdown (AC-12), and drops out only when its sweep ends.
- **AC-9**: **Signal safety.** A signal to a single pid is sent only if, checked right before sending, that pid exists, has the recorded start time, belongs to the engine's real uid, is greater than 1, and isn't the engine itself. Otherwise it counts as gone and is never signalled. The gap of under a millisecond between that check and the `kill` call is accepted. A group signal is sent only while the main process is unreaped (0003's invariant still holds, and reaping now happens after the sweep). No signal ever goes to a process found only by name. A `ChildProcess` dropped without `reap()` (for example on a spawn error path) kills its group and reaps the main process in its `Drop`, so the engine never accumulates zombies.
- **AC-10**: **Restart cleanup.** At engine start, once 0002's `recover_at_startup` has run with storage `ok`, the engine reads every `run_process` row into a snapshot before it accepts any connection, so no run started by this engine is in it. A background task then works only from that snapshot and touches only runs that are not live:
  1. For a main process row that still matches (AC-9), SIGCONT and SIGTERM go to its group, which is safe because a live member reserves the group id. No group signal is sent when the main row no longer matches.
  2. SIGCONT and SIGTERM go to every other matching row.
  3. After 3 seconds, SIGKILL goes to the same targets for those still matching.
  4. Each row confirmed gone (a mismatch counts as gone) is deleted by its exact (`run_id`, `pid`, `started_at`).

  The engine accepts connections while this runs, and it logs the counts at info. With storage not `ok` the task is skipped, and the rows wait for the next start.
- **AC-11**: **Fault isolation.** A panic in a run's driver task, PTY reader, PTY writer or waiter thread is caught at that boundary. The release profile builds with `panic = "unwind"` so this works in shipped builds, and the daemon logs an error at start if it was built with `panic = "abort"`. A panic in a thread sends `Control::SupervisorFault` to the driver, which ends the run as below. If the driver itself panicked, its monitor does it. Either way:
  1. The token is revoked and removed from the server's token map.
  2. Held hooks get no reply: their reply channels close, `signal.hook` returns an error, and `sy-signal` uses its fallback.
  3. SIGHUP goes to the group, then 5 s, then SIGKILL, with no adapter stop input, since the decoder can't be trusted.
  4. Pending approvals are listed by run from the store, each becomes `cancelled`, and its `approval.decided` is sent.
  5. The PTY drain and the hub's final flush, whose byte count is the `log_bytes`.
  6. The run ends `failed` with the new `end_reason = "supervisor_error"`, a transition from its current status read from the store.
  7. The launch dir is removed, then the leftover sweep runs (AC-8).

  The steps are idempotent. If the run already reached its terminal transition before the panic, steps 4 to 6 are skipped, but step 7 and the sweep still run. A failure inside these steps is logged, never retried, and the run is left for the next start to mark `lost`. The panic's location is logged at error with `run_id` by a panic hook (reading a task local run id), because a `JoinError` doesn't carry the location. Other runs, the hub, the socket server and the engine keep running.
- **AC-12**: **Shutdown.** Engine shutdown runs 0003 AC-12's sequence for every live run at once (grace capped at 3000 ms), then waits for every run's sweep (all sweeps are held in one `TaskTracker`). Shutdown takes at most about 13 seconds: 3 s grace, 5 s after SIGHUP, up to 1 s PTY drain, the sweep's scan, up to 3 s after SIGTERM, and up to 0.5 s of SIGKILL confirmation. A user or limit stop outside shutdown can take up to 10 s of grace, plus 5 s, plus the sweep.
- **AC-13**: **Isolation under load.** With three fake runs live at once, one limit stopped for output, one ended by an injected decoder panic, and one echoing input, the third run's status, output and input echo keep working throughout, and the engine answers `engine.info` the whole time. At start, the daemon raises its open file soft limit to the smaller of its hard limit and 4096, through a `sy-infra` function (the OS specific code stays there), so many runs and held hooks never run out of descriptors. Agents inherit that soft limit.
- **AC-14**: **Fake agent support.** The fake agent gains these steps. `{ "spawn": { "detach"?: bool, "ignore_sighup"?: bool, "ignore_sigterm"?: bool } }` starts `sy-fake-agent --child` with those flags and continues at once. The child blocks forever, and `detach` puts it in its own process group, so it escapes the agent's group. `{ "ignore_sigterm": true }` makes the main fake ignore SIGTERM. `{ "panic_decoder": true }` calls `sy-signal --hook panic`, and the fake decoder panics in `on_hook`. The `panic` hook name is accepted only by the fake decoder, and only in devtools builds. `dev.agent_run` gains `limits?: { idle_ms?, active_ms?, output_bytes? }`, the highest source in AC-2. All of it exists only in `devtools` builds, as the fake agent does.
- **AC-15**: **Observability.** Info: a limit stop (`run_id`, limit, threshold, observed), each sweep's summary (how many got SIGTERM, how many needed SIGKILL, the process names), and restart cleanup counts. Warn: the tracking cap reached, a failed scan (once per minute), an invalid limit setting, a failed `run_process` write (once per minute), and a process that couldn't be confirmed gone. Error: `supervisor_error` with its panic location, and a release build compiled with `panic = "abort"`. Process command lines, arguments and environments are never read into logs or rows. Only the short executable name is.

## Decision

**Chosen option**: Option 1: supervise in place. The engine keeps owning the PTYs and gains limits, process tree tracking with persisted cleanup, and per run fault isolation.

The engine stays the only owner of agent processes. A per run monitor, one shared process tree scanner on `sysinfo`, and a new `run_process` table give it limits, full cleanup of everything an agent started (also after a crash), and a blast radius of one run.

**Implementation skills**: `rust-best-practices` (`apollographql/skills`, `.claude/skills/rust-best-practices/`) · `tokio-patterns` (`geoffjay/claude-plugins`, `.claude/skills/tokio-patterns/`) · `rust-async-internals` (`po4yka/rust-skills`, `.claude/skills/rust-async-internals/`) · `rust-error-handling` (`po4yka/rust-skills`, `.claude/skills/rust-error-handling/`) · `rust-database` (`po4yka/rust-skills`, `.claude/skills/rust-database/`)

**Smaller calls made with it** (the runner up is in brackets):
- No holder process: the engine owns the PTYs, and feature 23 resumes through the agent's own session [a `sy-holder` per agent].
- Everything an agent started is stopped at run end, group and escapees alike [group only].
- A limit stops the run, which ends `failed` with `limit_exceeded` [warn first, then stop].
- Limits come from defaults, then global `setting`, then `project_setting` [global only].
- Hung means silent (no output, no hook call) for 30 minutes while `starting` or `running` [silent in any live state].
- The time limit is 4 hours of active time, paused in `waiting_input` [wall clock].
- The output limit is 512 MiB of PTY bytes per run [512 MiB plus a sustained rate].
- No memory or CPU limits yet [sample memory and stop over a cap].
- `sysinfo` reads process facts [`libproc`].
- Every long lived descendant is persisted in `run_process`, so restart cleanup is exact [escapees only].
- Rows are deleted once their process is confirmed gone [kept with the run].
- A row holds `name` (the short executable name) but never arguments [identity only].
- The tracking cap is 256 per run [64].
- The scan runs every 2 seconds [1 second].
- A limit stop is recorded as a `run_limit_exceeded` event plus `end_reason = limit_exceeded` [separate end reasons].
- `0` turns a limit off [JSON null].
- Settings apply to new runs only [live runs too].
- Clients see a limit only through the event and the end reason, with no new methods [an effective limits read].
- Leftovers get SIGCONT and SIGTERM, then 3 s, then SIGKILL, after `run.status` [SIGKILL at once].
- A supervision panic fails only that run (`supervisor_error`), and release builds switch to `panic = "unwind"` [keep abort and let the engine crash].
- Restart cleanup runs in the background after runs are marked `lost`, from a snapshot taken before connections are accepted [block startup].
- *(Decided in this spec)* The main process is held as an unreaped zombie until its sweep ends, so its group and session id can't be reused [scan for group members by pgid]. Holding a zombie costs nothing, and it makes a group signal provably safe.
- *(Decided in this spec, after cross check)* Descendants are found by session id first (every run is its own session), and by the parent chain only for `setsid` escapees [the parent chain alone, which breaks as soon as the agent exits and its children are reparented].
- *(Decided in this spec, after cross check)* "Gone" is confirmed by a direct per pid check, never inferred from a scan alone, and a failed scan changes nothing [one missing scan deletes the row].
- *(Decided in this spec, after cross check)* Supervision clocks are monotonic and exclude sleep, with a 2 s cap per tick [the engine `Clock`, whose wall clock would trip limits after a Mac wakes].
- *(Decided in this spec)* The main process is a `run_process` row too, so restart cleanup reads one table [a separate pass over `run.pid`].
- *(Decided in this spec)* After a supervisor panic, the adapter's graceful stop input is skipped, and held hooks get the fallback by their channel closing [ask the decoder anyway]. A decoder that may have panicked can't be trusted to reply.
- *(Decided in this spec)* Stop timings stay as 0003 set them (adapter grace up to 10 s, SIGHUP, 5 s, SIGKILL) [add a SIGTERM step]. The new SIGTERM lives in the sweep, where `nohup`'d jobs need it.
- *(Decided in this spec)* `run_process` writes are bookkeeping: never journaled, and never queued in degraded mode except the main row [route all of them through the write queue]. Restart cleanup is best effort for children by nature, and queueing every scan would grow the 10000 command queue with churn.
- *(Decided in this spec)* The daemon raises its open file soft limit to min(hard, 4096) at start [leave the macOS default of 256].
- *(Decided in this spec)* A panic is tested through a devtools only fake decoder hook [a fault flag in `sy-core`].

## Rationale

Reasoning and options: see [rationale.md](rationale.md).

## Feature design

### Runtime shape (engine)

- **New port in `sy-core`, `ProcessTable`** (blocking, real one in `sy-infra`, a scripted double beside the tests):
  - `scan() -> Result<Vec<ProcInfo>, ProcError>` returns, for every process, `ProcInfo { pid, ppid: Option<i64>, sid: Option<i64>, started_at_ms, uid_is_self: bool, zombie: bool, name }`. `uid_is_self` compares the real uid.
  - `started_at(pid) -> Option<ProcState { started_at_ms, zombie }>` checks a single pid directly.
  - `signal(pid, started_at_ms, sig: Cont | Term | Kill) -> SignalOutcome { Sent, Gone }` re-checks AC-9 right before sending.
  - `engine_pid()`.

  The real implementation keeps one `sysinfo::System` behind a mutex and does a targeted refresh (processes only, with user info, no command lines, no environments). If the pinned `sysinfo` version doesn't report a session id on macOS, `sid` comes from `rustix::process::getsid` per pid. Pin `sysinfo` and check its API against the version you install, since it changes between major versions.
- **`ChildProcess` port changes**: `wait()` now only observes the exit (`waitid` with `WNOWAIT`) and returns `ExitInfo` without reaping. A new `reap()` reaps once and flips `reaped`. `signal_group(sig)` gains `GroupSignal::Cont` and `GroupSignal::Term` and still refuses once reaped. The `sy-infra` `UnixProcess` keeps the same lock discipline, and its `Drop` kills the group and reaps when `reap()` was never called (AC-9), with a `WNOHANG` reap and a short reaper thread as fallback. Nothing ever waits on or kills through the portable-pty `Child` handle it keeps.
- **`sy-core/src/agents/supervise.rs`** (new, the use case's pure parts, unit tested without infra):
  - `Limits` (resolved, `Option<u64>` each, where `None` means off) and `resolve_limits(override, project, global)`.
  - `LimitClocks` (idle and active ms, output bytes) on `Instant`, and `check(now) -> Option<LimitHit>`, in the order output, time, idle, comparing with `>=`.
  - `descendants(scan, main, roots) -> Vec<ProcInfo>` (session rule, then the parent chain rule with start time ordering).
  - `Tracker`, which does the two scans in a row rule, the cap from its own set, gone candidates, the `closed` flag, and yields `Insert` / `Delete` batches. It sits behind a per run mutex shared by the scanner and the monitor.
- **`sy-core/src/agents/scanner.rs`**: one engine wide task on a 2 s `tokio::time::interval` with `MissedTickBehavior::Skip`. It runs `scan()` in `spawn_blocking` only while any run is live or sweeping. For each run that is still open when the scan finishes, it computes the descendants, confirms gone candidates with `started_at`, updates the tracker, and writes the batches through the `Store`.
- **Driver changes** (`driver.rs`): it holds `Limits` and `LimitClocks`. The reader thread bumps an `AtomicU64` byte count and stores the last output `Instant`, shared with the driver, so the decoder lane's drops never fool the idle or output clocks. Hook calls and status changes update the clocks. `on_tick` runs `check`, and a hit journals the event, then calls `begin_stop` with a `StopCause` (`User`, `Shutdown`, `Limit(LimitHit)`). `end` picks `end_reason` from the cause, marks the tracker `closed` at the terminal transition, and returns. It no longer reaps.
- **Monitor** (`mod.rs`): `drive` spawns the driver and a per run monitor task that awaits the driver's `JoinHandle`. The monitor holds `Arc<dyn ChildProcess>`, the token, the run id, the run's tracker, the hub handle and the `RunWrites`. On a panic `JoinError` it runs AC-11's end order. Then, on every end, it runs the sweep. Monitors are spawned on one `TaskTracker` that `stop_all` waits on. The run counts as live (`live_count`, the scanner gate) until its monitor finishes. The reader, writer and waiter thread bodies run inside `std::panic::catch_unwind(AssertUnwindSafe(..))`, and a caught panic sends `Control::SupervisorFault`. A panic hook installed by the daemon logs the location with the run id taken from a task local (or thread local, for the PTY threads).
- **Sweep** (`supervise.rs`, async, run by the monitor): AC-8's six steps, using `ProcessTable::signal` per pid and `ChildProcess::signal_group` for the group, then `reap()`, then deleting the rows.
- **Restart cleanup** (`recovery.rs`, `cleanup_leftovers(snapshot)`): the daemon reads the snapshot after `recover_at_startup` and before it binds the socket for clients, then spawns the cleanup. It uses the same signal and wait helpers, plus a `killpg` through the `ProcessTable` for a main row that still matches.
- **Settings reads**: `AgentRuns::start` reads `project_setting` and `setting` for the three keys once, before `launch`. The warning dedupe set lives in `AgentRuns`.
- **`sy-infra`**: `raise_nofile_limit(4096)` through `rustix` (`cfg(unix)`, called by the daemon at start), the `sysinfo` `ProcessTable`, and `UnixProcess` changes.
- **`sy-daemon`**: calls `raise_nofile_limit`, installs the panic hook, logs an error if `cfg!(panic = "abort")`, takes the cleanup snapshot, and wires the scanner.
- **Root `Cargo.toml`**: `[profile.release] panic = "unwind"`, replacing the `"abort"` that came from the Tauri template. This costs a somewhat bigger binary (unwind tables), which is accepted.
- **New dependency**: `sysinfo` in `sy-infra` only (the crate table and `pnpm boundaries:check` allow lists are updated).

### Data model sketch

Migration 3, `crates/sy-infra/migrations/0003_run_process.sql`, additive per spec 0002 (it takes the usual pre migration backup, and no rebuild is needed):

| Table | Columns and constraints |
|---|---|
| `run_process` | `run_id` TEXT NOT NULL FK → `run(id)` ON DELETE CASCADE · `pid` INTEGER NOT NULL · `started_at` INTEGER NOT NULL (process start, Unix ms from whole seconds) · `name` TEXT NOT NULL (the short executable name from the first sighting, never arguments) · `first_seen_at` INTEGER NOT NULL (engine `Clock`, ms) · PRIMARY KEY (`run_id`, `pid`, `started_at`) · `STRICT` |

Relationship: one run has many tracked processes (1:N), main process included. A permanent delete of a task or project cascades through `run`. Rows exist only while their process might still be alive. The table is normally empty when nothing runs.

**Existing columns now filled**: `run.pid_started_at` (AC-1).

**New Rust values** (no `CHECK` change, since both columns are open text validated by Rust enums):
- `EndReason::SupervisorError = "supervisor_error"` (`LimitExceeded` already exists).
- Journal kind `run_limit_exceeded`, scopes project, task, workspace, run, payload `{ run_id, limit: "idle" | "time" | "output", threshold: u64, observed: u64 }`, `actor = engine`.

**New `Store` methods** (in `sy-core` ports, the SQLite impl in `sy-infra`; the row type is named `TrackedProcess` because `RunProcess` already names `set_run_process`'s input). None of them writes an event.
- `track_processes(run, &[TrackedProcess])` uses `INSERT OR IGNORE`. The main row's call alone is queued when degraded.
- `untrack_processes(run, &[(pid, started_at)])` deletes by the exact key.
- `list_tracked_processes() -> Vec<(RunId, TrackedProcess)>` returns all rows, for the restart snapshot.
- `list_approvals` (existing) by run and `pending` serves the monitor's AC-11 step 4.

**Settings keys** (existing `setting` and `project_setting` tables, JSON integer values): `limits.run_idle_ms`, `limits.run_active_ms`, `limits.run_output_bytes`.

**In memory only**: the clocks, each run's tracker state (seen once and tracked sets, the `closed` flag), the scan results, and the settings warning dedupe set.

### State transitions

The run machine is 0002's, unchanged. What feature 5 adds as a trigger:

| Move | Triggered by |
|---|---|
| `starting \| running \| waiting_input → failed` (`limit_exceeded`) | a limit stop (AC-6), seen before the exit |
| `starting \| running \| waiting_input → failed` (`supervisor_error`) | a caught supervision panic (AC-11) |

Supervisor phases per run (in memory): `live → stopping(cause) → ended (tracker closed) → sweeping → swept`. The sweep starts only after the terminal transition, and the run counts as live until `swept`.

### API surface (engine protocol additions)

| Method | Kind | Inputs | Outputs | Access | Errors |
|---|---|---|---|---|---|
| `dev.agent_run` (devtools, existing) | request | gains `limits?: { idle_ms?: u64, active_ms?: u64, output_bytes?: u64 }` (0 = off) | unchanged | devtools builds only | `InvalidParams` for a non integer |
| `run.status` (existing) | notification | n/a | `end_reason` may now be `limit_exceeded` or `supervisor_error` (it's already a string) | unchanged | n/a |
| `event.since` (existing) | request | n/a | may return `run_limit_exceeded` events | unchanged | n/a |

No new stable methods. `end_reason` is already a `String` in `sy-protocol`, so only its doc comment lists the new values (then `pnpm protocol:gen`).

### Value sourcing

| Action | Value | Source |
|---|---|---|
| Spawn (AC-1) | `pid`, `pgid` | the `PtyChild` (0003) |
| Spawn | `pid_started_at`, the row's `started_at` | `ProcessTable::started_at(pid)` × 1000 (a zombie still answers) |
| Spawn | the main row's `name` | the file name of the resolved program path (0003 AC-2) |
| Spawn, scan | `first_seen_at` | `Clock::now_ms()` at insert |
| Resolve limits (AC-2) | each limit | `dev.agent_run.limits` → `project_setting(project of the run's task, key)` (skipped if the read fails) → `setting(key)` → the constant default |
| Idle clock (AC-3) | last activity | the latest `Instant` among the reader's last output, the last hook call's arrival in the driver, and the last exit from `waiting_input`, starting at the spawn |
| Active clock (AC-4) | active ms | the sum of `Instant` deltas between ticks, each capped at 2 s, while the driver's stored status is `starting` or `running`, starting at the spawn |
| Output (AC-5) | bytes | the reader's `AtomicU64`, added to after each PTY read |
| Limit event (AC-6) | `threshold`, `observed` | the resolved limit and the clock value at the tick that hit it |
| Scan (AC-7) | descendants | `ProcessTable::scan()`: `sid == main pid` with start ≥ the main's, or a `ppid` chain to a (pid, start) root with each start ≥ its parent's |
| Scan | "the same process" | the pair (`pid`, `started_at`) |
| Scan | a tracked row's `name` | `ProcInfo.name` at its first sighting |
| Scan | gone | `ProcessTable::started_at(pid)` re-check: absent, a different start, or a zombie |
| Signal safety (AC-9) | owner, engine pid | `ProcInfo.uid_is_self` (real uid), `ProcessTable::engine_pid()` |
| Restart cleanup (AC-10) | what to stop | the `Store::list_tracked_processes()` snapshot taken before accepting connections, minus rows of live runs |
| Restart cleanup | the group to signal | `run.pgid` of a run whose main row still matches |
| Supervisor error (AC-11) | the end transition's `from` | the run's current status from `Store::get_run` |
| Supervisor error | `log_bytes` | the byte count from the hub's final flush |
| Supervisor error | pending approvals | `Store::list_approvals` (by run, `pending`) |
| Supervisor error | panic location | the daemon's panic hook (`PanicHookInfo::location`), with the run id from a task or thread local |
| Shutdown bound (AC-12) | timings | constants: 3000 ms grace cap (0003), 5000 ms after SIGHUP, 1000 ms drain, 3000 ms after SIGTERM, 500 ms SIGKILL confirmation |
| fd limit (AC-13) | the new soft limit | `min(hard, 4096)` from `getrlimit` |

### Key invariants

- The engine is the only owner of agent processes. No process an agent started outlives the run's sweep, except one the OS refuses to kill, which keeps its row and is retried at start.
- A signal never reaches a process whose (`pid`, start time) doesn't match a record, that isn't owned by our real uid, or that is pid 1 or the engine itself.
- Group signals are sent only while the main process is unreaped, or at restart only while the recorded main process still matches. The main process is reaped only after its sweep.
- A row is deleted only after its process is confirmed gone by a direct check. A failed scan never deletes anything.
- The scanner never writes for a run after its terminal transition (the `closed` flag).
- A run is limit checked only while live and not stopping. Exactly one end wins: the first of exit, stop, limit or fault that the driver or monitor sees.
- The idle and output clocks never depend on the lossy decoder lane, and no clock depends on the wall clock.
- A failure inside one run's supervision ends only that run.
- `run_process` holds only processes that may still be alive, and never arguments or environments.
- Limits for a run never change after it starts.

### Security model

- **One user, as before.** The supervisor signals only processes owned by the engine's real uid and matching a recorded (`pid`, start time). That is a correctness guard against pid reuse, not a security boundary.
- **No secrets in rows or logs.** `sysinfo` can expose command lines. The refresh is configured without command lines or environments, and the engine never puts them into rows, events or logs, only the executable's short name.
- **Fail safe.** When in doubt (a mismatch, a failed scan, a failed re-check), the supervisor doesn't signal the process and doesn't delete its row. It never kills by name.

### Configuration required

- No new environment variables or credentials.
- New global and project settings: `limits.run_idle_ms`, `limits.run_active_ms`, `limits.run_output_bytes` (integers, 0 = off). The UI to edit them comes later. Until then they're set through `Store::set_setting` / `set_project_setting` in tests.
- Root `Cargo.toml` release profile: `panic = "unwind"`.

### Critical test scenarios

Socket tests use the fake agent through the real engine with small `dev.agent_run` limits. Core tests use the scripted `ProcessTable` double and a controllable monotonic clock.

- Output limit thread: a `flood` run with `output_bytes = 65536` ends `failed`, `limit_exceeded`, and `event.since` shows one `run_limit_exceeded { limit: output, threshold: 65536, observed ≥ 65536 }` before its `run_status_changed`. Verifies **AC-2**, **AC-5**, **AC-6**.
- Idle: `hang` with `idle_ms = 1000` is stopped about 1 s later as `idle`. `status: waiting_input`, then `hang`, with the same limit is still live after 3 s. Verifies **AC-3**.
- Active time: a slow `flood` with `active_ms = 1500` is stopped as `time`. A run that spends 2 s in `waiting_input` mid run is stopped only after 1.5 s of active time. Verifies **AC-4**.
- Clocks (unit): a 10 minute jump between two ticks (a simulated sleep or stall) adds at most 2 s to each clock. Verifies **AC-3**, **AC-4**.
- Limit races (driver unit tests with clock doubles): a limit at the same tick as the exit, a `run.stop` during a limit stop (no op), a limit during a user stop (never checked), and a limit hit while an approval is held (the hook gets the deny with `"run stopped: output limit"`, the row is `cancelled`). Verifies **AC-6**.
- Settings resolution (unit): override beats project beats global beats default, `0` turns off, `-5`, `"x"` and `1.5` are skipped, a failed project read falls through to global, the same bad value on ten launches warns once, and a setting changed mid run doesn't change that run. Verifies **AC-2**.
- Leftovers: a fake runs `spawn {}`, `spawn { detach: true }` and `spawn { detach: true, ignore_sighup: true, ignore_sigterm: true }`, waits 5 s, then exits 0. After `run.status`, every child is gone within about 3.5 s (the last by SIGKILL), the main pid is reaped only after them, and the run's `run_process` rows are gone. A fake that exits at once with no children finishes its sweep in under 1 s. Verifies **AC-7**, **AC-8**.
- Orphans after exit: a fake runs `spawn {}`, then exits at once, before any scan. The child, reparented to launchd, is still found by its session id at the sweep and stopped. Verifies **AC-7**, **AC-8**.
- Tracking (unit): two scans in a row are needed, a reparented child stays tracked, a child that started before its supposed parent isn't a descendant, the 257th descendant isn't recorded (one warning) and is recorded once a slot frees, a reused pid with a new start time is treated as gone, a process missing from one scan but confirmed alive by the re-check keeps its row, a failed scan changes nothing, and a closed run gets no writes. Verifies **AC-7**, **AC-9**.
- pid_started_at: after a spawn, `run.pid_started_at` is set and the main process is a row. A main that exits before the read (a zombie) is still recorded. A double that returns no start time leaves it NULL with no row. Verifies **AC-1**.
- Zombie hygiene (unit, `sy-infra`): a `UnixProcess` dropped without `reap()` leaves no zombie. Verifies **AC-9**.
- Restart cleanup: start a fake with a detached `ignore_sigterm` child, `kill -9` the engine, and start a new one. The child is SIGKILLed within about 3.5 s, the run is `lost`, the rows are gone, and `engine.info` answers before the cleanup ends. A row whose pid is now a different process (start time mismatch) is deleted and never signalled. A new run started during the cleanup is never touched. Verifies **AC-9**, **AC-10**.
- Panic isolation: three runs (output limit, `panic_decoder`, `echo_until`). The panicked run ends `failed`, `supervisor_error`, its held approval is `cancelled`, its `sy-signal` used the fallback, and the log shows the panic location with its run id. The echo run's p95 input to echo stays under 100 ms, and `engine.info` answers throughout. Run it in a release profile test too (`cargo test --release -p sy-daemon` for this test) to prove `unwind`. Monitor unit test: a panic after the terminal transition skips the end steps but still sweeps. Verifies **AC-11**, **AC-13**.
- Shutdown: with a fake that has `ignore_sighup`, `ignore_sigterm`, a detached child and `hang`, `engine.shutdown` ends within about 13 s with every process gone. Verifies **AC-12**.
- Fake agent: each new step behaves as AC-14 says, and `pnpm release:check` still finds no fake in a release build. Verifies **AC-14**.
- Logging: a test log capture after a limit stop and a sweep contains the limit and process names, and never an argument, a prompt or an env value. Verifies **AC-15**.

## Build plan

Tracer Bullet: first one thin limit thread through every layer (settings, driver, store, protocol, socket test), then thicken one segment at a time.

1. **The limit thread, output first.** `Limits` and `resolve_limits` (with the failure fallthrough and the warning dedupe), settings reads in `AgentRuns::start`, `dev.agent_run.limits`, the reader's shared byte count, the tick check, the `run_limit_exceeded` journal kind, `StopCause::Limit` through `begin_stop` and `end`, and `end_reason = limit_exceeded`. Then `pnpm protocol:gen` and the output limit socket test. Satisfies **AC-2**, **AC-5**, **AC-6**.
2. **Idle and active clocks.** Monotonic `Instant` clocks with the 2 s tick cap, the last output time, hook and status updates, the pause in `waiting_input`, the clock and limit race unit tests, and the idle and time socket tests. Satisfies **AC-3**, **AC-4**, **AC-6**.
3. **Process facts and migration 3.** Add `sysinfo` to `sy-infra` (crate table, `boundaries:check`, `cargo deny`), the `ProcessTable` port and its implementation (with the `getsid` fallback if needed), `0003_run_process.sql`, the `TrackedProcess` store methods, and `pid_started_at` plus the main row at spawn (queued when degraded). Satisfies **AC-1**, **AC-9**.
4. **Hold the zombie, the monitor and the sweep.** Split `wait()` / `reap()`, add `GroupSignal::Cont` and `Term`, `Drop` for `UnixProcess`, the per run monitor on a `TaskTracker`, live counting until swept, the sweep for every end cause, the fake `spawn` and `ignore_sigterm` steps, and the leftovers and orphan socket tests. Satisfies **AC-8**, **AC-9**, **AC-14**.
5. **The scanner and tracking.** `descendants` (session rule and parent chain rule), `Tracker` (two scans, the cap, re-check before delete, the `closed` flag), the shared 2 s scanner wired in the daemon, and the unit and socket tests. Satisfies **AC-7**.
6. **Restart cleanup.** The snapshot before accepting connections, `cleanup_leftovers` in the background with the group signal for a matching main row, and the `kill -9` socket test. Satisfies **AC-10**.
7. **Fault isolation.** The root release profile set to `unwind` with the startup check, the panic hook with the run id local, `catch_unwind` around the thread bodies, `Control::SupervisorFault`, the monitor's idempotent end order, `EndReason::SupervisorError`, the fake `panic_decoder` step and decoder hook, and the three run isolation test (debug and release). Satisfies **AC-11**, **AC-13**, **AC-14**.
8. **Shutdown, descriptors and logging.** `stop_all` waits on the sweeps' `TaskTracker`, the 13 s bound test, `raise_nofile_limit` in `sy-infra` called at start, and the log lines with the log capture test. Satisfies **AC-12**, **AC-13**, **AC-15**.

## Consequences

**Positive**:
- A hung, looping or flooding agent can't run forever, and the reason (which limit, the threshold, what was observed) is on record.
- Nothing an agent starts outlives its run, including daemonized and `nohup`'d jobs, and a crashed engine's leftovers are cleaned at the next start.
- A bug in one run's supervision costs only that run.
- `pid_started_at` is finally filled, so feature 23 and diagnostics can trust a stored pid.
- Sleeping the Mac never trips a limit.

**Negative / tradeoffs**:
- An engine crash still ends every agent (their terminals hang up). Survival across engine crashes would need a holder process, which this spec declines. Feature 23 resumes through each agent's own session instead.
- A TUI stuck while still redrawing a spinner looks alive, so the idle limit won't catch it. Only the 4 hour active limit will.
- A `setsid` escapee whose parent dies within about 2 seconds, before two scans have seen it, isn't tracked and can survive the run. Processes that stay in the agent's session are always found.
- Killing everything at run end also kills what you might have wanted to keep: a dev server, or a shared helper daemon a build started (`sccache`, a gradle daemon, git's fsmonitor). Those restart on their next use, so this is accepted. Long lived servers belong to feature 13's run scripts, and a name allowlist can come later if this bites.
- The release binary gets bigger (unwind tables), and a panic inside a `Drop` during unwinding still aborts the engine.
- A process scan every 2 seconds costs some CPU while agents run (a targeted `sysinfo` refresh, a few ms on a typical Mac). It's measured in the load check.
- Child `run_process` writes are best effort. If one fails while storage is degraded, restart cleanup may miss that process (the main row is always queued).
- Shutdown can now take about 13 seconds instead of about 8.
- Agents inherit an open file soft limit of 4096 instead of the macOS default.

**Neutral**:
- Migration 3 is additive, with no data transform. A new dependency (`sysinfo`, MIT) lands in `sy-infra`.
- `ChildProcess::wait` changes meaning (it no longer reaps). Every caller is in `sy-core/src/agents/` and `sy-infra`.
- No new stable protocol methods. `end_reason` gains two documented values.

## Migration plan

**Strategy**: no migration needed beyond additive migration 3. `run_process` starts empty, and existing ended runs keep `pid_started_at` NULL, which nothing reads for ended runs.
**Phases**:
1. Migration 3 creates `run_process` at the first start of the new engine (after the usual backup).
2. Limits apply to runs started from then on. Runs already `lost` from an earlier engine have no rows, so restart cleanup does nothing for them.
**Rollback**: revert the commit. An older engine ignores the extra table (spec 0002's forward rule), and `limits.*` settings rows are ignored by code that doesn't read them.
**Risks**: the `panic = "unwind"` switch changes release behaviour everywhere, not only in supervision. The release profile isolation test (AC-11) and the startup check are the guards.

## Follow-up

- [x] Spec 0001: the crate table (`sy-infra` gains `sysinfo`), the shutdown note (about 13 s with the sweep), the release profile `panic = "unwind"`, and its Follow-up "Feature 5: holder process" is settled: no holder. (Done by `/architect` on confirmation of this spec.)
- [x] Spec 0002: AC-6's "features 5 and 23 own that" now reads: feature 5 stops leftovers at start (this spec, AC-10). Add migration 3, `supervisor_error`, `run_limit_exceeded` and the `limits.*` keys to its inventory. Its Follow-up on a holder is answered: no holder, so `lost` stays as is. (Done by `/architect` on confirmation of this spec.)
- [x] Spec 0003: its Follow-up "Feature 5: fill `pid_started_at`, add limits, handle grandchildren, tune stop timings" is covered here. Note that reaping now happens after the sweep, and that the stop sequence ends when the exit is observed. (Done by `/architect` on confirmation of this spec.)
- [ ] At build time (step 3): confirm whether the pinned `sysinfo` reports session ids on macOS. If not, use the `rustix::process::getsid` fallback the spec already allows.
- [ ] Feature 23 (crash safe resume): leftovers are already stopped at start, so 23 designs only resume and restart through `agent_session_ref`.
- [ ] Memory and CPU watching (sample the tree's resident memory, maybe a cap) as its own decision, when a real runaway shows the need.
- [ ] A settings UI for the three `limits.*` keys (global and per project), and showing a `run_limit_exceeded` event's limit and values on the run (features 7, 9, 10).
- [ ] Feature 10: notify on `limit_exceeded` and `supervisor_error` ends.
- [ ] Feature 13 (setup and run scripts): long lived dev servers you want to keep belong there, since an agent run's sweep stops everything it started.
- [ ] `/sync`: add `sysinfo` to the stack and the `ProcessTable` port to `crates/AGENTS.md`, and record the declined skills (`dankosik/rust-cli-skills@rust-processes`, `dotnetage/mindx@sysinfo`) on the `Declined:` line in root `AGENTS.md`.
