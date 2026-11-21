# Stack skill and MCP discovery

Date: 2026-11-21
Scope: Tauri 2, Rust, tokio, rusqlite/SQLite, React 19, TypeScript, Vite, Zustand, TanStack Router, xterm.js, pnpm, JSON-RPC/JSON Schema (schemars), git/worktrees, macOS signing and notarization.

## Method

- `npx -y skills find <term>` for every item, with one alias retry where results were weak (jsonrpc, @xterm/xterm, jsonschema).
- `npx -y skills add <owner>/<repo> --list` on 16 candidate repos. Listing only, nothing was added. The output was checked for side effects: the global skill count stayed at 38 before and after all `--list` runs.
- `npx -y skills list`: project scope shows no skills. Global scope shows 38 skills (Cloudflare, insforge, jsmastery-pro, and others). None overlap the stack set.
- MCP: 3 web searches (SQLite, git worktree, Tauri). Search snippets only. No repo was opened, so every MCP entry is unverified.
- Nothing was installed.

Confirmation key: "list" = name seen in `skills add --list` output for that repo. "find" = name seen only in `skills find` results.

## Skill candidates by technology

### Tauri 2 (tauri, tauri-apps)
- full-stack-skills/tauri-skills@tauri (list). Router index for Tauri 2 covering Rust backend, frontend integration, and the plugin ecosystem. Same repo has tauri-ipc, tauri-security, tauri-build, tauri-config, tauri-window, tauri-setup, and about 40 tauri-app-* plugin guides (list).
- dchuk/claude-code-tauri-skills@signing-tauri-apps (list). Signing for Tauri apps.
- dchuk/claude-code-tauri-skills@distributing-tauri-for-macos (list). macOS distribution.
- dchuk/claude-code-tauri-skills@configuring-tauri-capabilities, @configuring-tauri-permissions, @understanding-tauri-ipc, @debugging-tauri-apps, @testing-tauri-apps (list). About 40 skills in this repo.
- hairyf/skills@tauri (list). General Tauri skill.
- pedronauck/skills@tauri-v2 (list). Tauri v2 skill.
- nodnarbnitram/claude-code-extensions@tauri-v2 (find). 8.6K installs, not opened.

### Rust (general)
- apollographql/skills@rust-best-practices (list). General Rust conventions.
- po4yka/rust-skills (list). Large set of about 50 Rust skills, including rust-error-handling, rust-async-internals, rust-send-sync, rust-performance, rust-lints, rust-cli, and rust-database.
- pedronauck/skills@rust-best-practices (list). Same name as Apollo's skill, different repo.
- wshobson/agents@rust-async-patterns (find). 22.4K installs.
- leonardomso/rust-skills@rust-skills (find). 7.4K installs.

### tokio
- geoffjay/claude-plugins@tokio-patterns (list). Also tokio-concurrency, tokio-networking, tokio-troubleshooting (list).
- wshobson/agents@rust-async-patterns (find). Async patterns.

### rusqlite / SQLite
- daiki48/dotfiles@rusqlite-guide (list). Small (27 installs). Only rusqlite-specific match.
- po4yka/rust-skills@rust-database (list). Rust database patterns.
- rightnow-ai/openfang@sqlite-expert (list). General SQLite expert.
- martinholovsky/claude-skills-generator@sqlite-database-expert (find). 3.3K installs.
- erayack/sqlite-best-practices@sqlite-best-practices (find). 263 installs.
- Note: no rusqlite-specific skill with real adoption was found.

### React 19
- vercel-labs/agent-skills@vercel-react-best-practices (list). 919.8K installs.
- hairyf/skills@react and @react-router (list).
- pedronauck/skills@react (list).
- Note: nothing specific to React 19 was found. The general React skills above are the closest matches.

### TypeScript
- jeffallan/claude-skills@typescript-pro (list). 15K installs.
- pedronauck/skills@typescript-advanced (list).
- martinholovsky/claude-skills-generator@typescript (list).
- wshobson/agents@typescript-advanced-types (find). 101.1K installs.

### Vite
- antfu/skills@vite (list). 42.6K installs.
- Also antfu/skills@vitest and vitepress (list).

### Zustand
- pedronauck/skills@zustand (list).
- lobehub/lobehub@zustand (find). 7K installs.
- existential-birds/beagle@zustand-state (find). 261 installs.

