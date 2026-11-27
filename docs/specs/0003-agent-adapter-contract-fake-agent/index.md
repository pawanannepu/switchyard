# 0003. Agent adapter contract: pure adapters, the real TUI in a PTY, and a hook side channel, plus a scripted fake agent

**Date**: 2026-11-22
**Status**: Accepted

## Summary

This spec defines the one interface every coding agent plugs into, and a fake agent that tests drive. Each agent's real interactive terminal app runs in a PTY (a pseudo terminal), so you see and type into exactly what you would in your own terminal. Alongside it, the agent's own hooks (small commands the CLI runs on events) report status, approval requests, events and the session id to the engine through a tiny helper program, `sy-signal`. Adapters are pure translators: they never spawn anything, open files or touch a network, which keeps them easy to test and makes "never read login tokens" something a script can check. Feature 4 also builds the plain launcher, the approval relay with its table, detection, and the fake agent that runs through the exact same path.

## Requirements

**User stories**:
- As a switchyard user, I want each agent to run in its own real terminal that I can watch and type into, so switchyard never hides or changes what the agent shows me.
- As a switchyard user, I want to see when an agent is running, waiting for me, finished or failed, and to answer its permission requests from switchyard, so I can run many at once without watching every terminal.
- As a switchyard user, I want switchyard to tell me which agents are installed and signed in without ever reading my login tokens, so my credentials stay with the tools that own them.
- As the switchyard developer, I want adding an agent to mean one new crate and one registration line, and I want a scripted fake agent that makes orchestration tests fast and deterministic.

