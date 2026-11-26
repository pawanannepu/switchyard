# Verify: Agent adapter contract & fake agent · spec 0003 · updated 2026-11-26
_Steps derived from spec 0003 acceptance criteria. `/check verify` runs these; `/test` locks the durable ones. All four milestones: 1 (a fake run through a real PTY), 2 (the signal side channel), 3 (approvals), 4 (detection, login environment and release gates)._

## Milestone 1: a fake run through a real PTY

### UI / manual
- [x] `pnpm dev`, open **Load check**, press **Start 12 runs**, wait about 15 s → 12 terminals stream, the p95 reads under 100 ms and the verdict says Pass (measured 25 ms on 2026-11-26) → AC-14
- [x] In the same screen press **Stop runs** → every terminal stops streaming; `event.since` shows each run ended `stopped` with `user_stop` → AC-12, AC-14
- [x] Type into one load check terminal → the characters echo back (through the fake agent, not the terminal driver, which is in raw mode) → AC-5

### Commands
- [x] `cargo test -p sy-adapter-api` → contract helpers pass: `OutputScanner` strips escapes split across chunks, `hook_command` quotes spaces, quotes, `$` and newlines → AC-1
- [x] `pnpm boundaries:check` → passes; then append `fn _p() { let _ = std::fs::read("x"); }` to `crates/sy-adapters/fake/src/lib.rs` and run it again → it fails naming that line (remove the line after) → AC-2
- [x] `cargo test -p sy-core agents` → the start order, the AC-6 status rule, stop phases (grace, SIGHUP, SIGKILL) and the AC-17 end order pass against port doubles → AC-1, AC-4, AC-6, AC-12, AC-17
- [x] `cargo test -p sy-infra --test agents` → a real PTY child reports its exit code, a death by SIGSEGV reports signal 11 and no code, `pgid = pid`, no group signal after the reap, the child gets exactly the env it was given, launch dirs are `0700` with `0600` files created new → AC-4, AC-6, AC-13
- [x] `cargo test -p sy-daemon --features devtools --test engine` → all pass, including:
  - `a_fake_run_should_print_write_a_file_finish_and_leave_no_launch_dir` → `starting → running → finished`, exit 0, log equals what was printed, the file is in the worktree, `agent-runtime/` is empty → AC-4, AC-5, AC-6, AC-13, AC-14
  - `exit_codes_and_signals_should_classify_the_end` → `exit 3` is `failed` with 3; SIGSEGV and SIGKILL are `failed`, `exited`, NULL code → AC-6
  - `a_malformed_script_should_fail_each_run_with_its_reason_in_the_log` and `a_missing_agent_program_should_fail_the_run_with_spawn_failed` → `failed`, `spawn_failed`, the reason as the log's first line, counted in `log_bytes` → AC-4
  - `unknown_or_detection_only_agents_should_be_refused_before_any_row` → `AgentNotFound`, `AgentLaunchUnsupported` (Claude), `InvalidParams` past 32, and no task or run rows → AC-4, AC-15
  - `run_stop_should_ctrl_c_the_fake_and_refuse_ended_or_unknown_runs` → `stopped`, `user_stop`, exit 130; then `RunNotLive` and `RunNotFound` → AC-12
  - `a_run_ignoring_ctrl_c_and_sighup_should_get_sigkill_after_about_10_s` → ends 9.5 to 13 s after `run.stop`, NULL code → AC-12
  - `shutdown_should_stop_every_run_within_about_8_s` → the engine exits in under 10.5 s and both runs are `stopped` → AC-12, AC-17
  - `start_should_sweep_launch_dirs_left_by_a_crash` → a leftover `agent-runtime/<run>` is gone after a restart → AC-13
  - the ported 0002 tests (`agent_runs_should_be_saved…`, `kill_9…`, `disk_full…`, `slow_client…`) → persist before broadcast still holds for agent runs → AC-5, AC-14
- [x] `cargo test -p sy-daemon` (no devtools) → `release_engine_should_not_answer_dev_methods` passes: `dev.agent_run` is `MethodNotFound` → AC-14
- [x] `pnpm protocol:check` → schema and TS types match (`run.stop`, `dev.agent_run`, the new codes, protocol 1.1) → AC-4, AC-12

