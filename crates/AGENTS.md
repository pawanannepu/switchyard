# Engine crates

## Overview

The Rust side of switchyard outside the app host: the protocol, the client library, the domain core, the OS layer, agent adapters and the `switchyardd` daemon that composes them. These crates follow the dependency table in [spec 0001](../docs/specs/0001-stack-architecture/index.md#workspace-layout-and-dependency-rules), which is what keeps "a new agent never touches the core" and "nothing in the core tied to one OS" true.

## Key files

| File | Owns |
|---|---|
| `sy-protocol/src/` | JSON-RPC wire types, method catalog, app error codes; `bin/gen-schema.rs` writes `protocol.schema.json` |
| `sy-client/src/` | the one data dir and socket path resolver, socket transport, handshake |
| `sy-adapter-api/src/lib.rs` | the `AgentAdapter` contract (placeholder until feature 4) |
| `sy-core/src/` | domain and orchestration; defines the ports (traits) |
| `sy-infra/src/` | implements the ports: SQLite, git CLI, PTY, keychain, OS paths, login env |
| `sy-adapters/<agent>/` | one crate per agent adapter (`claude` first) |
| `sy-daemon/src/` | `switchyardd`: composition root, socket server, lock, lifecycle, outbox |
| `sy-daemon/tests/engine.rs` | integration tests against the real socket |
| `sy-devbridge/src/main.rs` | dev and test only WebSocket to socket bridge for Playwright |

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo build -p sy-daemon --features devtools   # adds dev.* methods
pnpm boundaries:check                          # enforces the dependency table
```

## Conventions

- Dependency direction is the spec table, checked by `scripts/check-boundaries.mjs`. `sy-core` never depends on `sy-infra`, an adapter, tauri, rusqlite, portable-pty or keyring.
- `cfg(target_os)` / `cfg(unix|windows)` only in `sy-infra`, `sy-client` (paths and socket details) and `src-tauri`.
- New ports are traits in `sy-core`; their real implementations go in `sy-infra`, test doubles beside the tests.
- Adding an agent touches only `sy-adapters/<new>/` plus one registration line in `sy-daemon`.
- Errors: `thiserror` enums in library crates, `anyhow` only in binaries (`sy-daemon`, `sy-devbridge`). Protocol error codes live once in `sy-protocol`.
- Every child process is spawned with an argument array, never a shell string. Git runs with `GIT_TERMINAL_PROMPT=0` and `LC_ALL=C`.
- Protocol changes start in `sy-protocol`; then run `pnpm protocol:gen` so the TS types follow.

## Gotchas

- Tests set `SWITCHYARD_DATA_DIR=/tmp/sy-<random>`; a long path exceeds the 104 byte macOS socket limit.
- Only the process holding `engine.lock` may remove or recreate `engine.sock`. A second engine exits 0 without touching it.
- A slow client must never block a run's producer; stream chunks drop and `run.resync_needed` goes out instead.
- `dev.*` methods exist only behind the `devtools` feature; `pnpm release:check` fails if a release build answers one.
- Tests use a file backed secret store, never the real Keychain.
- Doc comments on `sy-protocol` types become schema descriptions and TSDoc in the generated TS, so editing one needs `pnpm protocol:gen` too, or `pnpm protocol:check` fails.

## Related specs

- [0001 Stack & architecture](../docs/specs/0001-stack-architecture/index.md)

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