**Acceptance criteria**:
- **AC-1**: `sy-adapter-api` exposes `CONTRACT_VERSION = 1`, the `AgentAdapter` trait (`id`, `display_name`, `capabilities`, `programs`, `detection_probes`, `interpret_detection`, `launch`) and the per run `RunDecoder` trait (`on_output`, `on_hook`, `on_tick`, `hold_reply`, `input_reply`, `stop`, `on_exit`), with the types under *Contract*. Every method is synchronous and does no I/O. The crate depends only on `serde`, `serde_json`, `thiserror` and `vte`. `AgentAdapter::id` returns `&str` (not `&'static str`) and the registry takes `Arc<dyn AgentAdapter>` instances, so one generic adapter type can register many configs (feature 18). The fake adapter and the Claude adapter land with no change to any `sy-core` file outside `sy-core/src/agents/` (the use case that drives the contract) and `adapters.rs`.
- **AC-2**: The token rule holds by construction and by check. `pnpm boundaries:check` fails when any library source under `crates/sy-adapters/*/src` names `std::fs`, `std::process`, `std::net`, `std::env`, `std::os::unix::net`, `tokio::fs`, `tokio::process`, `tokio::net` or `dirs`. The engine refuses a probe or launch whose `program` is not in that adapter's `programs()` list (a refused probe reads as `installed: false` with a logged warning, and a refused launch fails the run with `spawn_failed`). A bare name in `programs()` matches a plan `program` of the same bare name, which is then resolved on the login `PATH`. An absolute path matches only that exact absolute path, compared literally and never resolved. The engine passes the login environment to agents and probes unchanged, except for the adapter's `env_set` and `env_remove` and the engine's own `TERM`, `COLORTERM` and `SWITCHYARD_SIGNAL_*` variables. It never adds a credential variable and never reads an agent's config or auth files.
- **AC-3**: Detection runs once in the background at engine start (after the login environment is captured), on `agent.detect`, and on `env.refresh`. Each probe runs with an argument array, no TTY, stdin null, cwd `<data dir>/agent-runtime/`, the login environment plus `NO_COLOR=1` and `TERM=dumb`, a 5 second timeout (the adapter may ask for less), and stdout and stderr capped at 64 KiB each. A program that doesn't resolve on the login `PATH` gives `installed: false` without spawning. The adapter turns the probe results into `{ installed, signed_in: yes | no | unknown, version?, hint? }`, and the engine adds `path?` (the first probe's resolved path, absent when the adapter has no probes or it didn't resolve) and `checked_at`. `agent.list` returns every registered adapter in registration order with its capabilities and its last detection (absent until the first pass finishes). There is at most one detection pass in flight: an `agent.detect` call, the pass `env.refresh` starts, and the first pass at start all join a running pass instead of starting another, and `agent.detect` returns when that pass ends.
- **AC-4**: `dev.agent_run { agent_id, prompt?, options?, count?, cols?, rows? }` (devtools builds only) creates fixture rows as 0002 AC-15 did, with the given `agent_id` and `agent_options`, and creates each workspace's `worktree_path` as a plain `0700` directory. For each run the order is fixed: `create_run` (`queued`), the hub entry, `transition_run` to `starting`, mint and register the signal token, create `<data dir>/agent-runtime/<run_id>/` (`0700`), then call `launch`. The engine then writes the plan's files (`0600`, each created new with no symlink followed, and a `rel_path` that is absolute or contains `..` is refused), resolves `program` per AC-2, and spawns it in a new PTY session (default 120 by 40) with the plan's argv, the worktree as cwd, and the environment from AC-2. It records `pid` and `pgid` with `set_run_process`. If the first login environment capture (AC-16) hasn't finished, the launch waits for it. An `AdapterError` from `launch` (including `InvalidOptions` for a bad fake script), a refused program or file, or a spawn error makes that run `failed` with `end_reason = "spawn_failed"`. The reason is written through the hub as one line of text at offset 0, so it is in the log and counted in `log_bytes`. With `count` > 1, each run starts and fails on its own, and rows already created are never rolled back. An unknown `agent_id` returns `AgentNotFound`, and an adapter whose `capabilities.launch` is false returns `AgentLaunchUnsupported`, both before any row is created.
- **AC-5**: PTY output goes through the run hub unchanged, so 0002's persist before broadcast holds for agent runs. `run.input` writes to the PTY, and `run.resize` resizes it. The PTY reader reads at most 16 KiB at a time. The decoder gets a copy of each chunk on its own droppable output lane, bounded at 256 chunks (4 MiB at most). When that lane is full, the chunk is dropped for the decoder only (counted, with a warning once per run per minute), and the hub, the log and the producer never wait on the decoder. Everything else the run's task receives (hook calls, decisions, stop, exit, ticks) arrives on a separate lane that never drops.
- **AC-6**: A run moves `starting → running` when its decoder reports `running`, or on the first output byte if the decoder has reported nothing. After that, the live status is always computed the same way: `waiting_input` if the run has any pending approval, or if the decoder's last status report was `waiting_input`, otherwise `running`. The engine computes it again after every decoder report, new approval and decision, and applies it through `transition_run` when it differs from the stored status. A `running` report while an approval is pending is therefore ignored for status. A report the state machine refuses is dropped with a debug log, and one that names the current status is a no op (0002 AC-8). The engine calls `on_tick(now_ms)` every 250 ms while the run is live (missed ticks are skipped, not queued), so an adapter can use a quiet period timer. When the process exits, `ExitInfo.code` is its exit code and `ExitInfo.signal` is the number of the signal that ended it (from portable-pty's exit status), and exactly one of them is set. The engine then reads the PTY until EOF or for at most 1 second, closes the PTY master and lets the reader thread go, calls `on_exit`, and ends the run `finished` or `failed` as the decoder says, with `end_reason = "exited"` and `exit_code` (NULL for a death by signal). The full end order is AC-17.
- **AC-7**: `sy-signal` reads its stdin (capped at 256 KiB) and its trailing arguments, opens the engine socket named in its argv, sends `session.hello { client: "signal" }`, then sends `signal.hook { token, hook, args, stdin_b64 }`. It writes the reply's `stdout_b64` bytes to stdout and exits with its `exit_code`. If the socket can't be reached, the hello fails, the call returns any error, or the input is over the cap, it writes the fallback stdout and exit code given in its own argv instead, and exits. On a `signal` connection only `session.hello`, the stable methods and `signal.hook` work (anything else returns `NotAllowedForClient`), and the connection gets no notifications. A signal connection carries one `signal.hook` at a time, and a second one sent while the first is open gets `InvalidRequest`. While a call is open, the engine keeps reading the connection, so it notices at once when the helper goes away. `signal.hook` on any other kind of connection returns `NotAllowedForClient`. An unknown or revoked token gives `SignalTokenUnknown`, and a payload over 256 KiB gives `SignalPayloadTooLarge`. The rate limit is charged after the decoder answers: every call reaches `on_hook`, and a call it holds for an approval is never charged. Each run gets 100 calls answered at once per second, with a burst of 500. When that is used up, the call's reports are dropped and it gets `SignalRateLimited` instead of the decoder's reply, so the helper uses its fallback. Each such call is counted, with one warning per run per minute. Tokens are 128 bit random values held only in memory, and the run's task revokes its token at the start of the run's end (AC-17).
- **AC-8**: A decoder's `Event` report is journaled with `Store::append_event` as `message`, `command_run` or `file_changed`, with `actor = "agent"` and `actor_ref` set to the run's workspace id. A `SessionRef` report calls `set_agent_session_ref` (the last one wins). Neither ever changes the run's status.
- **AC-9**: When `on_hook` returns `Hold(request)`, or a decoder reports `ApprovalRequested(request)`, the engine creates an `approval` row (`pending`, the request's kind, summary cut to 200 characters, and its detail with `raw` cut to fit 64 KiB plus `raw_truncated: true` when cut) together with an `approval_requested` event in one transaction. It then sends `approval.requested { approval, event_id }`, and the run becomes `waiting_input` per AC-6. A run may have several pending approvals at once. Each request's `key` must be unique among that run's pending approvals. A request whose key is already pending is dropped with a warning, and a held hook with such a key gets the decoder's fallback reply. A held hook's `signal.hook` call stays open until the approval leaves `pending`. If the row can't be created (storage degraded, `StoreError::Full`), no approval exists, a held hook gets the decoder's fallback reply (the agent's own prompt), and a reported one is dropped with a warning.
- **AC-10**: The run's task is the only place an approval changes. `approval.respond { approval_id, decision: "approve" | "deny", message? }` (message up to 4 KiB) hands the decision to that task and waits for its answer. The task calls `decide_approval`, which changes the row only while it is still `pending`, so the first change wins any race with a stop, the run ending, a terminal answer or a closed hook. On success the row is `approved` or `denied` with `decided_at`, `decided_by = "user"`, `decided_by_ref = "inbox"`, `response = { message? }` and one `approval_decided` event. The task then delivers the decoder's `hold_reply` to the waiting hook, or queues its `input_reply` bytes for the PTY (waiting for a free input slot, never failing as busy, and written as one write). It sends `approval.decided { approval, event_id }` and computes the live status again (AC-6). `approval.respond` returns the decided approval even if the agent can no longer receive the reply (the hook closed or the process exited a moment earlier). A request the decoder reports as settled in the terminal becomes `approved` or `denied` (or `cancelled` for `SettledOutcome::Unknown`), with `decided_by = "user"` and `decided_by_ref = "terminal"` (both NULL for `cancelled`). A held hook settled this way gets the decoder's `hold_reply` for the outcome it saw (a deny for `Unknown`), so the agent never shows a prompt you already answered. A settled report whose key matches no pending approval is ignored. If a held hook's connection closes first (the agent's own hook timeout), the row becomes `expired`. When a run ends, its pending approvals become `cancelled` (AC-17). At engine start, every pending approval becomes `expired`. `expired` and `cancelled` rows have `decided_by` and `decided_by_ref` NULL. Every one of these changes writes one `approval_decided` event and sends `approval.decided`. Answering a row that isn't pending returns `ApprovalNotPending`, and an unknown id returns `ApprovalNotFound`.
- **AC-11**: `approval.list { status?, run_id?, after?, limit? }` returns approvals filtered by `status` (default `pending`) and `run_id`, ordered by (`requested_at`, `id`), keyset paged after the given `after: { requested_at, id }`, with `limit` defaulting to 100 (also when 0) and clamped to 500. `after` combines with the filters (AND). The response carries `next` (the `{ requested_at, id }` of the last row, absent on an empty page) and `has_more`. An unknown `status` value or a malformed `after` gives `InvalidParams`.
- **AC-12**: `run.stop { run_id }` returns `{}` once the stop has started. For a live run the engine first answers every held hook with the decoder's `hold_reply` for a deny with the message `"run stopped"` and marks those approvals `cancelled`. It then queues the decoder's `stop()` input for the PTY (waiting for a free input slot), waits the plan's grace (clamped to 0 to 10000 ms), sends SIGHUP to the process group, waits 5 seconds, and sends SIGKILL. Group signals are sent only until the main process has been reaped, and never after, so a reused group id is never hit (feature 5 owns leftover grandchildren). Whichever comes first decides the end: a stop request seen by the run's task before it saw the exit ends the run `stopped` with `end_reason = "user_stop"` (and `exit_code` if it exited with one), even if the process exits during the grace. An exit seen first ends it per AC-6, and a `run.stop` arriving after that is a no op. A `queued` run goes straight to `stopped`. Stopping a run that is already stopping is a no op, an ended run gives `RunNotLive`, and an unknown run gives `RunNotFound`. Engine shutdown runs this sequence for every live run at once, with the grace capped at 3000 ms, so a shutdown takes at most about 8 seconds. Open signal connections stay writable until their held replies are sent, even after the engine stops accepting new connections.
- **AC-13**: A run's launch dir exists from just before `launch` until just after its terminal transition, then it is removed. At engine start, every directory under `<data dir>/agent-runtime/` is removed before detection runs. Nothing is ever written to the worktree or to an agent's global config by the engine.
- **AC-14**: The fake agent (`FakeAdapter` in `crates/sy-adapters/fake/`, program `sy-fake-agent` from `crates/sy-fake-agent/`) runs the JSON script in `agent_options.script` step by step (the step list is under *Fake agent*). It reaches the engine only by running `sy-signal`, exactly as a real agent's hook does. It is registered only in `devtools` builds. `pnpm release:check` fails if a release engine lists `fake` in `agent.list` or if the bundle contains `sy-fake-agent`. `dev.synthetic_run` and `dev.stop_runs` are removed. Their socket tests and the UI Load check screen move to `dev.agent_run` with a `flood` script, and spec 0001's load check (12 runs at 51200 B/s with echo, p95 input to echo under 100 ms) passes on it.
- **AC-15**: The Claude adapter reports `capabilities.launch = false` and real detection: it probes `claude --version` and always gives `signed_in: unknown`. On exit 0 it gives `installed: true` with `version` from the first word of its output. On a nonzero exit or a timeout it gives `installed: true`, no version, and the hint `"claude --version did not answer"`. When `claude` isn't found or the probe is refused, it gives `installed: false`. Its `launch` returns `AdapterError::Unsupported` until feature 8.
- **AC-16**: The engine captures the login environment as spec 0001's *Login environment* describes (`$SHELL -l -i -c` with start and end markers, `env -0`, no stdin, 5 second timeout, fallback `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`). It does this at start and on `env.refresh`, and `env.refresh` then reruns detection. `engine.info` gains the optional field `login_env: { status: "ok" | "fallback", message? }` (additive to the frozen stable method), absent until the first capture finishes. A refresh changes only later launches and probes, never a run that is already live. In `devtools` builds only, `SWITCHYARD_LOGIN_ENV=inherit` uses the engine's own environment instead, so tests don't depend on your shell.
- **AC-17**: A run ends in this order, all on the run's task: (1) the exit, the stop's last signal, or a launch failure is seen; (2) the run's signal token is revoked, so new hook calls get `SignalTokenUnknown` and the helper uses its fallback; (3) every held hook gets the decoder's `hold_reply` for a deny; (4) every pending approval becomes `cancelled` and its `approval.decided` is sent; (5) the PTY drain (AC-6) and the hub's final flush and delivery; (6) the terminal `transition_run` with `log_bytes`, then `run.status`; (7) the launch dir is removed, whatever step 6's outcome. If storage is degraded, steps 4 and 6 join 0002's write queue (AC-13 there) and their notifications carry `event_id: null`. `event_id` on `approval.requested` and `approval.decided` is null only in that case.

## Decision

**Chosen option**: Option 1: PTY TUI plus a hook side channel, with pure adapters.

Every agent runs its real interactive TUI in a PTY that the engine owns. A pure, synchronous adapter turns the agent's own hooks (configured for that launch only) and, as a fallback, its output into status, approvals, events and a session ref, and the engine does all the I/O.

**Implementation skills**: `rust-best-practices` (`apollographql/skills`, `.claude/skills/rust-best-practices/`) · `tokio-patterns` (`geoffjay/claude-plugins`, `.claude/skills/tokio-patterns/`) · `rust-async-internals` (`po4yka/rust-skills`, `.claude/skills/rust-async-internals/`) · `rust-error-handling` (`po4yka/rust-skills`, `.claude/skills/rust-error-handling/`) · `rust-database` (`po4yka/rust-skills`, `.claude/skills/rust-database/`)

**Smaller calls made with it** (the runner up is in brackets):
- Adapters are pure: plans out, reports in, no I/O [async adapters with injected handles].
- Status is split by who knows: the engine owns `queued`, `starting`, `stopped` and `lost`, and the adapter reports `running` and `waiting_input` and classifies the exit [exit code alone decides].
- Hooks reach the engine through `sy-signal`, a separate client only binary, over `engine.sock` as a `signal` client with a per run token [a `switchyardd signal` subcommand; a separate `signal.sock`].
- One blocking `signal.hook` call per hook, whose reply the adapter writes [report, then await].
- The fallback when a hook can't reach the engine is always the agent's own prompt, never allow [deny].
- Output patterns plus a quiet timer, after ANSI stripping with `vte` [hooks only; `strip-ansi-escapes`].
- Feature 4 builds a minimal real launcher, and feature 5 adds supervision [contract only].
- The approvals table, notifications and `approval.respond`/`approval.list` land now, and feature 12 adds the inbox and policies [in memory only].
- Detection results stay in memory and are exposed by `agent.list` and `agent.detect` [persist in the `agent` table].
- Stop: the adapter's graceful input, then SIGHUP, then SIGKILL [signals only].
- The fake agent is a real binary in a real PTY, driven by JSON steps in `agent_options`, in devtools builds only [an in process fake].
- `serde_json` and `vte` join `sy-adapter-api`'s allow list, and `tokio` leaves it [a JSON type of our own].
- Launch files go in `<data dir>/agent-runtime/<run_id>/` [inside the worktree].
- The Claude adapter does detection only until feature 8 [unregister it].

## Rationale

Reasoning and options: see [rationale.md](rationale.md).

## Feature design

### Contract (`sy-adapter-api`)

The sketch below is the contract's shape. Names are binding, and field order and derives are the builder's call. Every `pub` item gets a doc comment.

```rust
pub const CONTRACT_VERSION: u32 = 1;

pub trait AgentAdapter: Send + Sync + 'static {
    fn id(&self) -> &str;                       // "claude", "fake", "gen_<uuidv7 hex>"
    fn display_name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn programs(&self) -> Vec<String>;          // the only programs probes and launches may run
    fn detection_probes(&self) -> Vec<Probe>;
    fn interpret_detection(&self, results: &[ProbeResult]) -> Detection;
    fn launch(&self, req: &LaunchRequest) -> Result<Launch, AdapterError>;
}

pub struct Capabilities { pub launch: bool, pub approvals: bool, pub resume: bool,
                          pub events: bool, pub status_source: StatusSource }
pub enum StatusSource { Hooks, Patterns, ExitOnly }

pub struct Probe { pub program: String, pub args: Vec<String>, pub timeout_ms: u32 }
pub struct ProbeResult { pub probe: Probe, pub outcome: ProbeOutcome,
                         pub stdout: Vec<u8>, pub stderr: Vec<u8> }   // each capped at 64 KiB
pub enum ProbeOutcome { Exited(ExitInfo), TimedOut, NotFound, Refused, SpawnError(String) }
pub struct Detection { pub installed: bool, pub signed_in: SignedIn,
                       pub version: Option<String>, pub hint: Option<String> }
pub enum SignedIn { Yes, No, Unknown }

pub struct LaunchRequest {
    pub run_id: String, pub prompt: String, pub cwd: String,
    pub options: serde_json::Value,             // workspace.agent_options
    pub resume: Option<String>,                 // an agent_session_ref (None until feature 23)
    pub launch_dir: String,                     // <data dir>/agent-runtime/<run_id>
    pub signal: SignalEndpoint, pub size: PtySize,
}
pub struct SignalEndpoint { pub helper: String, pub socket: String, pub token: String }
pub struct PtySize { pub cols: u16, pub rows: u16 }

pub struct Launch { pub plan: LaunchPlan, pub decoder: Box<dyn RunDecoder> }
pub struct LaunchPlan {
    pub program: String, pub args: Vec<String>,
    pub env_set: Vec<(String, String)>, pub env_remove: Vec<String>,
    pub files: Vec<LaunchFile>,                 // written under launch_dir, 0600
    pub initial_input: Option<Vec<u8>>,         // typed once the run is `running`
}
pub struct LaunchFile { pub rel_path: String, pub contents: Vec<u8> }   // no `..`, not absolute

pub trait RunDecoder: Send {
    fn on_output(&mut self, chunk: &[u8], now_ms: i64) -> Vec<Report>;
    fn on_hook(&mut self, call: &HookCall, now_ms: i64) -> HookOutcome;   // must always answer
    fn on_tick(&mut self, now_ms: i64) -> Vec<Report>;
    fn hold_reply(&mut self, key: &str, decision: &Decision) -> HookReply;
    fn input_reply(&mut self, key: &str, decision: &Decision) -> Vec<u8>;
    fn stop(&mut self) -> StopPlan;
    fn on_exit(&mut self, exit: &ExitInfo) -> ExitOutcome;
}

pub struct HookCall { pub hook: String, pub args: Vec<String>, pub stdin: Vec<u8> }
pub struct HookOutcome { pub reports: Vec<Report>, pub response: HookResponse }
pub enum HookResponse { Reply(HookReply), Hold(ApprovalRequest) }
pub struct HookReply { pub stdout: Vec<u8>, pub exit_code: i32 }

pub enum Report {
    Status(LiveStatus),                          // Running | WaitingInput
    ApprovalRequested(ApprovalRequest),          // answered by typing (input_reply)
    ApprovalSettled { key: String, outcome: SettledOutcome },   // Approved | Denied | Unknown
    Event(AgentEvent),                           // Message | CommandRun | FileChanged (0002 shapes)
    SessionRef(String),
}
pub struct ApprovalRequest { pub key: String, pub summary: String,
                             pub detail: ApprovalDetail, pub raw: serde_json::Value }
pub enum ApprovalDetail {                        // the variant is approval.kind
    Command { command: String, cwd: Option<String> },
    FileEdit { path: String, diff: Option<String> },
    Fetch { url: String },
    McpTool { server: String, tool: String, input: serde_json::Value },
    Other { tool: String, input: serde_json::Value },
}
pub struct Decision { pub verdict: Verdict, pub message: Option<String> }   // Approve | Deny
pub struct StopPlan { pub input: Vec<u8>, pub grace_ms: u32 }
pub struct ExitInfo { pub code: Option<i32>, pub signal: Option<i32> }
pub enum ExitOutcome { Finished, Failed }

pub enum AdapterError { Unsupported(String), InvalidOptions(String) }   // thiserror
```

**Contract rules** (in the trait docs, binding on every adapter):
- `on_hook` must always answer. An unknown hook name, or a payload it can't parse, gets the same fallback reply that hook's `hook_command` was given, with no reports.
- An `ApprovalRequest.key` is the adapter's own id for the request (for example the agent's tool call id). It must be unique among the run's pending approvals. The engine maps it to the `apr_` id, and `hold_reply`, `input_reply` and `ApprovalSettled` use the key.
- Decoder calls should be cheap (parse, match, return). The engine makes them in order on the run's task and never in parallel.

