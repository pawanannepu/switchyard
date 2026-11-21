# 0001. Rationale: Tauri shell with a Rust engine daemon

The decision record behind [index.md](index.md). `/develop` builds from `index.md` and skips this file.

## Context

> ⚠️ Premise note: you chose to run the engine as a separate daemon (a background process that keeps running when the window closes) from day one, rather than designing for one and shipping it later. That is a sound call for this product, and the spec commits to it. Be clear about what it buys, though. Agents survive the **window** closing or the **UI** crashing, which is the common failure (a webview is the least stable layer). They do **not** survive the **engine itself** crashing, because the engine holds each agent's terminal, and a terminal whose owner dies hangs up its child. Making agents outlive an engine crash needs a small holder process per agent (the way tmux works). That belongs to feature 5 (process supervisor) and feature 23 (crash safe resume), not here. This spec keeps that door open by putting the terminal behind a trait.

switchyard is a desktop app that runs many AI coding agents at once. Each one is a heavy command line process (Claude Code, Codex, Gemini CLI, OpenCode) in its own git worktree and its own terminal, streaming output live. You will keep it open all day, next to a dozen of those agents, so the app's own memory, CPU and download size are a real feature, not a nicety. It ships for macOS first, but nothing in the core may assume macOS, because Linux and Windows follow (feature 35).

The forces that shape the foundation:

- **Real terminals at scale.** Every agent needs a genuine pseudo terminal (PTY, the kernel device that makes a program think it is talking to a terminal), and a dozen of them stream at once while the UI stays responsive.
- **Agents outlive the window.** Closing the window, or the UI crashing, must not kill running agents, and you still need to hear when an agent needs you.
- **Crash safety.** A hard kill of the app loses nothing already shown (feature 3), and a restart picks up where you left off (feature 23).
- **Modularity.** Adding an agent never touches the core (feature 4).
- **Security.** The app never reads agent login tokens. Secrets live in the OS secure store. Nothing is exposed beyond your machine.
- **Distribution.** A signed, notarized build (feature 24), and later safe signed updates (feature 34).

The team is one person, comfortable in TypeScript, Rust and Go, with no deadline. Whatever is chosen has to be operable by that one person on a bad day: few moving parts, boring tools, clear seams.

If this isn't decided, every later spec (data model, adapter contract, supervisor, logging, design system) would be designing against an unknown host, and the walking skeleton in feature 8 couldn't start.

## Options considered

These are full stacks compared side by side, not individual tools. The landscape facts are as of 2026-11-21, from the check that ran during the design conversation.

### Option 1: Tauri 2 shell, Rust engine daemon, React UI

The window is a Tauri 2 app (the system webview hosted by a small Rust process). A separate Rust daemon, the engine, owns agents, terminals, git, the database and secrets. The window talks to it over JSON-RPC on a local socket. The UI is React and TypeScript with xterm.js terminals. (basis: Tauri 2.12.7 is the current stable release, Nov 18 2026, with official shell, notification and updater plugins)

**Pros**:
- Lean. It uses the system webview, so you get tens of MB instead of hundreds, which fits your footprint priority.
- One back end language for both the shell and the engine. Protocol types are shared in one crate.
- Rust gives low overhead, predictable process and PTY control with no garbage collector pauses under a dozen streams.
- Tauri's official notification and signed updater plugins cover features 10, 24 and 34.

**Cons**:
- Rust is slower to write than TypeScript or Go, especially for UI adjacent glue.
- Three different webviews across OSes (WebKit on macOS, WebKitGTK on Linux, WebView2 on Windows) to test against later.
- The Rust PTY crate (portable-pty 0.9.0) hasn't released since Feb 2025. It's stable but quiet, so it carries some maintenance risk.
- Tauri's WebDriver testing doesn't work on macOS, so end to end tests of the packaged app need another route.

### Option 2: Electron, TypeScript everywhere, Node engine daemon

The window is Electron (bundled Chromium). The engine is a Node process using node-pty, and everything is TypeScript. (basis: Electron 42 is the newest major; node-pty 1.1.0 is current)

