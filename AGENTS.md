# switchyard

Open source macOS desktop workspace for running many AI coding agents in parallel, each in its own git worktree.

## Stack

From [spec 0001](docs/specs/0001-stack-architecture/index.md): a Rust engine daemon (`switchyardd`) plus a Tauri 2 app with a React UI, talking JSON-RPC 2.0 over a user only Unix socket.
- **Language / Runtime**: Rust 1.98 (edition 2024, tokio), TypeScript (strict) on Node 22
- **Framework**: Tauri 2, React 19, Vite, TanStack Router, Zustand, @xterm/xterm
- **Key dependencies**: SQLite (rusqlite), portable-pty, system `git` CLI, OS keychain (`keyring`), `schemars` to TS codegen, `tracing`
- **Package manager**: pnpm 10 workspaces (`apps/*`, `packages/*`) + Cargo workspace (`crates/*`, `apps/desktop/src-tauri`)

## Build approach

Tracer Bullet: prove one real thread through every layer first, then thicken one segment at a time, end to end.

## Commands

```bash
pnpm install                          # install
pnpm dev                              # devtools engine + tauri dev
cargo build --workspace && pnpm build # build
cargo test --workspace && pnpm test   # test: Rust over the real socket, then Vitest
pnpm test:e2e                         # Playwright through sy-devbridge (first: playwright install webkit)
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
pnpm typecheck && pnpm lint && pnpm format:check
pnpm boundaries:check && pnpm protocol:check
cargo deny --all-features check       # licences, advisories, bans, sources (deny.toml)
```

## Specs

Stored in `docs/specs/`. Format: `docs/specs/NNNN-title/index.md`. Scope lives in `docs/scope/`.

## Rules

- Clean Architecture as ports and adapters: `sy-core` holds domain and use cases and defines ports as traits; `sy-infra`, adapters and clients sit outside and depend inward, never the reverse. The crate table in spec 0001 is the law.
- Use cases orchestrate; entities enforce their own invariants. Cross boundary data is plain DTOs (`sy-protocol` types), never core internals.
- Domain and use cases are unit tested without mocking infrastructure; infra is integration tested against real systems (real socket, real SQLite, the fake agent).
- Types strict: TS `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`, no `any` (use `unknown` and narrow), exhaustive switches. Rust: no `unsafe`, `unwrap`/`expect` denied outside tests.
- Errors: `thiserror` in library crates, `anyhow` only in binaries; protocol error codes defined once in `sy-protocol`.
- Documented public APIs: doc comments on every `pub` item in library crates (`missing_docs`), TSDoc on `@switchyard/protocol` exports.
- WCAG 2.2 AA and keyboard first for all UI; every UI string in the catalog.
- Never log secrets; spawn every process with an argument array, never a shell string.
- Testing gate: each feature ships unit plus integration tests in the spec's layers (Rust over the real socket, Vitest for UI units, Playwright through `sy-devbridge`) and isn't done until they pass.

## Tooling

Chosen here, installed by `/develop tooling`:
- Lint/format: rustfmt + clippy; ESLint (typescript-eslint strict type checked, jsx-a11y, react-hooks) + Prettier.
- Pre-commit (lefthook): format and lint staged files only. Typecheck, clippy and tests run in CI.
- CI: GitHub Actions on push and PRs, macOS runner: fmt, clippy, tsc, ESLint, tests, `boundaries:check`, `protocol:check`, cargo-deny (licences, advisories).

## Git

- integration: off (you manage git and GitHub yourself; skills never branch, commit, push or open PRs)

## Agent skills

- [rust-best-practices](.claude/skills/rust-best-practices/): `apollographql/skills`, idiomatic Rust
- [tokio-patterns](.claude/skills/tokio-patterns/): `geoffjay/claude-plugins`, tokio idioms, shutdown, timeouts
- [rust-error-handling](.claude/skills/rust-error-handling/): `po4yka/rust-skills`, thiserror and anyhow chains
- [rust-async-internals](.claude/skills/rust-async-internals/): `po4yka/rust-skills`, cancel safety, task ownership
- [rust-performance](.claude/skills/rust-performance/): `po4yka/rust-skills`, profiling and build tuning
- [rust-database](.claude/skills/rust-database/): `po4yka/rust-skills`, rusqlite, transactions, migrations
- [typescript-pro](.claude/skills/typescript-pro/): `jeffallan/claude-skills`, advanced TS types
- [pnpm](.claude/skills/pnpm/): `antfu/skills`, workspaces and catalogs
- [github-actions](.claude/skills/github-actions/): `callstackincubator/agent-skills`, CI workflow authoring

Declined: rusqlite-guide (daiki48), using-git-worktrees (obra/superpowers), git worktree toolbox MCP, Playwright MCP (microsoft), Axe MCP (Deque)
MCP servers: mcp-server-tauri (recommended, drive the app in `/check verify`), sqlite-mcp (recommended, inspect `switchyard.db`)

## Context files

- [crates/AGENTS.md](crates/AGENTS.md): engine crates, dependency rules, daemon gotchas
- [apps/desktop/AGENTS.md](apps/desktop/AGENTS.md): Tauri host and React UI, plus their skills
- [packages/protocol-ts/AGENTS.md](packages/protocol-ts/AGENTS.md): generated protocol types and typed client

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
