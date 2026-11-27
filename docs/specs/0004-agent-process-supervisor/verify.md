# Verify: agent process supervisor · spec 0004 · updated 2026-11-28
_Steps derived from spec 0004 acceptance criteria and its Value sourcing table. `/check verify` runs these; `/test` locks the durable ones._

All steps use a devtools engine (`cargo build -p sy-daemon --features devtools`) and the fake agent through `dev.agent_run`.

## Engine / manual
- [x] Start a fake run with a `hang` step. While it runs, read `run.pid_started_at` in `switchyard.db`: not NULL, a multiple of 1000, and the run has one `run_process` row named `sy-fake-agent` whose `started_at` matches → AC-1
- [x] A `flood` run with `limits: { output_bytes: 65536 }` ends `failed` / `limit_exceeded`, and `event.since` shows exactly one `run_limit_exceeded { limit: "output", threshold: 65536, observed ≥ 65536 }`, `actor = engine`, before its terminal `run_status_changed` → AC-5, AC-6
- [x] `[print, hang]` with `idle_ms: 1000` is stopped about 1 s later as `idle`; the same with a `status: waiting_input` step first is still live after 3 s → AC-3
- [x] A slow `flood` (2000 B/s) with `active_ms: 1500` is stopped as `time`, `threshold: 1500` → AC-4
- [x] Settings precedence: set global `limits.run_output_bytes = 32`, start a run with no override that prints 40 bytes: it stops as `output` with `threshold: 32`. Set the project's `limits.run_output_bytes = 0`: the next run is not output limited. Pass `limits.output_bytes` on `dev.agent_run`: it wins over both → AC-2 (value sourcing: override → project → global → default)
- [x] An invalid setting (`"x"`, `-5`, `1.5`) is skipped and warned once in `engine.log`, however many runs start → AC-2
- [x] A setting changed while a run is live does not change that run's limits → AC-2
- [x] Put the Mac to sleep for a minute during a run with `idle_ms: 30000`: it is not stopped on wake (monotonic clocks, 2 s cap per tick) → AC-3, AC-4
- [x] A held approval when a limit hits: the hook gets a deny with `run stopped: output limit`, the approval becomes `cancelled` → AC-6
- [x] `[spawn {}, spawn {detach: true}, spawn {detach, ignore_sighup, ignore_sigterm}, expect_input "go", exit 0]`: after about 4 s the three children have `run_process` rows; type `go`; after `run.status` every child is gone within about 3.5 s, and the run has no `run_process` rows → AC-7, AC-8
- [x] `[spawn {detach: true}, exit 0]`: after the run ends, no process has the run's main pid as its session id → AC-7, AC-8
- [x] A run with no children: its main `run_process` row is gone within 1 s of `run.status` → AC-8
- [x] Restart cleanup: a run with a detached child ignoring SIGHUP and SIGTERM, then `kill -9` the engine and start it again: `engine.info` answers at once, the child is gone within about 3.5 s, the run is `lost`, the rows are gone. A row whose pid belongs to another process now (another start time) is deleted and that process is never signalled → AC-9, AC-10
- [x] `[panic_decoder]`: the run ends `failed` / `supervisor_error`, and `engine.log` has an ERROR `panic` line with the run id and a source location → AC-11, AC-14
- [x] Three runs at once (output limit, a held approval plus a `panic` hook, an echo run): the echo run's p95 input to echo stays under 100 ms, `engine.info` keeps answering, the panicked run's approval is `cancelled`, and its fake asks `Allow ...?` in its own terminal (its `sy-signal` fell back) → AC-11, AC-13
- [x] The same panic test in a release profile (`cargo test --release -p sy-daemon --features devtools --test engine -- panic`) passes, proving `panic = "unwind"` → AC-11
- [x] `engine.shutdown` with a fake that ignores SIGHUP and SIGTERM, has a detached stubborn child and hangs: the engine exits in under 15 s, and the agent and the child are gone → AC-12
- [x] The engine's open file soft limit is `min(hard, 4096)` (`launchctl procinfo <pid>` or `lsof -p` while many runs are live) → AC-13
- [x] `engine.log` after a limit stop and a sweep shows the limit, threshold and observed values and the swept process names, and never a prompt, an argument (`--child`), or an env value → AC-15

## Commands
- [x] `cargo test --workspace --features sy-daemon/devtools` → all pass
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean
- [x] `pnpm boundaries:check && pnpm protocol:check` → pass (`sysinfo` only in `sy-infra`)
- [x] `pnpm release:check` → no fake agent, no `dev.*` in the release bundle → AC-14
- [x] `sqlite3 switchyard.db '.schema run_process'` → the table exists, `STRICT`, primary key (`run_id`, `pid`, `started_at`) → data model

## Acceptance criteria coverage
- AC-1 step 1 · AC-2 steps 5, 6, 7 · AC-3 steps 3, 8 · AC-4 steps 4, 8 · AC-5 step 2 · AC-6 steps 2, 9 · AC-7 steps 10, 11 · AC-8 steps 10, 11, 12 · AC-9 step 13 · AC-10 step 13 · AC-11 steps 14, 15, 16 · AC-12 step 17 · AC-13 steps 15, 18 · AC-14 step 14, release check · AC-15 step 19