**Pros**:
- One language end to end, the biggest ecosystem, and the fastest to build UI and glue.
- One rendering engine on every OS, so there's no webview matrix.
- node-pty is actively maintained and battle tested in VS Code.

**Cons**:
- 150 MB+ installs and hundreds of MB of RAM for the shell, plus a resident Node runtime for the daemon. This works directly against your top non functional priority.
- Garbage collector pauses and single threaded event loop contention with a dozen streams need care.
- The closest open source precedent in this space (Crystal) was Electron and is reportedly deprecated. That isn't damning, but it's no signal either.

### Option 3: Tauri (or Wails) shell, Go engine daemon

A webview shell plus a Go daemon using creack/pty, with goroutines supervising each agent. Claude Squad shows Go works well for driving agents in worktrees.

**Pros**:
- Go is excellent at supervising many processes, and fast to write.
- creack/pty is mature and widely used.
- Small static binaries and low memory.

**Cons**:
- With Tauri you get two back end languages (Rust shell, Go engine), and protocol types are written twice or generated across both.
- Wails, the Go native shell, has a smaller ecosystem and a long running v3 alpha. Its signed update story is thinner than Tauri's.
- Windows PTY (ConPTY) support in the Go ecosystem is less settled than portable-pty's.

### Option 4: Native macOS app (SwiftUI) over a Rust core

A SwiftUI app for the best macOS feel, talking to a Rust engine. Linux and Windows would get their own UIs later.

**Pros**:
- The best native macOS look, accessibility and performance.
- Native terminal options (SwiftTerm, or libghostty now at 1.0).

**Cons**:
- Linux and Windows each need a second and third UI, which isn't realistic for a solo build. It contradicts the cross platform requirement.
- Swift isn't in your language set.

## Rationale

Footprint and process control decide this. The app sits open all day beside a dozen agents, each already a heavy process, and you rated the app's own footprint as mattering a lot. That rules Electron out as the default. It isn't bad technology, but it spends exactly the resource you said to protect. Among the lean options, Tauri plus a Rust engine wins on having one back end language: the shell, the engine and the protocol crate share types, one toolchain and one set of conventions. For a solo maintainer, that's the difference between one codebase and two. Go would supervise processes just as well and is faster to write, but pairing it with Tauri means two back end languages, and pairing it with Wails means a weaker shell. A native Swift UI gives up the cross platform requirement.

The daemon split follows from "agents outlive the window" plus your choice to build it now. Putting the engine in its own process makes the most fragile layer, the webview, disposable. The UI can crash, close or update without touching an agent, and the menu bar presence keeps notifications working while the window is closed. JSON-RPC over a local socket keeps the engine client agnostic from the start. The same protocol later serves a CLI and feature 33's agent managed workspaces, with no network port to secure (basis: Unix domain sockets restricted by file permissions and peer credentials). The cost is real and accepted: you now own protocol versioning (solved with a handshake plus drain then swap) and daemon lifecycle (solved with a bundled helper that the app starts and that exits when idle).