### Value sourcing
- [x] Launch cwd: start a fake run and read `workspace.worktree_path` → the `write_file` step's file lands there, never in the launch dir → cwd row
- [x] Launch options: `dev.agent_run` with `options.script` → `workspace.agent_options` holds that JSON and `<data dir>/agent-runtime/<run>/script.json` holds the script while the run is live → options and launch dir rows
- [x] Environment: a fake run of `/usr/bin/env` is not possible through the fake, so check `the_child_should_get_exactly_the_environment_it_was_given` (sy-infra) and `agent_env_should_be_the_login_env_plus_only_the_listed_variables` (sy-core) → the env is the login env plus only `TERM`, `COLORTERM`, `SWITCHYARD_SIGNAL_SOCKET`, `SWITCHYARD_SIGNAL_TOKEN` → environment row
- [x] Program path: point `SWITCHYARD_HELPER_DIR` at an empty dir → the fake run fails `spawn_failed`; the absolute path is never resolved elsewhere → program path and fake agent path rows
- [x] Signal token: two runs get different 32 character lowercase hex `SWITCHYARD_SIGNAL_TOKEN` values → signal token row
- [x] `pid`, `pgid`: a live run's row has `pid = pgid`, `pid_started_at` NULL → run row
- [x] `end_reason`: `exited` (exit), `user_stop` (`run.stop` or shutdown), `spawn_failed` (bad script) → end reason row
- [x] PTY size: `dev.agent_run` with `cols: 90, rows: 30` → the spawn gets 90 by 30 (default 120 by 40) → PTY size row

## Acceptance-criteria coverage (milestone 1)
- AC-1: contract crate tests, sy-core agents tests, the fake and Claude adapters land with no sy-core change outside `agents/` and `adapters.rs`
- AC-2: boundaries check plus its probe, the program allowlist tests, the env tests. The probe refusal half lands in milestone 4
- AC-4: the daemon spawn failure and refusal tests, the sy-core start order tests
- AC-5: echo and load check, ported persist before broadcast tests
- AC-6: status rule unit tests, exit classification socket test
- AC-12: stop unit tests, `run.stop`, SIGKILL and shutdown socket tests (held approvals join in milestone 3)
- AC-13: launch dir removal after each run and the startup sweep
- AC-14: the fake agent through the real path, the ported load check (p95 25 ms), the release engine refusing `dev.agent_run`. The release bundle check lands in milestone 4
- AC-17: the end order in the driver (steps 3 and 4 land with approvals in milestone 3)

## Milestone 2: the signal side channel

### Commands
- [x] `cargo test -p sy-signal` → the helper says hello as `signal`, relays token, hook, args and base64 stdin, and answers with the reply's stdout and exit code; any error reply, an unreachable socket, an incompatible hello, and stdin plus args over 256 KiB all write the fallback and exit with its code (the last one without calling); bad fallback arguments exit 2 → AC-7
- [x] `cargo test -p sy-core agents` → hook calls set the status, journal events as `agent` with the workspace id, the last session ref wins, and none of them changes the status by itself; an unknown hook gets the decoder's fallback with no reports; a token is unknown before its run and after its end; past the burst of 500 a call is `RateLimited` and its report dropped, while a held hook is never charged → AC-6, AC-7, AC-8, AC-17
- [x] `cargo test -p sy-core limit` → the bucket passes 500, refills 100 a second up to 500, and never refills on a clock stepping back → AC-7
- [x] `cargo test -p sy-adapter-fake` → `status`, `session` and `event` steps parse (bad ones are `InvalidOptions`), each hook maps to its report, a bad payload or unknown hook gets the fallback; `waiting_pattern` split across chunks with escapes reports `waiting_input`, `quiet_after_ms` reports it only after the period, new output reports `running` → AC-6, AC-8
- [x] `cargo test -p sy-daemon --features devtools --test engine` → all pass, including:
  - `hook_steps_should_set_status_journal_agent_events_and_save_the_last_session` → `starting → running → waiting_input → running → finished`; `message`, `command_run` and `file_changed` journaled with `actor = agent` and `actor_ref` = the workspace id; `agent_session_ref` is `s-2` → AC-6, AC-8
  - `a_waiting_pattern_on_screen_should_mean_waiting_input_until_new_output` → `[y/n]` with ANSI escapes in the line makes the run `waiting_input`; typing `y` and the output after it make it `running` → AC-6
  - `a_hook_payload_over_256_kib_should_fall_back_and_journal_nothing` → the 300 KiB `event` is not journaled, the next one is, and the run finishes → AC-7
  - `a_signal_connection_should_only_relay_hooks_and_get_no_notifications` → `signal.hook` before hello is `HelloRequired`; on a signal connection `engine.info` works, `run.attach` is `NotAllowedForClient`, a bad token is `SignalTokenUnknown`, 256 KiB plus 1 is `SignalPayloadTooLarge`; another client's `signal.hook` is `NotAllowedForClient`; the signal connection gets no `run.status` → AC-7
  - `hook_calls_past_the_burst_should_be_rate_limited_with_one_warning` → of 700 calls, 500 (plus the refill) are answered, the rest `SignalRateLimited`, and `engine.log` has exactly one rate limit warning → AC-7
