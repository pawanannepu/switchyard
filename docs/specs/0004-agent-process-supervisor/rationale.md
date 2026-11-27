# 0004. Rationale: agent process supervisor

Decision record for [index.md](index.md). `/develop` doesn't need this file.

## Context

Feature 4 (spec 0003) built a minimal real launcher. Each agent runs its real TUI in a PTY that the engine (`switchyardd`) owns, in a new session, so `pgid = pid`. A per run driver task owns the decoder and approvals. A reader thread feeds the hub, and a waiter thread observes the exit with `waitid(WNOWAIT)` and reaps at once, under the same lock that guards group signals. `run.stop` and shutdown type the adapter's stop input, wait its grace, send SIGHUP to the group, wait 5 s, and send SIGKILL, but only until the main process is reaped. Several things were deliberately left to feature 5: `run.pid_started_at` stays NULL, there are no limits, a grandchild that outlives the agent keeps running, and spec 0001's question of whether agents should survive an engine crash through a holder process per agent is open. Spec 0002 already reserved `end_reason = "limit_exceeded"` and the `setting` / `project_setting` tables.

The scope's done when line for feature 5: each agent runs in its own process with a terminal; a crash, hang or runaway (time or output limits) is detected and that agent is stopped cleanly; the app and other agents keep running; tests prove it with the fake agent.

The forces:
- **Many agents, unattended.** The product's point is running many agents in parallel without watching each one. A hung or looping agent that nobody notices burns hours and tokens, and one that floods its terminal fills the 5 GiB log budget (spec 0002), whose trimming only touches archived tasks.
- **Agents start other processes.** Coding agents routinely start dev servers, watchers, test runners and package managers, some with `nohup` or in their own process group. Today those survive the run, and they survive an engine crash too.
- **pid reuse.** macOS recycles pids. Signalling a stored pid or group id after its process is gone can hit an unrelated process of yours. Any cleanup has to prove identity, not assume it.
- **Blast radius.** The engine is one process owning every agent. A bug in one run's handling (a panic in its driver) must not take the engine down, and the release profile currently aborts on panic.
- **Cross platform core.** Nothing in `sy-core` may be tied to macOS (spec 0001). Process inspection needs an OS layer behind a port. `unsafe` is banned in our code, so raw `libc` calls are out.
- **Solo build, Tracer Bullet.** Whatever is chosen has to be buildable and testable by one developer with the fake agent, in thin end to end slices.

Without a decision, every later feature (core task loop, notifications, crash resume, cost tracking) inherits agents that can run forever, leak processes, and take each other down.

## Options considered

### Option 1: Supervise in place (engine owned PTYs, limits, tracked cleanup)

Keep the engine as the PTY owner. Add limits checked on the driver's existing tick, a shared process tree scanner behind a new `ProcessTable` port (`sysinfo` in `sy-infra`), a `run_process` table so cleanup survives a crash, a sweep at every run end, and a per run monitor that catches panics.

**Pros**:
- Builds directly on 0003's driver, tick, stop sequence and ports. No new process, protocol or IPC.
- Cleanup is exact (pid plus start time), and works after a crash through the persisted rows.
- Fault isolation comes from task and thread boundaries the engine already has.

**Cons**:
- An engine crash still kills every agent. Survival relies on each agent's own session resume (feature 23).
- Polling the process tree costs a little CPU and misses very short lived escapees.
- It needs the release profile changed to `panic = "unwind"`.

### Option 2: A holder process per agent

A tiny `sy-holder` binary per run owns the PTY and the agent, like tmux. The engine talks to holders over a local socket and reattaches after a restart. Holders could also do their own process group cleanup and limits.

**Pros**:
- Agents truly survive an engine crash or upgrade. Reattach is lossless if the holder buffers output.
- A natural place for per run isolation: a holder crash affects one agent.

**Cons**:
- A second protocol, a holder lifecycle, holder discovery at start, and output buffering while the engine is down, which conflicts with 0002's persist before broadcast and single writer log.
- Spec 0002's "every live run is `lost` at restart" turns into a lost versus reattachable state, touching the data model and every client.
- Several times the build and test effort for a failure (an engine crash) that a tested Rust daemon should rarely hit, while agents can already resume their own sessions.

### Option 3: OS supervisors (launchd jobs or process sandboxes per run)

Run each agent as its own launchd job (or, later, systemd units on Linux and Job Objects on Windows), and use the OS for limits, cleanup and restart.

**Pros**:
- The OS reliably reaps and kills whole jobs, escapees included (a Windows Job Object or a Linux cgroup is exact).
- Agents could outlive the engine without a custom holder.

**Cons**:
- A different mechanism per OS. macOS launchd has no cgroup like containment, and per job plists for ad hoc runs are awkward. It pushes OS specifics deep into what should be core behaviour.
- The engine still needs the PTY. A launchd job doesn't give one, so it would need a holder anyway.
- Hard to drive deterministically from tests.

### Option 4: Process group only, no persistence

Option 1 minus the scanner and the table: at run end, signal the agent's process group (holding the zombie so the id stays reserved), fill `pid_started_at`, and at restart signal only the old main pid if its start time matches.

**Pros**:
- Much less to build: no scanner, no migration, no new dependency beyond reading a start time.
- It catches the common case (children that stay in the group).

**Cons**:
- Misses everything that left the group (`setsid`, double fork, a tool that makes its own process group), which is exactly how dev servers and daemons tend to start.
- After a crash, group members can't be safely identified, so restart cleanup reaches only the main process.

## Rationale

Option 1 meets the done when line in full with the least new machinery. Each piece maps to a force. Limits on the existing 250 ms tick answer "many agents, unattended". Using the lossless reader path for the idle and output clocks means the decoder lane's deliberate drops (0003 AC-5) can't hide activity or output. Process tracking plus a persisted (`pid`, start time) answer "agents start other processes" and "pid reuse" together: identity is proven before every signal, and holding the main process as an unreaped zombie until its sweep makes the group signal provably safe without any OS specific group lookup. The per run monitor and `catch_unwind` around thread bodies answer the blast radius force with boundaries the engine already has. Switching release builds to `unwind` is the price, small for a daemon whose speed is dominated by I/O.

The holder (Option 2) is the one real alternative, and it buys only one thing Option 1 lacks: survival across engine crashes. That is costly: a second protocol, buffered output that breaks the single writer log, and a new lost versus reattachable state rippling through the data model and UI. For a solo build it's the wrong trade while crashes should be rare and every first class agent can resume its own session (feature 23 builds on `agent_session_ref`). `ChildProcess` and `Pty` stay ports, so a holder can still be added later behind them if crashes prove common. Option 3 fails on macOS, which has no containment primitive comparable to cgroups or Job Objects and still needs a PTY owner. Option 4 is the honest minimum, but it fails the common case it exists for, since dev servers and daemons often leave the group.

The engineer's picks follow the same line: stop on limit (not warn only) to satisfy "stopped cleanly", generous defaults (30 minutes idle, 4 hours active, 512 MiB) so only genuine runaways trip, limits paused while waiting on you, project overrides for large repos, and no memory or CPU limits until a real runaway shows the need, because macOS can't enforce them by rlimit and sampling them is its own design. `sysinfo` was picked over `libproc` for its Linux and Windows coverage, matching the later platform plan. The registry search found no skill for it, and the two loosely matching candidates were declined.
