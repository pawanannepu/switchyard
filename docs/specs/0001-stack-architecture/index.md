# 0001. Tauri shell with a Rust engine daemon

**Date**: 2026-11-21
**Status**: Accepted

## Summary

switchyard is built as two processes. The first is the **engine**, a small Rust background program (a daemon) that runs the agents, their terminals, git and the local database. The second is the **app**, a lightweight Tauri window with a React UI that connects to the engine over a private local socket. Splitting them this way means closing or crashing the window never stops an agent, and the app stays lean enough to leave open all day. Every later feature builds inside this shape: agent specific code lives in adapters, operating system specific code lives in one infrastructure crate, and the core stays free of both.

## Decision

**Chosen option**: Option 1: Tauri 2 shell, Rust engine daemon, React UI.

switchyard is a Rust engine daemon (`switchyardd`), bundled inside a Tauri 2 desktop app whose React and TypeScript UI talks to the engine through the app's Rust host over JSON-RPC on a user only local socket.

**Implementation skills** (all under `.claude/skills/<skill>/`):
- **Tauri shell:** `tauri`, `tauri-ipc`, `tauri-security`, `tauri-build` (`full-stack-skills/tauri-skills`) · `tauri-v2` (`pedronauck/skills`)
- **Rust engine:** `rust-best-practices` (`apollographql/skills`) · `tokio-patterns` (`geoffjay/claude-plugins`) · `rust-error-handling`, `rust-async-internals`, `rust-performance`, `rust-database` (`po4yka/rust-skills`)
- **UI:** `vercel-react-best-practices` (`vercel-labs/agent-skills`) · `typescript-pro` (`jeffallan/claude-skills`) · `tanstack-router` (`tanstack-skills/tanstack-skills`) · `zustand` (`pedronauck/skills`)
- **Build tooling:** `vite`, `pnpm` (`antfu/skills`)
- **Release (feature 24):** `distributing-tauri-for-macos` (`dchuk/claude-code-tauri-skills`) · `macos-packaging-notarization` (`robinebers/skills`)

Where two skills overlap (`tauri` and `tauri-v2`), this spec wins on any conflict, then the more specific skill.

## Rationale

Reasoning, options and the landscape scan: see [rationale.md](rationale.md).

## Proposed stack

Versions are the current ones as of 2026-11-21. Pin the exact current patch at scaffold time.

| Layer | Choice | Reason |
|---|---|---|
| Architecture pattern | Two process modular monolith: one engine daemon plus thin clients | One deployable engine with clear internal seams, and a disposable UI. No services. |
| Engine language and runtime | Rust (edition 2024, stable toolchain pinned in `rust-toolchain.toml`), tokio | One back end language with the shell, and low overhead process and PTY control. Tauri already runs on tokio. |
| Desktop shell | Tauri 2 (2.12.x) | Uses the system webview, so it's lean, with official notification, tray and signed updater support. |
| UI | React 19, TypeScript (strict), Vite, pnpm | The strongest accessible primitives for WCAG 2.2 AA and keyboard first use. Vite is Tauri's default front end build. |
| UI state | Zustand stores fed by engine notifications | Narrow subscriptions, so one status tick redraws only one card. |
| UI routing | TanStack Router, memory history | Type safe deep links (`/workspace/$id`) for notification clicks and the command palette. |
| Terminal rendering | @xterm/xterm 6 with the WebGL addon | Proven (VS Code). Terminal bytes bypass React entirely. |
| Client to engine IPC | JSON-RPC 2.0 over a Unix domain socket (a named pipe on Windows later), one message per line | Only your user can reach it, it opens no network port, and any language can speak it, so the same protocol later serves a CLI and feature 33. |
| Protocol types | Rust structs → JSON Schema (`schemars`) → generated TS types | One source of truth. The schema doubles as the published protocol contract. |
| PTY | `portable-pty` 0.9, behind an internal `Pty` trait | Covers macOS, Linux and Windows ConPTY. The trait makes the quiet crate easy to swap. |
| Local database | SQLite (rusqlite 0.43, `bundled`), WAL mode, plain SQL migrations | Crash safe, relational, one file owned only by the engine. |
| Git | The system `git` CLI (2.39 or newer), argument arrays, machine readable output | Full worktree and merge parity. It respects your hooks, signing and credentials, and switchyard holds no forge token. |
| Secrets | The OS keychain via the `keyring` crate (check the version at scaffold) | Meets "OS secure storage, never plain text". Only the engine reads it. |
| Notifications and menu bar | `tauri-plugin-notification`, Tauri tray icon | Posted from the app's Rust host, so they work with the window closed. |
| Logging | `tracing` | The tokio ecosystem standard. Redaction, sinks and the diagnostics bundle belong to feature 6. |
| Errors | `thiserror` in libraries, `anyhow` in binaries | Typed errors at the seams, ergonomic at the top. |
| Testing | Rust tests against the real socket with the fake agent. Vitest for UI units. Playwright against the Vite build through a dev only bridge | Tauri's WebDriver doesn't work on macOS, so test in layers. Feature 2 installs the tooling. |
| Platform target | macOS 13 or newer, universal binary (Apple silicon and Intel), bundle id `dev.switchyard.app` | Covers almost every developer Mac. The bundle id anchors Keychain items, notification permission and the data folder. |