- [x] `pnpm protocol:check` → `signal.hook`, `SignalHookParams`/`SignalHookResult`, `ClientKind` `signal`, and `NotAllowedForClient`, `SignalTokenUnknown`, `SignalPayloadTooLarge`, `SignalRateLimited` match → AC-7

### UI / manual
- [x] `pnpm dev`, then `dev.agent_run` (through the dev bridge or a test client) with the hook steps script above → the run's status goes to `waiting_input` and back, and `event.since { run_id }` lists the agent's events → AC-6, AC-8
- [x] While a fake run is live, `ps -o command= -p <pid>` shows its `--token`; after the run ends, a `signal.hook` with that token is `SignalTokenUnknown` → AC-7, AC-17

### Value sourcing
- [x] Agent event `actor`, `actor_ref`: every event a fake reports is `agent` with the run's `workspace_id`, never `user` or `engine`; with two runs, each event carries its own run's workspace → agent event row
- [ ] Quiet timer now and period: in the decoder test, vary `quiet_after_ms` (500 versus 2000) against the same injected clock → `waiting_input` comes exactly at the period, not before; with no `quiet_after_ms` nothing is reported → quiet timer rows
- [x] Limits: 256 KiB payload (stdin plus args, so `MAX - 2` bytes plus `"abc"` is over, plus `"ab"` is not), 100 a second with a burst of 500 → limits row

## Acceptance-criteria coverage (milestone 2)
- AC-6: hook status reports and the pattern and quiet fallback, in decoder, driver and socket tests
- AC-7: the helper's own tests, the signal connection rules, the size cap, the rate limit charged after the decoder answers. Still owed: a second `signal.hook` while one is open (`InvalidRequest`, built in dispatch) and a held call that is never charged over the socket, both tested with approvals in milestone 3, where a hold keeps a call open
- AC-8: events journaled as the agent's, the last session ref wins, neither changes the status
- AC-17: the token is unknown after the run's end (revoked at step 2)

## Milestone 3: approvals