**Helpers in the same crate** (pure, unit tested):
- `OutputScanner`: feeds chunks to a `vte::Parser`, keeps the printable text, yields complete lines (and the current partial line) with escapes removed, and remembers the time of the last output, for pattern and quiet period checks. A sequence split across chunks is handled.
- `hook_command(endpoint, hook, fallback: &HookReply) -> HookCommand { argv: Vec<String>, shell: String }`: the `sy-signal` invocation for one hook, as an argument array and as a POSIX single quoted shell string. The shell string is for agents whose hook config takes a command string. Every argument is quoted, including paths with spaces or quotes. The argv is `[helper, "--socket", socket, "--token", token, "--hook", hook, "--fallback-exit", n, "--fallback-stdout-b64", b64, "--"]`, and the agent's own arguments follow the `--`.

### Runtime shape (engine)

- **Ports in `sy-core`** (traits, real ones in `sy-infra`, test doubles beside the tests): `Pty` (spawn `{ program path, args, cwd, env, size }` and return a child with a blocking reader, a writer, `resize`, `wait`, `pid`, `pgid` and `signal_group(sig)`), `ProcessRunner` (run a probe with a timeout and capped output), `RuntimeDir` (create, write files `0600`, remove, sweep) and `LoginEnv` (capture, cached, plus `resolve(program) → path`).
- **`sy-core/src/agents/`**: the use case. `AgentRuns::start` mints the token, registers it, writes the launch dir, calls `launch`, spawns, and hands the PTY reader to the hub's producer. It then spawns one driver task per run that owns the `RunDecoder`, the run's approvals (key to `apr_` id, held replies) and its token validity. It takes two lanes: the droppable output lane (AC-5) and a lossless control lane carrying hook calls (each with a oneshot for its reply), decisions (each with a oneshot for `approval.respond`'s answer), stop, exit and ticks. All decoder calls for a run therefore happen on one task, in order, and every approval change for a run is decided there (AC-10). Approval and status writes go through `Store` and broadcast its committed events.
- **Signal routing**: the server keeps only `token → the driver's control sender`, and the driver removes its entry when it revokes the token (AC-17). `signal.hook` checks the size, looks up the sender, sends the call, and waits on the oneshot while still reading the connection. The driver runs `on_hook`, then charges the rate limit for a reply given at once (AC-7). A held reply waits in the driver's map, keyed by approval id. If the `signal.hook` connection closes, the server tells the driver, which drops the held entry and makes the approval `expired` if it is still pending.
- **Store failures on the run's task**: status, event and approval writes use 0002's degraded write queue like every other caller (they never block the task beyond the store's own call). A failed `create_approval` follows AC-9's fallback.
- **New crates**: `crates/sy-signal/` (bin, `sy-protocol` and `sy-client` only, like `sy-devbridge`), `crates/sy-fake-agent/` (bin, dev and test only, no workspace crate dependencies), `crates/sy-adapters/fake/` (lib, `sy-adapter-api` only).
- **Finding the helper binaries**: the engine looks for `sy-signal` and (devtools) `sy-fake-agent` beside its own executable (`Contents/MacOS` in the bundle, `target/<profile>/` in development). In devtools builds, `SWITCHYARD_HELPER_DIR` overrides that directory. The daemon test harness runs `cargo build -p sy-signal -p sy-fake-agent` once (an argument array) before it spawns an engine.

### Fake agent

`agent_options = { script: Step[], quiet_after_ms?: u32, waiting_pattern?: string }`. `FakeAdapter::launch` writes `script.json` into the launch dir and runs `sy-fake-agent --script <path> --signal <helper> --socket <socket> --token <token>`. Steps run in order, and each waits on an event, never the clock:

| Step | Does |
|---|---|
| `{ "print": "text" }`, `{ "print_b64": "…" }` | Writes the bytes to its terminal |
| `{ "status": "running" \| "waiting_input" }` | Calls `sy-signal --hook status` |
| `{ "session": "id" }` | Calls `sy-signal --hook session` |
| `{ "event": { "message" \| "command_run" \| "file_changed": {…} } }` | Calls `sy-signal --hook event` with the 0002 payload shape |
| `{ "approval": { "kind", "summary", "detail", "on_approve": Step[], "on_deny": Step[] } }` | Calls `sy-signal --hook approval` and blocks. Reply `{"decision":"approve"}` or `{"decision":"deny","message"?}` picks the branch. The fallback `{"decision":"ask"}` prints `Allow <summary>? [y/n] ` and reads a line from its terminal instead |
| `{ "expect_input": "literal" }` | Reads terminal input until a line contains the literal |
| `{ "echo_until": "literal" }` | Echoes every input byte until the literal is seen |
| `{ "flood": { "bytes_per_sec", "total_bytes"?, "echo"? } }` | Writes seeded printable bytes at that rate (forever when no total), echoing input when `echo` is set |
| `{ "write_file": { "path", "contents" } }`, `{ "delete_file": { "path" } }` | Changes a file inside its cwd (a path that is absolute or contains `..` fails the script) |
| `{ "hang": true }` | Blocks forever and ignores input |
| `{ "ignore_sighup": true }` | Installs a SIGHUP listener so SIGHUP no longer ends it |
| `{ "die_by_signal": "SIGSEGV" \| "SIGKILL" \| "SIGTERM" }` | Sends itself that signal |
| `{ "exit": code }` | Exits with that code (the end of the script is `exit 0`) |

A malformed script makes `launch` return `InvalidOptions`. The fake decoder maps the `status`, `session`, `event` and `approval` hooks as named, reports `Hold` for `approval`, and classifies exit 0 as finished and anything else as failed. `quiet_after_ms` and `waiting_pattern` switch on the `OutputScanner` fallback, so the pattern path is testable. Its `stop()` input is `\x03` (Ctrl C), with a 5000 ms grace.

### Data model sketch

Migration 2, `0002_approval.sql`, additive per spec 0002 (it takes the usual pre migration backup):

| Table | Columns and constraints |
|---|---|
| `approval` (apr_) | `id` TEXT PK · `run_id` FK → `run(id)` ON DELETE CASCADE · `kind` TEXT (no CHECK, the Rust `ApprovalKind`: `command`, `file_edit`, `fetch`, `mcp_tool`, `other`) · `summary` TEXT · `detail` TEXT `CHECK (json_valid(detail))` (`{ <typed fields of the kind>, raw, raw_truncated? }`) · `status` TEXT `CHECK (status IN ('pending','approved','denied','expired','cancelled'))` · `requested_at` INTEGER · `decided_at?` INTEGER · `decided_by?` TEXT `CHECK (decided_by IN ('user','policy','agent'))` · `decided_by_ref?` TEXT · `response?` TEXT `CHECK (json_valid(response))`. `STRICT`. Index `ix_approval_pending` on (`requested_at`) `WHERE status = 'pending'`. Index `ix_approval_run` on (`run_id`, `requested_at`, `id`). |

Relationship: one run has many approvals (1:N). A permanent delete of a task or project cascades to them through `run`.

**New journal kinds** (scope ids: project, task, workspace, run):
- `approval_requested` `{ approval_id, kind, summary }`
- `approval_decided` `{ approval_id, status, decided_by?, decided_by_ref?, message? }`

**New `Store` methods**: `create_approval` (row plus event), `decide_approval` (`pending` to any end state, row plus event, `StoreError::ApprovalNotPending` otherwise), `get_approval`, `list_approvals` (AC-11's filters and paging), `cancel_pending_approvals(run)` and `expire_pending_approvals()` (startup), each returning the committed events.

**In memory only**: signal tokens, decoder state, held hook replies, detection results, the login environment, and rate limit buckets. Launch dir files live on disk only for the run's life.

### State transitions

**Run** (0002's machine, unchanged; who triggers each move in feature 4):

| Move | Triggered by |
|---|---|
| `queued → starting` | the engine, before the spawn |
| `starting → running` | decoder `Status(Running)`, or the first output byte |
| `running ↔ waiting_input` | the AC-6 rule: `waiting_input` while any approval is pending or the decoder last reported it, else `running` |
| `starting \| running \| waiting_input → finished \| failed` | process exit seen before any stop request, as `on_exit` classifies (`exited`) |
| `starting → failed` | launch or spawn failure: an adapter error, a refused program or file, a spawn error (`spawn_failed`) |
| any live state, or `queued` `→ stopped` | `run.stop` or engine shutdown (`user_stop`) |

**Approval**: `pending → approved | denied` (you, in the inbox or the terminal) · `pending → expired` (held hook connection closed, or engine start) · `pending → cancelled` (run ended or stopped). End states never change.

### API surface (engine protocol additions)

All new methods need a compatible `hello` and obey 0002's storage gate (`StorageUnavailable` in degraded mode). The protocol minor goes up by one.

| Method | Kind | Key inputs | Key outputs | Auth | Key errors |
|---|---|---|---|---|---|
| `agent.list` | request | none | `{ agents: AgentInfo[] }`, where `AgentInfo = { id, display_name, capabilities: { launch, approvals, resume, events, status_source }, detection?: { installed, signed_in, version?, path?, hint?, checked_at } }` | compatible `hello` | `StorageUnavailable` |
| `agent.detect` | request | `agent_id?` (all when absent) | `{ agents: AgentInfo[] }` after the pass ends | compatible `hello` | `AgentNotFound` |
| `env.refresh` | request | none | `{ login_env: { status, message? } }`, then detection reruns in the background | compatible `hello` | (none) |
| `run.stop` | request | `run_id` (req) | `{}` | compatible `hello` | `RunNotFound`, `RunNotLive` |
| `approval.list` | request | `status?` (default `pending`), `run_id?`, `after?: { requested_at, id }`, `limit?` (0 or absent means 100, max 500) | `{ approvals: Approval[], next?: { requested_at, id }, has_more }` | compatible `hello` | `InvalidParams` (unknown status, malformed `after`) |
| `approval.respond` | request | `approval_id` (req), `decision: "approve" \| "deny"` (req), `message?` (max 4 KiB) | `{ approval: Approval }` | compatible `hello` | `ApprovalNotFound`, `ApprovalNotPending`, `InvalidParams` |
| `approval.requested` | notification, to every non signal connection | (none) | `{ approval: Approval, event_id: i64 \| null }` | (none) | (none) |
| `approval.decided` | notification, to every non signal connection | (none) | `{ approval: Approval, event_id: i64 \| null }` | (none) | (none) |
| `signal.hook` | request, `signal` connections only | `token` (req), `hook` (req), `args: string[]`, `stdin_b64` | `{ stdout_b64, exit_code }`, possibly after a hold | `hello { client: "signal" }` | `NotAllowedForClient`, `InvalidRequest` (a second call while one is open), `SignalTokenUnknown`, `SignalPayloadTooLarge`, `SignalRateLimited`, `StorageUnavailable` |
| `dev.agent_run` | request, devtools only | `agent_id` (req), `prompt?` (default `""`), `options?` (default `{}`), `count?` (default 1, max 32), `cols?`, `rows?` (default 120 by 40) | `{ task_id, workspace_ids, run_ids }` | compatible `hello` | `AgentNotFound`, `AgentLaunchUnsupported`, `InvalidParams` |
| `engine.info` (existing, stable) | request | none | adds optional `login_env: { status: "ok" \| "fallback", message? }` | none (stable) | (none) |
| `session.hello` (existing, stable) | request | `client` gains `"signal"` | unchanged | none (stable) | (none) |
| `dev.synthetic_run`, `dev.stop_runs` | removed | | | | |

`Approval = { id, run_id, workspace_id, kind, summary, detail, status, requested_at, decided_at?, decided_by?, decided_by_ref?, message? }`.

**New error codes** (`sy-protocol`, in their area blocks): `NotAllowedForClient = -32002` (session) · `RunNotLive = -32044` (run) · `AgentNotFound = -32060`, `AgentLaunchUnsupported = -32061` (agent) · `ApprovalNotFound = -32070`, `ApprovalNotPending = -32071` (approval) · `SignalTokenUnknown = -32080`, `SignalPayloadTooLarge = -32081`, `SignalRateLimited = -32082` (signal).

### Value sourcing

| Action | Value produced / displayed | Source |
|---|---|---|
| Launch | prompt | `dev.agent_run.prompt` now. Feature 8 passes `task.prompt` (or the follow up input) |
| Launch | cwd | `workspace.worktree_path` (created as a plain dir by `dev.agent_run`, a git worktree from feature 8) |
| Launch | options | `workspace.agent_options` (from `dev.agent_run.options`) |
| Launch | resume | always `None` in feature 4. Feature 23 reads `run.resumes_run_id` → that run's `agent_session_ref` |
| Launch | launch dir | `<data dir>/agent-runtime/<run_id>`, the data dir from `sy-client`'s resolver |
| Launch | signal helper path | `sy-signal` beside `current_exe()` (devtools: `SWITCHYARD_HELPER_DIR`) |
| Launch | signal socket | `EnginePaths::socket` from `sy-client` (already resolved, including the `/tmp` fallback) |
| Launch | signal token | 16 bytes from `getrandom`, lowercase hex, minted per run |
| Launch | PTY size | `dev.agent_run.cols/rows`, default 120 by 40. Then `run.resize` |
| Launch | environment | the `LoginEnv` cache, plus `env_set` and minus `env_remove`, plus `TERM=xterm-256color`, `COLORTERM=truecolor`, `SWITCHYARD_SIGNAL_SOCKET`, `SWITCHYARD_SIGNAL_TOKEN` |
| Launch | program path | the plan's `program`, resolved on the login `PATH`, and it must be in `programs()` |
| Launch | fake agent path | `sy-fake-agent` beside `current_exe()`, given to `FakeAdapter::new` at registration |
| Run row | `pid`, `pgid` | the PTY child (a new session, so `pgid = pid`). `pid_started_at` stays NULL until feature 5 |
| Run end | status, `exit_code` | `on_exit(ExitInfo)` from the PTY child's `wait` |
| Run end | `end_reason` | `exited` (process exit), `user_stop` (`run.stop` or shutdown), `spawn_failed` (AC-4) |
| Status | `running` / `waiting_input` | decoder reports, the first output byte (AC-6), approval rules (AC-10) |
| Quiet timer | now | `Clock::now_ms()`, passed to `on_tick` every 250 ms |
| Quiet timer | period | the adapter's own setting (fake: `agent_options.quiet_after_ms`), default off |
| Approval | `kind`, `summary`, `detail`, `raw` | the decoder's `ApprovalRequest` (`kind` from the `ApprovalDetail` variant) |
| Approval | `requested_at`, `decided_at` | `Clock::now_ms()` |
| Approval | `decided_by`, `decided_by_ref` | `user` + `inbox` (`approval.respond`), `user` + `terminal` (`ApprovalSettled`), NULL for `expired` and `cancelled` |
| Approval | `response.message` | `approval.respond.message`. Stop uses `"run stopped"` |
| Approval wire | `workspace_id` | joined from `run.workspace_id` |
| Agent event | `actor`, `actor_ref` | `agent`, and the run's `workspace_id` |
| Detection | `installed`, `signed_in`, `version`, `hint` | `interpret_detection` over the probe results |
| Detection | `path` | `LoginEnv::resolve(program)` of the first probe |
| Detection | `checked_at` | `Clock::now_ms()` when the pass ended |
| Login env | status, message | the capture result (AC-16) |
| Limits | 256 KiB payload, 100/s burst 500, 4 KiB message, 200 character summary, 64 KiB detail, 5 s probe, 64 KiB probe output | constants in this spec |

### Key invariants

- An adapter never performs I/O. Every process, file, socket and environment read is the engine's (enforced by AC-2's check and the trait shape).
- A probe or launch runs only a program its adapter listed in `programs()`.
- All decoder calls for one run happen on that run's driver task, in order, so a decoder needs no locking. That task is also the only place the run's approvals change and its token is revoked.
- The hub, the log and the PTY reader never wait on the decoder, a hook, or a client (the decoder copy is lossy, the output is not).
- A held hook gets exactly one reply, and it always matches the row's final status: the inbox decision, the terminal outcome, the stop or run end deny, the fallback when no row could be created, or nothing because its connection closed (then `expired`).
- An approval leaves `pending` exactly once (first change wins), and each change writes one `approval_decided` event in the same transaction.
- A run is `waiting_input` whenever it has a pending approval.
- Process group signals are never sent after the main process has been reaped.
- No new approval can appear on a run after its token is revoked.
- The fallback reply for any approval hook is the agent's own prompt, never an allow.
- A signal token is valid only while its run is live, and a signal connection receives no notifications.

### Security model

- **One user.** Everything runs as you, and the socket's peer user check (0001) remains the boundary. Agents also run as you, so they could open `engine.sock` anyway. The signal token is attribution (which run is calling), not a security wall, and that's acceptable for one user on one machine. Feature 33 designs any deliberate agent facing control surface.
- **Tokens and credentials.** Adapters can't read files, so they can't read login tokens. Probes and launches run only declared programs. The engine adds no credential variable. Detection asks each CLI and never reads its files.
- **Shell strings.** The engine spawns only argument arrays. Some agents' hook configs take a command string that the agent runs through a shell. `hook_command` builds that string by quoting every argument, and unit tests cover spaces, quotes, `$` and newlines in paths.
- **The token on argv.** It's visible to your own processes through `ps`, which is within the one user boundary above.
- **Approval detail and raw payloads** may contain secrets an agent was about to use. They are stored locally like run logs (0002), never logged, and masked on display later (features 6 and 14).
- **Fallbacks fail toward you.** An unreachable engine never auto approves anything.

### Observability

- Each run's driver task runs in a `tracing` span with `run_id` and `agent_id`.
- Info: launch (the program name and argument count, never the args, which can hold the prompt), exit and stop.
- Debug: each hook call's name, size, outcome and latency.
- Warn: refused programs, dropped decoder chunks, rate limiting, the login env fallback, a probe timeout.
- Never logged: hook payloads, approval detail, env values, tokens or prompts.

### Configuration required

- No new user settings or credentials.
- `SWITCHYARD_SIGNAL_SOCKET`, `SWITCHYARD_SIGNAL_TOKEN`: set by the engine in each agent's environment, for adapters whose hooks read env instead of argv.
- `SWITCHYARD_HELPER_DIR` (devtools only): where to find `sy-signal` and `sy-fake-agent`.
- `SWITCHYARD_LOGIN_ENV=inherit` (devtools only): skip the login shell capture in tests.

### Critical test scenarios

- Happy path: a fake script prints, reports `running`, a session, a message and a file change, writes a file, then exits 0. The run goes `queued → starting → running → finished` with `exit_code 0`. The log equals what a client saw. The events are journaled with `actor = agent`. `agent_session_ref` is set. The launch dir is gone afterwards. Verifies **AC-4**, **AC-5**, **AC-6**, **AC-8**, **AC-13**, **AC-14**.
- Approval round trip: a fake `approval` step holds. The client gets `approval.requested`, and the run is `waiting_input`. `approval.respond approve` runs the `on_approve` branch, the run is back to `running`, and `approval.decided` arrives. Deny runs `on_deny` with the message. Two pending at once keep the run waiting until both are answered. Verifies **AC-9**, **AC-10**.
- Fallback: kill the engine while a fake `approval` step is held. The fake prints its own `[y/n]` prompt, never continues as approved, and accepts a typed `y`. A bad token gives `SignalTokenUnknown`, and the fallback runs. Verifies **AC-7**, **AC-10**.
- Approval ends: a held hook whose `sy-signal` is killed becomes `expired`. A run that exits with a pending approval cancels it. After an engine restart, a pending row is `expired`. `approval.respond` on any of them gives `ApprovalNotPending`. `approval.list` pages pending and filters by run. Verifies **AC-10**, **AC-11**.
- Stop: `run.stop` during a held approval denies it ("run stopped"), cancels it, sends Ctrl C, and ends the run `stopped` with `user_stop`. With `ignore_sighup` plus `hang`, SIGKILL lands about 10 s later. A second `run.stop` is a no op, and an ended run gives `RunNotLive`. Verifies **AC-12**.
- Exit classification and spawn failure: `exit 3` ends `failed` with `exit_code 3`. `die_by_signal SIGSEGV` ends `failed`. A malformed script and a missing program both end `failed` with `spawn_failed` and a reason line in the log. An unknown agent gives `AgentNotFound` and the Claude adapter gives `AgentLaunchUnsupported`, with no rows created. Verifies **AC-4**, **AC-6**, **AC-15**.
- Signal limits: a 300 KiB hook payload gives the fallback. A script that loops `status` 2000 times quickly sees `SignalRateLimited` past the burst and one warning, and an `approval` step right after it still holds and is answered (holds are never charged). A second `signal.hook` on an open signal connection gets `InvalidRequest`. Verifies **AC-7**.
- Approval races (driver unit tests with a scripted decoder, then socket tests): `approval.respond` against `run.stop`, against the process exiting, against a terminal `ApprovalSettled` and against a closed hook connection. In every case exactly one change lands, the held hook's reply matches the row, and the loser gets `ApprovalNotPending` (or a no op). A `running` report while an approval is pending leaves the run `waiting_input`. A duplicate pending key is dropped. A failing `create_approval` gives the held hook the fallback. Verifies **AC-6**, **AC-9**, **AC-10**.
- Run end order: a fake with a held approval exits. The client sees `approval.decided` (`cancelled`) before `run.status`, the last `run.output` before `run.status`, and a hook call after the exit gets `SignalTokenUnknown`. The launch dir is gone afterwards. With `ignore_sighup` and `hang`, engine shutdown ends within about 8 s. Verifies **AC-12**, **AC-17**.
- Pattern fallback (decoder unit tests with an injected clock): `waiting_pattern` matched across a chunk split with ANSI escapes in it reports `waiting_input`. `quiet_after_ms` reports it only after the quiet period, and new output reports `running`. A full decoder queue drops chunks without slowing the hub. Verifies **AC-5**, **AC-6**.
- Token rule: adding `std::fs::read` to an adapter crate fails `pnpm boundaries:check`. A probe naming a program outside `programs()` reads `installed: false`. The agent's environment equals the login env plus the listed variables. Verifies **AC-2**.
- Detection: with a stub `claude` script on a test `PATH` that prints `2.1.325 (Claude Code)`, the result is `installed`, version `2.1.325`, `signed_in: unknown`. A missing program gives `installed: false` with no spawn, and a probe that sleeps past the timeout is killed at 5 s. Concurrent `agent.detect` calls share one pass. Verifies **AC-3**, **AC-15**.
- Login env: a shell whose startup files print noise yields only the marked env, a hanging shell falls back after 5 s with `login_env.status = fallback`, and `env.refresh` reruns detection. Verifies **AC-16**.
- Load check: 12 fake `flood` runs at 51200 B/s with echo in the Load check screen give p95 input to echo under 100 ms. Release check: a release engine has no `dev.agent_run` and no `fake`, and the bundle has `sy-signal` but no `sy-fake-agent`. Verifies **AC-14**.
- Contract seam: `sy-core` builds and its tests pass with only the fake and Claude adapters registered from `sy-daemon`, and `pnpm boundaries:check` passes with the new crate rules. Verifies **AC-1**.

## Build plan

Tracer Bullet: first one real fake agent run through a real PTY over the real socket, then the signal channel, then approvals, then detection and the release gates. Migration 2 lands with the approvals milestone, the first one that needs it.

**Milestone 1: a fake run through a real PTY, end to end**
1. `sy-adapter-api`: the contract types and traits, `CONTRACT_VERSION`, `OutputScanner` (with `vte`) and `hook_command`, all unit tested. Update the crate rules: `sy-adapter-api` allows `serde`, `serde_json`, `thiserror` and `vte`, and the new `sy-signal`, `sy-fake-agent` and `sy-adapters/fake` rows, in `check-boundaries.mjs`, together with the forbidden API source scan for `sy-adapters/*`. Satisfies **AC-1**, **AC-2**.
2. `sy-core`: the `Pty`, `ProcessRunner`, `RuntimeDir` and `LoginEnv` ports. The registry takes `Arc<dyn AgentAdapter>` with `&str` ids. `agents/` holds the start path (AC-4's order) and the per run driver task (the two lanes, decoder loop, ticks, the AC-6 status rule, the AC-17 end order). Unit tests use port doubles and a scripted decoder. Satisfies **AC-1**, **AC-6**, **AC-17**.
3. `sy-infra`: `PortablePty` (portable-pty, a new session per child, `signal_group` through `rustix`), the probe runner, the runtime dir (modes, sweep), and integration tests against real processes. Satisfies **AC-4**, **AC-5**, **AC-13**.
4. `sy-fake-agent` with `print`, `exit`, `echo_until`, `flood`, `expect_input` and `write_file`/`delete_file`, plus `FakeAdapter` (exit classification, `InvalidOptions` on a bad script). Satisfies **AC-14**.
5. `sy-daemon` and `sy-protocol`: register the fake adapter (devtools), `dev.agent_run`, `run.stop` (input, grace, SIGHUP, SIGKILL only until reaped, first of stop or exit wins, and the shutdown path with its 3 s grace cap), `RunNotLive`, `AgentNotFound`, `AgentLaunchUnsupported`. Remove `dev.synthetic_run` and `dev.stop_runs`, port their socket tests and the Load check screen to `dev.agent_run` with `flood`, then `pnpm protocol:gen`. Run the load check again. Satisfies **AC-4**, **AC-5**, **AC-6**, **AC-12**, **AC-13**, **AC-14**, **AC-17**.

**Milestone 2: the signal side channel**
6. `sy-signal` (stdin and argument caps, hello as `signal`, one call, fallback on any failure) and its own integration test. `ClientKind::Signal`, the `signal` connection rules (`NotAllowedForClient`, no notifications, one open call, reading on while a call is open), `signal.hook` routing to the driver's sender, the size cap, the rate limiter charged after the decoder answers, and token revocation by the driver. Satisfies **AC-7**, **AC-17**.
7. The fake agent's `status`, `session` and `event` steps, the fake decoder's hook mapping, `Event` journaling with `actor = agent`, `SessionRef` saving, and the `waiting_pattern`/`quiet_after_ms` fallback. Satisfies **AC-6**, **AC-8**.

**Milestone 3: approvals**
8. Migration 2 (`approval` table and indexes), the `Approval` domain type and its state rule, the new `Store` methods with their events, and the startup `expire_pending_approvals`. Store integration tests. Satisfies **AC-9**, **AC-10**, **AC-11**.
9. Hold and keystroke approvals in the driver as the only arbiter (key mapping, first change wins), `approval.requested`/`approval.decided`, `approval.respond` through the driver, `approval.list`, the `waiting_input` rule, terminal settlement with matching held replies, expiry on a closed hook, the create failure fallback, cancel on run end in AC-17's order, and the deny and cancel step in `run.stop`. The fake `approval` step with branches and its `[y/n]` fallback. The race tests, then socket tests for every scenario. Satisfies **AC-6**, **AC-9**, **AC-10**, **AC-11**, **AC-12**, **AC-17**.

**Milestone 4: detection, login environment and release gates**
10. `LoginEnv` in `sy-infra` per spec 0001 (markers, `env -0`, timeout, fallback, the devtools `inherit` seam), `engine.info.login_env` and `env.refresh`. Satisfies **AC-16**.
11. Detection passes (start, `agent.detect`, after `env.refresh`, a shared pass in flight), the program allowlist for probes and launches, `agent.list`, and the Claude adapter's detection. Satisfies **AC-2**, **AC-3**, **AC-15**.
12. Build and bundle `sy-signal` beside `switchyardd` (`build-engine.mjs`, universal, the same signing identity), and extend `release-check.mjs` (no `fake`, no `sy-fake-agent`, `sy-signal` present). Add the observability spans and the never logged test (no payload, token or env value in `engine.log` after the scenario suite). Satisfies **AC-2**, **AC-14**.

## Consequences

**Positive**:
- You always get the agent's real TUI, so switchyard never lags behind an agent's own features or hides what it shows.
- Status, approvals and events come from the agents' own hook systems, which are structured and documented, not scraped from the screen. Scraping is only the fallback.
- Pure adapters test as plain functions, and a new agent is one crate plus one registration line. The token rule is mechanically checked.
- The fake agent exercises the exact production path (PTY, `sy-signal`, socket), so orchestration tests stay realistic while staying fast and deterministic.
- Approvals have their real table, events and methods, so feature 12 only adds the inbox, policies and modes.

**Negative / tradeoffs**:
- Each agent's hook system is different and changes across CLI versions. Every adapter must be checked against a live CLI, and a CLI update can silently break status or approvals until the adapter follows. Detection should report the version so drift is visible.
- Not every CLI's hooks can hold a permission request until it's answered. Those adapters fall back to keystroke approvals read from the screen (`input_reply`), which are more fragile.
- A held approval blocks that agent until you answer, or until its own hook timeout expires it. That's by design, but a forgotten request stalls work (feature 10's notifications matter).
- Each hook call starts a `sy-signal` process (a few milliseconds), and a second bundled binary must be built universal and signed.
- Pattern fallback and the quiet timer can misread a TUI, and the decoder's output copy can drop chunks under overload, so pattern based status is best effort.
- The login environment capture adds up to 5 seconds before the first detection on a slow shell.
- Removing `dev.synthetic_run` rewrites 0002's devtools tests and the Load check screen, and 0001's load check must pass again on the fake agent.
- `sy-adapter-api` gains `serde_json` and `vte`, a change to 0001's crate table.
- `pid_started_at` stays NULL until feature 5, so orphan detection isn't possible before then.
- Engine shutdown can now take up to about 8 seconds (a 3 s graceful step, then 0001's SIGHUP and 5 s wait), instead of 0001's 5 seconds.
- A grandchild that outlives the agent keeps running after the run ends, because group signals stop once the main process is reaped. Feature 5 cleans these up.

**Neutral**:
- The protocol minor version goes up, and `ClientKind` gains `signal`.
- ACP isn't used. It can be added later as a second driver behind the same `RunDecoder` reports if an adapter needs it.
- `docs/.agent-cache/research/agent-cli-integration.md` holds this feature's unconfirmed web notes, for adapter features to check again.

## Follow-up

- [x] Spec 0001: update the crate table for `sy-adapter-api` (`serde`, `serde_json`, `thiserror`, `vte`) and add rows for `sy-signal`, `sy-fake-agent` and `sy-adapters/fake`. Note that the fake agent replaced `dev.synthetic_run` in the load check, and that shutdown now runs AC-12's sequence (a graceful step capped at 3 s, then SIGHUP, 5 s, SIGKILL). (Done by `/architect` on confirmation of this spec.)
- [x] Spec 0002: AC-15's dev fixture is now produced by `dev.agent_run`, and `dev.stop_runs` is replaced by `run.stop`. (Noted in 0002 by `/architect` on confirmation of this spec.)
- [ ] Feature 8 (Claude adapter): launch with a per run `--settings` file holding the hooks. Confirm which hook can hold a permission request (`PreToolUse` with `permissionDecision`, as a search snippet suggested), how its timeout is set, and whether a no token sign in check exists (a `claude auth status` style command) before `signed_in` stops being `unknown`.
- [ ] Features 15 to 17: confirm per CLI, against the installed version, the per launch hook config (Codex `-c`, a Gemini settings path, an OpenCode plugin via its config env), which hook can hold approvals (Codex `PermissionRequest`, Gemini `BeforeTool`, OpenCode `permission.asked` were all unconfirmed), and where none can, use keystroke approvals.
- [x] Feature 5: fill `pid_started_at`, add hang, runaway and output limits, handle grandchildren that keep the PTY open after the agent exits, and tune the stop timings. Covered by spec 0004. Note: the main process is now reaped only after its leftover sweep, and AC-12's stop sequence ends once the exit is observed; only the sweep signals the group after that.
- [ ] Feature 12: "allow for this session" as an optional decision field, policies (`decided_by = policy`), the default permission mode, and the inbox UI on `approval.list`. The inbox should explain an `ApprovalNotPending` answer, for example "this request expired when the agent's own hook timed out, answer it in the terminal".
- [ ] Feature 18: the generic adapter's config format on this contract (argv template, hook or pattern status, exit code map, `programs`).
- [ ] Feature 19: persist detection in the `agent` table, add fix hints, and onboarding.
- [ ] Feature 23: pass `LaunchRequest.resume` from `agent_session_ref`.
- [ ] Feature 24: sign and bundle `sy-signal`, and check how macOS privacy prompts attribute hook processes.
- [ ] Feature 6: fold this spec's "never logged" list into the logging and redaction convention.
- [ ] `/sync`: add `sy-signal`, `sy-fake-agent`, the adapter purity rule and the helper binary lookup to `crates/AGENTS.md`.