The remaining picks are the boring, proven choice in each layer:
- **SQLite in WAL mode** (write ahead logging) gives crash safe local storage with relational queries for history, cost and compare (basis: a relational database is the right default; SQLite WAL survives a process kill).
- **System git** gives full worktree parity and respects your hooks, signing and credentials, so switchyard never holds a forge token (basis: your scope's "stores no forge tokens" rule).
- **xterm.js with a replay buffer** is how VS Code does it (basis: @xterm/xterm 6.0.0 is current).
- **The OS keychain** meets "secrets in OS secure storage, never plain text".
- **React** has the strongest accessible primitives for your WCAG 2.2 AA and keyboard first goals.

Each of these sits behind a trait in the engine, so a later swap is local.

**Picks I made for you** (each with the runner up; feature 2 or the named feature may refine them):
- **Wire framing: newline delimited JSON** (one JSON-RPC message per line), easy to debug with a socket tool. Runner up: LSP style `Content-Length` headers.
- **JSON-RPC layer: a small in house implementation** on tokio, because subscriptions and replay are custom and the surface is small. Runner up: `jsonrpsee` with a custom transport.
- **Terminal bytes on the wire: base64 inside JSON notifications, batched about every 16 ms per run.** Simple, and one message type. Measured by the 12 stream check in `index.md`. Runner up: a second binary frame type on the same socket, if measurement shows it's needed.
- **Migrations: plain SQL files embedded in the binary, applied in order with `rusqlite_migration`** (it tracks the version in SQLite's `user_version`). Runner up: `refinery`.
- **SQLite build: rusqlite `bundled` feature**, so every install runs the same SQLite version, not whatever the OS ships.
- **Errors: `thiserror` in library crates, `anyhow` in the two binaries.** Runner up: `snafu`.
- **Logging: `tracing`**, the standard in the tokio ecosystem. Redaction and sinks are feature 6's job.
- **Login shell environment.** A macOS app opened from Finder gets a minimal `PATH`, so `claude` and the other agents would come up "not found". The engine resolves your login shell's environment once at startup (runs your `$SHELL` as a login shell and reads `env`), caches it, and uses it for every agent and script. This is what VS Code does. Runner up: asking you to configure `PATH` by hand, which is a poor first run.
- **Crate prefix `sy-`**, short and greppable. Runner up: `switchyard-`.

## References

**Project sources**
- Scope index and Foundations epic (`docs/scope/index.md`, `docs/scope/foundations.md`): product decisions, feature 1's "done when", the Tracer Bullet approach and the GA workflow.
- Scope epics for Workspaces, Control, Agents, Review and Platform: terminal, notification, secrets, sandbox, update and cross platform requirements that shaped each layer.
- Your answers in this design conversation (2026-11-21): languages (TypeScript, Rust, Go), footprint matters a lot, daemon from day one, and every layer pick.

**Practices & standards**
- Boring technology and a monolith first: one engine process, not services.
- Ports and adapters (hexagonal architecture): the core defines traits, and the edges implement them.
- A relational database as the default, with SQLite WAL for crash safety.
- Unix domain sockets with file permissions and peer credential checks, instead of a localhost TCP port.
- Never interpolate untrusted input into a shell: spawn processes with argument arrays.
- A protocol version handshake with additive minor versions (the same idea as LSP's capability negotiation).

**Landscape scan (2026-11-21, compact)**
- Tauri 2.12.7 is current (Nov 18 2026), with official shell, notification, updater and stronghold plugins. No Tauri 3 news found.
- Electron 42 is the newest major. The latest 3 stable lines are supported.
- node-pty 1.1.0 and @xterm/xterm 6.0.0 are current. portable-pty 0.9.0 has been quiet since Feb 2025.
- rusqlite 0.43.0 came out Nov 15 2026.
- libghostty-vt is reportedly 1.0 (Nov 2026), but only secondary sources were found, so this is unverified.
- Comparable apps: Crystal (Electron, reportedly deprecated) and Claude Squad (Go plus tmux). Conductor's stack isn't public.
- Agent Client Protocol: the official agents list includes Claude Code, Codex (through adapters), Gemini CLI and OpenCode. This is relevant to feature 4.
- Not verified: a keychain plugin for Tauri, the `keyring` crate's current version, the sqlx version, and Electron's current stable. Check the `keyring` version at scaffold time.

**Links** (web verified)
- Tauri release notes: https://v2.tauri.app/release/tauri/
- Electron timelines and support policy: https://www.electronjs.org/docs/latest/tutorial/electron-timelines
- node-pty on npm: https://registry.npmjs.org/node-pty/latest
- @xterm/xterm on npm: https://registry.npmjs.org/@xterm/xterm/latest
- portable-pty on crates.io: https://crates.io/api/v1/crates/portable-pty
- rusqlite on crates.io: https://crates.io/api/v1/crates/rusqlite
- git worktree manual: https://git-scm.com/docs/git-worktree
- Agent Client Protocol: https://agentclientprotocol.com/
- Apple, notarizing macOS software: https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution
- The Update Framework: https://theupdateframework.io/