Deferred to their own specs: styling and components (feature 7), lint, format and CI (feature 2), the schema (feature 3), the adapter contract (feature 4), supervision limits (feature 5), log redaction (feature 6), signing and the updater (features 24 and 34).

## Architecture

### Processes

```
┌──────────────── switchyard.app (Tauri) ───────────────┐
│  React UI (webview)                                   │
│    └─ Transport: Tauri channel ──┐                    │
│  App host (Rust): relay · tray · notifications ·      │
│    engine launcher · deep links  │                    │
└──────────────────────────────────┼────────────────────┘
                                   │ JSON-RPC, one message per line
                     Unix socket (0600, same uid only)
                                   │
┌──────────────── switchyardd (engine daemon) ──────────┐
│  RPC server → sy-core (orchestration, state)          │
│    ports: Store · Git · Pty · Secrets · Env · Clock   │
│    adapters: claude · (codex · gemini · opencode ·    │
│              generic · fake)                          │
│  sy-infra: SQLite · git CLI · portable-pty · keychain │
└──────┬───────────────┬───────────────┬────────────────┘
       PTY             PTY             PTY
   agent (worktree A)  agent (B)  …    agent (N)
```

- **The engine** (`switchyardd`) is the only owner of agents, PTYs, the database, git operations and secrets. It runs as one instance per user.
- **The app host** (Tauri's Rust side) is a thin client. It has no business logic. It owns the window, the menu bar item and OS notifications, and starts the engine when it isn't running. It holds **one** engine connection and does that connection's `session.hello` itself. It relays JSON-RPC frames between the webview and the engine unchanged **except for the `id`**: the host rewrites each webview request id to its own connection id and maps the response back, and it forwards notifications as they are. When the webview reloads, every in flight request gets a JSON-RPC error, and the fresh UI attaches again (replay below). The engine connection is never dropped because of a reload.
- **The UI** (webview) only renders and sends requests. It never touches the file system, processes or the network.

### Workspace layout and dependency rules

```
Cargo.toml                 # Cargo workspace
package.json, pnpm-workspace.yaml
crates/
  sy-protocol/             # JSON-RPC wire types + JSON Schema generation
  sy-client/               # data dir + socket path resolver, socket transport, handshake (used by every client and the engine)
  sy-adapter-api/          # AgentAdapter trait + adapter facing types (shape decided in spec for feature 4)
  sy-core/                 # domain + orchestration; defines the ports (traits)
  sy-infra/                # implements the ports: SQLite, git CLI, PTY, keychain, OS paths, login env
  sy-adapters/claude/      # first adapter (feature 8); codex, gemini, opencode, generic, fake follow
  sy-daemon/               # binary `switchyardd`: composition root + socket server
apps/desktop/
  src-tauri/               # app host: relay, tray, notifications, engine launcher
  src/                     # React UI
packages/protocol-ts/      # generated TS types + typed RPC client with a pluggable Transport
```

These rules keep "a new agent never touches the core" and "nothing in the core tied to one OS" true. Feature 2 enforces them in CI.

| Crate | May depend on | Must never depend on |
|---|---|---|
| `sy-protocol` | serde, schemars | anything else in the workspace |
| `sy-client` | `sy-protocol`, tokio, serde_json, `dirs` (or similar for OS paths) | `sy-core`, `sy-infra`, any adapter, tauri |
| `sy-adapter-api` | serde, serde_json, thiserror, vte (changed by [spec 0003](../0003-agent-adapter-contract-fake-agent/index.md): the contract is pure and synchronous) | `sy-core`, `sy-infra`, any adapter |
| `sy-core` | `sy-adapter-api`, serde, tokio, thiserror, tracing | `sy-infra`, any adapter, tauri, rusqlite, portable-pty, keyring. No `cfg(target_os)` code. |
| `sy-infra` | `sy-core` (to implement its ports), rusqlite, portable-pty, keyring, sysinfo (spec 0004) | `sy-daemon`, tauri, any adapter |
| `sy-adapters/*` | `sy-adapter-api` only (plus general utility crates) | `sy-core`, `sy-infra`, any other adapter |
| `sy-daemon` | everything above | tauri |
| `apps/desktop/src-tauri` | `sy-protocol`, `sy-client`, tauri and its plugins | `sy-core`, `sy-infra`, `sy-daemon`, adapters |
| `sy-devbridge` (dev and test only) | `sy-protocol`, `sy-client` | everything else |
| `sy-signal` (hook helper, bundled beside `switchyardd`, spec 0003) | `sy-protocol`, `sy-client` | everything else |
| `sy-fake-agent` (dev and test only, spec 0003) | no workspace crate | every workspace crate |
| UI (`apps/desktop/src`) | `packages/protocol-ts`, React and the UI libraries | Tauri's file system, shell or HTTP plugins |

Operating system specific code (`cfg(target_os)`) lives only in `sy-infra`, `sy-client` (path and socket details only) and `src-tauri`. There is exactly one data dir and socket path resolver, in `sy-client`. The engine, the app host and the dev bridge all call it, so they can never disagree about where the socket is. Each adapter registers itself with the daemon at startup, so adding one touches `sy-adapters/<new>/` plus one registration line in `sy-daemon`, and nothing in `sy-core`.

### Engine protocol (the contract every client speaks)

- **Transport.** A Unix domain socket at `<data dir>/engine.sock`. The data dir is `0700` and the socket `0600`. The engine checks the peer's user id on every connection (`getpeereid`) and rejects anything else. If the path would exceed the macOS socket path limit (104 bytes), it falls back to `/tmp/switchyard-<uid>/engine.sock`, created `0700`. On Windows this becomes a named pipe behind the same transport trait. `sy-client` resolves the data dir: `SWITCHYARD_DATA_DIR` when set (tests, running the engine by hand), otherwise the OS default (see Local data). Tests always set a short `/tmp/sy-<random>` data dir so they never hit the path limit.
- **Framing.** JSON-RPC 2.0, one UTF-8 message per line (newline delimited JSON). The maximum line is 1 MiB. A longer line closes that connection with an error logged, and the engine and its other clients carry on.
- **Handshake.** The first call on every connection is `session.hello { client: "desktop" | "cli" | "test", client_version, protocol: { major, minor } }`. The engine replies `{ engine_version, pid, protocol: { major, minor } }`. They're compatible when the majors match. Minors only add things (new methods, new optional fields). After an incompatible `hello`, the connection stays open, but only the stable methods below work.
- **Stable methods** (frozen across every protocol version, legal before `hello` and after an incompatible one): `session.hello`, `engine.info` (returns `engine_version`, `pid`, `protocol`, running agent count), `engine.drain` (exit as soon as no agent is running, whatever clients are connected) and `engine.shutdown` (stop agents per *Shutdown*, then exit). Everything else needs a compatible `hello`.
- **Naming.** Methods are `<area>.<verb>` (`project.add`, `task.start`, `run.attach`). Engine to client events are JSON-RPC notifications named `<area>.<event>` (`run.output`, `run.status`, `approval.requested`).
- **Terminal streams.** `run.attach { run_id }` returns the replay of the run's ring buffer (the latest output, up to 2 MiB per run) plus `end_offset`, the run's total byte count at the end of that replay. Live `run.output { run_id, offset, data_b64 }` notifications follow, batched about every 16 ms per run. `offset` is the byte position of the chunk's first byte in that run's whole output, counted per run (not per connection), so the client drops any byte below `end_offset` and never shows a gap or a duplicate. Input goes through `run.input { run_id, data_b64 }`, capped at 64 KiB per message (a bigger paste is split by the client), and resizes through `run.resize`. The full output also goes to an append only log on disk (its layout and retention are feature 3's).
- **Backpressure.** Each client has an outgoing queue capped at 8 MiB (bytes, not messages). If a client falls behind, the engine drops that client's live stream for the affected run and sends `run.resync_needed { run_id }`, and the client attaches again. Resyncs are throttled to one every 2 seconds per run per client. A client that stays too slow sees a "stream paused, resume" state instead of looping. A slow window never slows or blocks an agent.
- **Errors.** Standard JSON-RPC error codes, plus app codes in the reserved server range (`-32000` to `-32099`), defined once as an enum in `sy-protocol` and blocked out by area (session, engine, project, task, run and so on).
- **Schema.** `sy-protocol` emits `protocol.schema.json`. The TS types in `packages/protocol-ts` are generated from it, and CI fails if the generated files drift from the source.

### Engine lifecycle

- **Packaging and identity.** `switchyardd` ships as a Tauri `externalBin` in `Contents/MacOS`. It's built for both architectures and merged into one universal binary, and signed with the same Team ID and hardened runtime as the app, so Gatekeeper and Keychain treat them as one product. The engine never posts notifications and never shows Keychain UI. Only the app host talks to the user.
- **Start.** On launch the app host tries the socket. If nothing answers, it spawns the bundled `switchyardd` detached from itself (in a new session, so it outlives the app), with stdout and stderr sent to `engine.stderr.log` to catch anything printed before logging starts. It then retries the connection with backoff for up to 5 seconds until `hello` succeeds. It ignores the spawned child's exit code, so if another launch's engine won the lock, the client simply connects to that one. Spawning is safe to repeat. A failure after 5 seconds shows a clear "engine didn't start" state linking to both logs.
- **Single instance, lock and socket.** The engine takes `flock(LOCK_EX | LOCK_NB)` on `<data dir>/engine.lock`. The kernel releases it when the process dies, so there's no PID file and nothing goes stale. Only the process holding the lock may remove and recreate `engine.sock`. That clears a stale socket left by a crash. A copy that fails to get the lock logs "already running" and exits with code 0 **without touching the socket**. Whoever takes the lock wins, and that includes during a version swap.
- **Window closed.** The app keeps a menu bar item showing running and attention counts. It keeps its engine connection, posts OS notifications from the app host, and reopens the window in one click. **Quit** from the menu bar asks first when agents are running.
- **Idle exit.** The engine exits by itself when no agent is running and no client has been connected for 5 minutes. While the app runs, its connection keeps the engine alive. That's intended, and draining (below) is the only way an engine exits while a client is connected.
- **Version mismatch (drain then swap).** After an update, if the running engine's protocol major differs from the app's, the app reads `engine.info` and shows "N agents are running on the previous engine". You choose **Restart now**, which calls `engine.shutdown` after a confirmation (feature 23 then resumes the agents), or **When they finish**, which calls `engine.drain`. The old engine exits once its last agent stops, and the app's retry loop then starts the new engine. A compatible older engine is used normally until it goes idle.
- **Shutdown.** On `engine.shutdown`, SIGTERM (for example at logout) or idle exit, the engine stops accepting connections. It sends SIGHUP to each agent's process group, waits up to 5 seconds, then SIGKILLs whatever remains. (Amended by spec 0003, AC-12: each agent's adapter first gets a graceful stop step capped at 3 seconds during shutdown, so shutdown takes at most about 8 seconds. Amended again by spec 0004, AC-12: shutdown then waits for each run's leftover sweep, so it takes at most about 13 seconds.) It flushes logs and the database, removes the socket, and exits. Feature 5 may refine the timeouts.
- **Engine crash.** The app detects the dropped connection, shows "engine stopped", and offers to restart it. Agents owned by the crashed engine are lost or orphaned. Finding and cleaning those up is feature 23's job (see Consequences).

### Local data

| What | Where (macOS) |
|---|---|
| Database | `~/Library/Application Support/dev.switchyard.app/switchyard.db` (WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout` set) |
| Run output logs | `~/Library/Application Support/dev.switchyard.app/runs/` (layout from feature 3) |
| Socket, lock | `~/Library/Application Support/dev.switchyard.app/engine.sock`, `engine.lock` |
| App and engine logs | `~/Library/Logs/dev.switchyard.app/`: `engine.log` (written by the engine itself), `engine.stderr.log` (early output caught by the launcher), `app.log` (format and rotation from feature 6) |
| Secrets | macOS Keychain, service `dev.switchyard.app`. The database stores only a reference to each secret, never the value. |

Paths are resolved by `sy-infra` per OS, never hardcoded in the core. The database has exactly one owner, the engine. The scaffold uses one connection on a dedicated thread for every query. WAL permits adding read only connections later, but only after measurement shows a need. Clients never open the database.

### Security boundaries

- **The webview is untrusted.** Tauri capabilities grant the UI only the relay commands, with no file system, shell, HTTP or process plugins. The content security policy allows only bundled assets, with no remote content.
- **Agent output is never HTML.** Terminal bytes go to xterm.js. Every other agent supplied text (file names, diffs, messages) is rendered as text, never injected as markup.
- **No shell strings.** Every child process (agents, git, scripts) is spawned with an argument array, never by building a shell command from input. Git runs with `GIT_TERMINAL_PROMPT=0` and `LC_ALL=C` for stable output.
- **Login environment.** At each engine start, and again on `env.refresh`, the engine runs `$SHELL -l -i -c '<print a start marker>; env -0; <print an end marker>'`. It runs with no stdin, so a prompt in your shell startup files can't hang it, and with a 5 second timeout. It reads only the bytes between the markers, ignoring any noise your startup files print, and caches the result for launching agents and scripts. That way agents installed through Homebrew, npm or other managers are found when the app was opened from Finder. If resolution fails or times out, the engine falls back to `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin` and reports a warning, which the UI shows.
- **The git shim.** Without the Xcode Command Line Tools installed, `/usr/bin/git` is a shim that pops an install dialog. If the resolved `git` is `/usr/bin/git`, the engine first checks `xcode-select -p` (with a timeout). If that fails, it reports "git missing" for onboarding (feature 19) and never runs the shim.
- **macOS privacy permissions.** Agents work in your repos, which often sit in `~/Documents` or `~/Desktop`. The permission prompt may name the app or the engine, depending on how macOS attributes the process that launched it. The scaffold doesn't settle this. Feature 24 checks it in the signed build and decides whether the engine needs to be the launcher of record (for example by registering it as a LaunchAgent).
- **No tokens.** No crate reads agent login files or forge credentials. Detection asks each CLI (feature 4, feature 19).
- **Local only.** There's no TCP listener in any shipped build. The Playwright bridge exists only in dev and test builds (below).

### Testing seams

- **Engine.** Rust integration tests start `switchyardd` with `SWITCHYARD_DATA_DIR=/tmp/sy-<random>` and drive it over the real socket, with the fake agent (feature 4).
- **Secrets in tests.** Tests use a file backed secret store behind the `Secrets` port, never the real Keychain. CI never touches a keychain. Local dev builds are signed with one stable development certificate, so Keychain items don't prompt again after every rebuild.
- **Dev only engine methods.** A `devtools` Cargo feature on `sy-daemon` adds `dev.*` methods, starting with `dev.synthetic_run { count, bytes_per_sec, echo }`, which creates fake runs that stream output and, with `echo`, write `run.input` straight back as output. Release builds are compiled without the feature, and a release check fails if any `dev.*` method answers.
- **UI.** `packages/protocol-ts` takes a `Transport`. The shipped app uses the Tauri channel transport. Dev and test builds can use a WebSocket transport to `sy-devbridge`, a dev only binary that forwards a random localhost port, protected by a random token, to the engine socket. At startup it prints one JSON line, `{"port": …, "token": …}`, which the Playwright setup reads. It's excluded from release bundles. Playwright drives the Vite build through it, and Vitest covers units.
- **Packaged app.** A short manual or scripted smoke check per release (feature 24). Notifications and the menu bar are verified only in the built, signed `.app`, never in `tauri dev`, because macOS attributes an unsigned dev build's notifications to the terminal.

## What the scaffold must prove

Derived from feature 1's "done when" in the scope. `/develop stack & architecture` builds the scaffold, and `/check verify` checks these.

1. `pnpm tauri build` produces a universal macOS 13+ `.app` with the bundle id `dev.switchyard.app`. `Contents/MacOS/switchyardd` is a universal binary (`lipo -archs` lists both `arm64` and `x86_64`), signed with hardened runtime and the same Team ID as the app. `cargo build --workspace` and `pnpm build` pass clean.
2. Opening the app shows a window. The app host starts `switchyardd`, `session.hello` succeeds, and the window shows `engine_version` and `protocol` from the `hello` reply.
3. Closing the window leaves the menu bar item and the engine running. Reopening from the menu bar reconnects to the same engine: the `pid` from `engine.info` is unchanged. This is checked in the built `.app`.
4. Running a second `switchyardd` by hand logs "already running" and exits with code 0. The socket is untouched and the app keeps working. Launching the app twice in a row ends with exactly one engine.
5. After `kill -9` of the engine, the app shows "engine stopped". Restarting it takes over the lock, replaces the stale socket and reconnects.
6. A load check (a `devtools` build): `dev.synthetic_run { count: 12, bytes_per_sec: 51200, echo: true }`. The UI renders all 12 runs in xterm.js over the Tauri channel, not the dev bridge. On an Apple silicon Mac, the p95 time from sending `run.input` to the echoed bytes being drawn (`performance.now()` at send, then xterm's write callback) is under 100 ms. The engine's per run producer is never blocked by a slow client. Feature 4's fake agent replaces the synthetic run once it lands (spec 0003: `dev.agent_run` with a `flood` script).
7. The dependency rules above hold: `sy-core` builds with no `cfg(target_os)` and none of the forbidden dependencies, adapters depend only on `sy-adapter-api`, and `src-tauri` depends only on `sy-protocol` and `sy-client`.
8. The generated TS protocol types match `protocol.schema.json`, so a drift check fails if they don't.
9. The peer check function rejects a user id that differs from the engine's (a unit test, since a second macOS user isn't available in CI). A line over 1 MiB closes only that connection.
10. A release build answers no `dev.*` method and contains no `sy-devbridge`.

## Consequences

**Positive**:
- Agents survive the window closing and UI crashes. Notifications work with the window closed.
- A lean footprint: a system webview plus one small Rust daemon, with no bundled browser or JS runtime in the background.
- One back end language and one protocol crate. TS types are generated, never hand synced.
- A client agnostic engine: a CLI, feature 33's agent interface, or a future remote bridge is just another JSON-RPC client.
- Hard seams. Adapters can't reach the core's internals, and OS code is confined to `sy-infra` and `src-tauri`.

**Negative / tradeoffs**:
- **An engine crash kills its agents.** The engine owns their PTYs, so a terminal whose owner dies hangs up its child. Agents survive UI failures, not engine failures. Real survival across engine crashes needs a small holder process per agent (the way tmux works). That's for features 5 and 23 to decide, and the `Pty` trait keeps it a local change.
- You own a protocol: versioning, a schema, a drift check and a handshake. That's ongoing work a single process app wouldn't have.
- Two processes to debug, plus engine lifecycle edge cases (stale socket, stale lock, a version mismatch after an update).
- Base64 terminal bytes inside JSON cost about 33% overhead. That's fine at expected rates, and the scaffold's 12 stream check measures it.
- Three webviews to test later (WebKit, WebKitGTK, WebView2), and no WebDriver for the packaged macOS app.
- The system git dependency: git 2.39 or newer must be installed (Xcode Command Line Tools provides it). Onboarding (feature 19) detects it and explains.
- portable-pty is quiet upstream. If it stalls, a replacement goes behind the `Pty` trait.
- macOS privacy prompts (Documents, Desktop) may name the app or the engine unpredictably until feature 24 settles which process is the launcher of record.
- Resolving your login shell at each engine start adds up to 5 seconds in the worst case (a slow shell startup), and your startup files run once per engine start.

**Neutral**:
- Rust edition 2024 and a pinned toolchain. Node LTS and pnpm pinned through `packageManager`.
- The bundle id `dev.switchyard.app` is effectively permanent: Keychain items, notification permission and the data folder hang off it.
- `synchronous=NORMAL` with WAL survives an app or process kill with nothing lost. A power cut or OS crash can lose the last fraction of a second of writes, which is accepted.

## Follow-up

- [ ] Feature 3 (data model): run output log layout and retention on disk, and the schema.
- [x] Feature 4 (adapter contract): define `sy-adapter-api`, and weigh the Agent Client Protocol (all four target agents are listed as supporting it) as the primary transport beside PTY. Settled in spec 0003: PTY plus a hook side channel, with ACP left as a possible later driver.
- [x] Feature 5 (supervisor): decide whether a per agent holder process is needed so agents outlive an engine crash. Set time and output limits. Settled in spec 0004: no holder (the engine keeps owning PTYs, and feature 23 resumes through each agent's session), plus idle, active time and output limits. Spec 0004 also switches the release profile to `panic = "unwind"` so a panic fails only one run.
- [ ] Feature 2 (tooling): enforce the dependency table in CI (for example with `cargo-deny` bans or a `cargo metadata` check), the schema drift check, lint, format and type checks, and the Playwright and Vitest setup.
- [ ] Feature 7 (design system): styling approach and the accessible component primitives.
- [ ] Features 8 and 11: where worktrees live on disk, and how the sandbox confines them.
- [ ] Features 24 and 34: sign and notarize the bundle with `switchyardd` inside it, keep `sy-devbridge` out of release builds, and set up the updater signing keys. Check which process macOS privacy prompts name when an agent touches `~/Documents`, and decide whether the engine should become a LaunchAgent (registered through SMAppService) so it is the launcher of record.
- [ ] There's no `AGENTS.md` yet, so none of the 19 installed skills are recorded in one. When `/audit` creates root `AGENTS.md` (feature 2), its `## Agent skills` section should list them (they're all project wide), plus a `Declined:` line for the ones offered and not taken: `xterm-js` (ssv445), `rusqlite-guide` (daiki48), `using-git-worktrees` (obra/superpowers), `signing-tauri-apps` (dchuk).
- [ ] Optional MCP servers you chose to consider (unconfirmed, so check each repo before connecting it with `claude mcp add`): a Tauri MCP (`https://github.com/hypothesi/mcp-server-tauri`) to drive the running app during `/check verify`, which matters because WebDriver doesn't work on macOS, and a SQLite MCP (`https://github.com/stackloklabs/sqlite-mcp`) to inspect `switchyard.db` while debugging. Declined: git worktree toolbox.
- [ ] Choose the open source licence before the first public commit, and add a dependency licence check (feature 2 or 25).
- [ ] Register `switchyard.dev` (or the domain behind `dev.switchyard.app`) if you can, so the bundle id matches a domain you control.
