# @switchyard/protocol

## Overview

Generated TypeScript types and a typed JSON-RPC client for the engine protocol, with a pluggable `Transport`. The UI uses it with the Tauri channel transport in the app, and with the WebSocket transport to `sy-devbridge` in dev and Playwright runs.

## Stack

- TypeScript (strict), ESM, consumed as source (`exports: ./src/index.ts`, no build step)
- `json-schema-to-typescript` for generation

## Key files

| File | Owns |
|---|---|
| `src/generated/types.ts`, `src/generated/methods.ts` | generated from `crates/sy-protocol/protocol.schema.json`, never edited by hand |
| `src/client.ts` | the typed RPC client and the `Transport` interface |
| `src/websocket.ts` | dev and test transport to `sy-devbridge` |
| `src/base64.ts` | terminal byte encoding with a portable fallback |
| `scripts/generate.mjs` | the generator and its `--check` drift mode |

## Commands

```bash
pnpm protocol:gen                       # regenerate from the Rust schema
pnpm protocol:check                     # fails if generated files drift
pnpm --filter @switchyard/protocol typecheck
```

## Conventions

- The Rust structs in `sy-protocol` are the single source of truth. Change them, then regenerate; never hand edit `src/generated/`.
- Keep this package free of Tauri and React imports so any client (UI, tests, a future CLI) can use it.

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