### Commands
- [x] `cargo test -p sy-infra --test store approval` and `cargo test -p sy-infra --test store terminal_and_engine` → migration 2's `approval` table is `STRICT` with its indexes; a create writes a `pending` row (summary cut to 200 characters) plus `approval_requested` under every ancestor; a decision lands once and the next is `ApprovalNotPending`; `response` is `{ message? }` for your decisions and NULL for `expired`; cancel ends only that run's pending ones, expire ends all; `list_approvals` filters by status and run and keyset pages; deleting a task takes its approvals → AC-9, AC-10, AC-11
- [x] `cargo test -p sy-infra --test storage approvals_while_degraded` → while degraded a create is refused (`Full`), a decision and the run end's cancel are queued and read back at once (`event_id` none), then land in order after recovery → AC-9, AC-17
- [x] `cargo test -p sy-core agents` → held hooks wait for your answer and get the decoder's reply; two pending keep the run `waiting_input` until both are answered, and a `running` report changes nothing meanwhile; a typed request gets `n\r` typed; terminal answers settle it (`Unknown` is `cancelled`, the hook gets a deny); a caller going away makes it `expired`; a duplicate key or a failed create makes the helper fall back; a stop cancels held ones with "run stopped" before typing Ctrl C; an exit denies held hooks and cancels before the terminal status; your answer racing a stop lands once and matches the held reply (20 rounds); `raw` over 64 KiB is cut and flagged → AC-6, AC-9, AC-10, AC-12, AC-17
- [x] `cargo test -p sy-adapter-fake` → the `approval` step validates its detail and branches; the hook holds with keys `a1`, `a2`; replies are `{"decision":"approve"}` or `{"decision":"deny","message"}`; an unreadable payload gets the `{"decision":"ask"}` fallback → AC-9
- [x] `cargo test -p sy-daemon --features devtools --test engine` → all pass, including:
  - `an_approval_should_hold_the_fake_until_answered_then_run_that_branch` → `approval.requested` with kind, detail, workspace and an `event_id`; the run is `waiting_input`; approve runs `on_approve` (exit 0), deny with a message runs `on_deny` (exit 4); `approval.decided` carries the message; answering again is `ApprovalNotPending` → AC-9, AC-10
  - `a_held_approval_whose_helper_dies_should_expire_and_the_fake_should_ask_itself` → killing the `sy-signal` child makes it `expired`; the fake prints `Allow deploy? [y/n] `, a typed `y` approves → AC-7, AC-10
  - `an_approval_the_engine_cannot_take_should_fall_back_to_the_agents_own_prompt` → a 300 KiB request falls back to the fake's own prompt, never approved, no row → AC-7, AC-9
  - `stopping_a_run_should_deny_and_cancel_its_held_approval_before_it_ends` → `approval.decided` `cancelled` with "run stopped" before `run.status` `stopped` (`user_stop`) → AC-12, AC-17
  - `a_run_that_dies_holding_an_approval_should_cancel_it_before_its_end_and_revoke_the_token` → `cancelled` before `failed`; the token is then `SignalTokenUnknown`; no launch dir left → AC-17
  - `a_pending_approval_should_be_expired_after_an_engine_restart` → `expired` after `kill -9` and a restart; answering it is `ApprovalNotPending` → AC-10
  - `approval_list_should_page_pending_filter_by_run_and_check_its_params` → pages of 2 with `next` and `has_more`, run filter, an unknown status, a malformed `after` and a 4097 byte message are `InvalidParams`, an unknown id is `ApprovalNotFound` → AC-10, AC-11
  - `a_held_signal_call_should_answer_over_the_wire_and_refuse_a_second_call_meanwhile` → a second `signal.hook` while one is held is `InvalidRequest`; the held one answers with the deny JSON (milestone 2's owed item) → AC-7
  - `hook_calls_past_the_burst_should_be_rate_limited_with_one_warning` → a hold right after the burst still holds (milestone 2's owed item) → AC-7

### UI / manual
- [x] `pnpm dev`, start a fake run with an `approval` step through the dev bridge, then `approval.list` → it is pending; `approval.respond` approve → the fake runs `on_approve` in its terminal → AC-9, AC-10, AC-11

### Value sourcing
- [x] `kind`, `summary`, `detail`, `raw`: vary the fake step's kind (command, file_edit, fetch, mcp_tool, other) → each row's `kind` and typed detail fields follow the variant, `raw` is the hook's payload → approval row
- [x] `requested_at`, `decided_at`: both from the engine clock, `decided_at` absent until it leaves `pending` → approval time rows
- [x] `decided_by`, `decided_by_ref`: `user` + `inbox` (`approval.respond`), `user` + `terminal` (a settled report, core test), both absent for `expired` and `cancelled` → decided by row
- [x] `response.message`: your message, or "run stopped" on a stop → message row
- [x] Approval wire `workspace_id`: two runs in different workspaces each show their own → workspace id row

## Milestone 4: detection, login environment and release gates

### Commands
- [x] `cargo test -p sy-core agents` → a pass probes only listed programs found on the login `PATH` (a missing one is `NotFound` without a spawn, an unlisted one `Refused`), with cwd `agent-runtime/`, `NO_COLOR=1`, `TERM=dumb` and the timeout capped at 5 s; concurrent `detect` calls share one pass → AC-2, AC-3
- [x] `cargo test -p sy-adapter-claude` → a version answer is installed with its first word; no answer is installed with the hint; missing or refused is not installed; launch is unsupported → AC-15
- [x] `cargo test -p sy-infra --test agents probes_and_login` → probes report exit and capped output with exactly their env; a timed out probe is killed with its children in under 2 s; a noisy login shell gives only the marked env; a hanging or silent one falls back to `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin` and says why; `snapshot` waits for the first capture → AC-3, AC-16
- [x] `cargo test -p sy-daemon --features devtools --test engine` → all pass, including:
  - `detection_should_ask_claude_for_its_version_on_the_login_path` → `agent.list` order `claude`, `fake`; a stub `claude` printing `2.1.325 (Claude Code)` is installed, version `2.1.325`, `signed_in: unknown`, its path; the fake has no path; `agent_id` filters; an unknown one is `AgentNotFound` → AC-3, AC-15
  - `a_missing_claude_should_be_not_installed_with_no_path` → AC-3, AC-15
  - `a_hanging_claude_should_be_killed_at_5_s_and_concurrent_detects_share_one_pass` → three `agent.detect` calls run the stub once and answer after about 5 s with the hint → AC-3, AC-15
  - `env_refresh_should_report_the_capture_and_rerun_detection` → `{ login_env: { status: "ok" } }`, then `checked_at` moves → AC-3, AC-16
  - `the_login_shell_should_be_asked_and_a_hanging_one_should_fall_back_after_5_s` (also without devtools) → `engine.info.login_env` is `ok` for a noisy shell and `fallback` with its message for a hanging one, in under 8 s → AC-16
  - `engine_log_should_never_hold_payloads_tokens_env_values_or_prompts` → at trace level, no prompt, env value, hook payload, session ref, approval summary, detail, message, probe output or token in `engine.log`, while hook debug lines and the run span are there → *Observability*
- [x] `pnpm app:build && pnpm release:check` → `sy-signal` is in `Contents/MacOS`, universal and signed with the engine's identity; no `sy-devbridge` or `sy-fake-agent` in the bundle; the bundled engine answers no `dev.*` method and lists no `fake` agent (passed 2026-11-26) → AC-2, AC-14
- [x] `pnpm --filter @switchyard/desktop test:e2e` → passes with the engine on `SWITCHYARD_LOGIN_ENV=inherit` → AC-16

### UI / manual
- [x] Run a release engine (`cargo run -p sy-daemon`) and call `engine.info` then `agent.detect` → `login_env.status` is `ok` and `claude` is found where only your login shell knows it (seen 2026-11-26: `~/.local/bin/claude`, 2.1.325) → AC-3, AC-15, AC-16

### Value sourcing
- [x] Detection `installed`, `signed_in`, `version`, `hint`: change the stub `claude` (version, exit 1, hang) → each field follows the probe as `interpret_detection` reads it → detection row
- [x] Detection `path`: put the stub in a different `PATH` dir → `path` follows the first probe's resolution → path row
- [x] Detection `checked_at`: rises with each pass, the same for every adapter in one pass → checked at row
- [x] Login env status and message: a working shell gives `ok` with no message; a hanging one `fallback` with why → login env row
- [x] Limits: probes capped at 5 s and 64 KiB per stream → limits row

## Acceptance-criteria coverage (milestones 3 and 4)
- AC-2: the probe allowlist (core test), the release bundle and agent list checks. Complete with milestone 1's launch half
- AC-3: detection passes, the shared pass, timeouts, no spawn for a missing program, `path` and `checked_at`
- AC-6: the `waiting_input` rule while approvals are pending, and no spurious `running` once the end starts
- AC-7: milestone 2's owed items now covered (a second call while one is held, a held call never charged)
- AC-9 to AC-11: approvals in the store, the driver and over the socket
- AC-12, AC-17: the stop and run end orders with held and typed approvals
- AC-14: the release check on a real universal bundle
- AC-15: the Claude adapter's detection, unit and socket tests
- AC-16: the login shell capture, its fallback, `env.refresh`, `engine.info.login_env`, and the `inherit` seam for tests