### TanStack Router (@tanstack/react-router)
- tanstack-skills/tanstack-skills@tanstack-router (list). Also tanstack-start, tanstack-query, and other TanStack skills in the same repo (list).
- tanstack/router@router-core, @router-plugin, @react-router (find). These are from the official TanStack repo. The repo's own `--list` only showed bundle-size-optimization, so these names come from find output only.
- pedronauck/skills@tanstack (list).

### xterm.js (@xterm/xterm)
- ssv445/claude-skills@xterm-js (list). Only usage-oriented xterm skill found.
- xtermjs/xterm.js@unit-test and @benchmark (list). Official repo, but these are test and benchmark skills, not integration guidance.
- ggprompts/my-plugins@xterm-js (find). 6 installs.
- Note: no widely adopted xterm.js integration skill was found.

### pnpm
- antfu/skills@pnpm (list). 23.3K installs.
- hairyf/skills@pnpm (list).

### JSON-RPC / JSON Schema (schemars)
- None found for Rust schemars or for JSON-RPC implementation in Rust. Retried "jsonrpc" and "jsonschema" aliases: only unrelated chain or Go/Delphi/Solana hits.
- Generic only: zaggino/z-schema@writing-json-schemas (find, 336 installs). Not a Rust or schemars skill.
- scaledockhq/scaledock-skills@json-rpc (find). 1 install, not checked.

### git / git worktrees
- obra/superpowers@using-git-worktrees (list). 241K installs.
- jamesrochabrun/skills@git-worktrees (list).
- robinebers/skills@worktree-setup (list).
- neolabhq/context-engineering-kit@git-worktrees (find). 1.2K installs.
- addyosmani/agent-skills@git-workflow-and-versioning (list).
- geoffjay/claude-plugins@git-advanced, @git-conventions, @git-troubleshooting (list).

### macOS code signing and notarization
- dchuk/claude-code-tauri-skills@signing-tauri-apps (list). Tauri-specific.
- dchuk/claude-code-tauri-skills@distributing-tauri-for-macos (list).
- robinebers/skills@macos-signing-entitlements (list). General macOS signing.
- robinebers/skills@macos-packaging-notarization (list). General macOS notarization.
- jamesrochabrun/skills@releasing-macos-apps (list).
- openai/plugins@signing-entitlements (find). 94 installs.
- rorkai/app-store-connect-cli-skills@asc-notarization (find). 7K installs. App Store Connect focus; probably not needed for Tauri direct distribution.

## Installed

- Project scope: none.
- Global scope: 38 skills. None are in the stack set. find-skills is installed.
- Result: no candidate listed above is already installed.

## MCP candidates (search snippets only; repos not opened)

SQLite:
- https://github.com/stackloklabs/sqlite-mcp
- https://github.com/neverinfamous/sqlite-mcp-server (Python, Docker image)
- https://github.com/spences10/mcp-sqlite-tools (separates read-only and destructive tools)
- https://github.com/0xOmarA/mcp-server-sqlite (Rust, fine-grained access control)
- https://github.com/jleonx/mcp-sqlite (npx)
- Note from search: the official reference SQLite MCP server is archived.

Git worktree:
- https://github.com/ben-rogerson/git-worktree-toolbox (MCP server and CLI)
- https://lobehub.com/en/mcp/mandalorian007-git-worktree-mcp (TypeScript; LobeHub listing, not the upstream repo)
- https://lobehub.com/mcp/georgeasoto-worktree-tools-mcp (worktree tools with PR creation)
- Also mentioned in search text without a URL: coderabbitai/gtr-mcp (wraps git-worktree-runner; needs gtr and GitHub CLI for some flags).

Tauri:
- https://github.com/hypothesi/mcp-server-tauri (most prominent; npm @hypothesi/tauri-mcp-server; needs withGlobalTauri in tauri.conf.json; companion Rust plugin tauri-plugin-mcp-bridge)
- https://github.com/delorenj/tauri-mcp-server (Tauri plugin plus MCP server for inspecting and debugging desktop apps)
- https://github.com/moinsen-dev/tauri-plugin-mcp (socket-based; built with pnpm)
- https://mcpservers.org/servers/dirvine/tauri-mcp (Rust; IPC inspection and window control)

## Caveats

- `--list` output for several repos included spinner text and an "installing non-interactively" banner. Nothing was installed (global count unchanged at 38), but the banner is worth knowing about.
- Installs-count figures come from the skills.sh listing and are not an endorsement or a quality check.
- No skill files were read. Each candidate needs a review of its SKILL.md before use.
- MCP entries are unverified. Check repo activity, license, and tool lists before relying on any of them.
