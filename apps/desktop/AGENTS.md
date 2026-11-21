# @switchyard/desktop

## Overview

The switchyard app: a Tauri 2 window whose Rust host (`src-tauri/`) owns the window, the menu bar item, notifications and the engine launcher, and relays JSON-RPC frames to `switchyardd`. The React UI (`src/`) only renders and sends requests. Closing or crashing either one never stops an agent.

## Stack

- **Host**: Rust, Tauri 2.12 (`tray-icon`), `tauri-plugin-dialog`; depends only on `sy-protocol` and `sy-client`
- **UI**: React 19, TypeScript (strict), Vite 8, TanStack Router (memory history), Zustand 5, @xterm/xterm 6 with the WebGL addon

## Key files

| File | Owns |
|---|---|
| `src-tauri/src/engine.rs` | the host's one engine connection: find or start, hello, relay, connection state |
| `src-tauri/src/launcher.rs` | spawns `switchyardd` detached so it outlives the app |
| `src-tauri/src/tray.rs`, `quit.rs` | menu bar item; quit asks first when agents run |
| `src-tauri/capabilities/default.json` | the webview's only permissions: engine send, subscribe, restart |
| `src/engine/` | transport choice (Tauri channel or dev bridge), client, connection store |
| `src/terminal/` | one xterm.js terminal per run, and the `run.output` router |
| `src/router.tsx` | routes, memory history |
| `src/strings.ts` | every UI string |

## Commands

```bash
pnpm dev                                    # builds the devtools engine, then tauri dev
pnpm --filter @switchyard/desktop dev       # Vite only (use the dev bridge)
pnpm --filter @switchyard/desktop typecheck
pnpm app:build                              # universal signed .app
```

## Conventions

- UI code is folder by feature under `src/features/<feature>/` (screens, components, store slice, tests together). `engine/`, `terminal/` and a shared `ui/` stay top level.
- The webview is untrusted: never add Tauri file system, shell, HTTP or process plugins or capabilities.
- The host has no business logic. It rewrites only the request `id` when relaying.
- Agent supplied text is rendered as text, never as HTML. Terminal bytes go straight to xterm.js, bypassing React.
- Zustand stores use narrow selectors so one status tick redraws one card.
- Every UI string goes in `src/strings.ts` (the catalog format comes with feature 7).
- WCAG 2.2 AA and keyboard first: every action reachable by keyboard, visible focus, labelled controls.

## Gotchas

- Notifications and the menu bar only behave correctly in the built, signed `.app`, never in `tauri dev`.
- Tauri WebDriver doesn't work on macOS; Playwright drives the Vite build through `sy-devbridge` instead.
- A webview reload fails every in flight request; the UI must attach again rather than assume the stream continued.
- Vite loads `vite.config.js` before `vite.config.ts`, so a stray compiled copy silently replaces the real config. `tsconfig.node.json` sets `noEmit` so `tsc -b` never writes one; keep it that way.

## Agent skills

- [tauri](../../.claude/skills/tauri/): `full-stack-skills/tauri-skills`, index for Tauri framework work
- [tauri-ipc](../../.claude/skills/tauri-ipc/): `full-stack-skills/tauri-skills`, commands, channels and invoke between UI and host
- [tauri-security](../../.claude/skills/tauri-security/): `full-stack-skills/tauri-skills`, capabilities and permission scopes
- [tauri-build](../../.claude/skills/tauri-build/): `full-stack-skills/tauri-skills`, production builds and bundles
- [tauri-v2](../../.claude/skills/tauri-v2/): `pedronauck/skills`, Tauri v2 config and IPC patterns (spec 0001 wins on conflict)
- [vercel-react-best-practices](../../.claude/skills/vercel-react-best-practices/): `vercel-labs/agent-skills`, React performance patterns
- [tanstack-router](../../.claude/skills/tanstack-router/): `tanstack-skills/tanstack-skills`, type safe routes
- [zustand](../../.claude/skills/zustand/): `pedronauck/skills`, store organisation
- [vite](../../.claude/skills/vite/): `antfu/skills`, Vite config and plugins
- [distributing-tauri-for-macos](../../.claude/skills/distributing-tauri-for-macos/): `dchuk/claude-code-tauri-skills`, DMG, bundle, entitlements (feature 24)
- [macos-packaging-notarization](../../.claude/skills/macos-packaging-notarization/): `robinebers/skills`, signing and notarization (feature 24)
- [vitest](../../.claude/skills/vitest/): `antfu/skills`, UI unit tests
- [playwright-cli](../../.claude/skills/playwright-cli/): `microsoft/playwright-cli`, e2e tests against the Vite build via `sy-devbridge`
- [accessibility-engineering](../../.claude/skills/accessibility-engineering/): `addyosmani/agent-skills`, WCAG 2.2 AA practice

Declined: xterm-js (ssv445), signing-tauri-apps (dchuk)

## Related specs

- [0001 Stack & architecture](../../docs/specs/0001-stack-architecture/index.md)

_Drafted by /audit from the repo, worth a quick human pass. Edit freely: once a line stops matching this draft, later runs treat it as curated and will flag rather than overwrite it._
