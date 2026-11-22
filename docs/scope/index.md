# Scope: switchyard

switchyard is an open source desktop workspace for running and managing many AI coding agents at once. You hand out several tasks in parallel, each in its own git worktree, and review, compare and merge the results in one place. It drives the agents you already use (Claude Code, Codex, Gemini CLI, OpenCode, and any command line agent through an adapter) as they're installed and signed in on your machine, and never touches their login tokens.

**Build approach:** Tracer Bullet (prove one real thread through every layer first, then thicken one segment at a time, end to end). (basis: vertical slices ship real value early; the riskiest part here is orchestration connecting to real agent processes)
**Workflow:** GA (after develop: check verify, test, a fresh model check review, then document). The project default level of rigor, chosen for every feature. `/architect` is the recommended first stop for a feature with a real decision, but skippable when you already know the build. Any feature can carry its own tag (e.g. `· Beta`) to do more or less.

_These are recommendations to keep your build orderly, not requirements. Skip anything that does not fit: if you already know how to build a feature, use `/develop` and skip `/architect`. You decide when a feature is `done`._

**Product decisions so far:** desktop GUI app · macOS first (core kept cross platform), Linux and Windows later · open source and free · first class adapters for Claude Code, Codex, Gemini CLI and OpenCode, plus a generic CLI adapter · no telemetry, success is daily use · WCAG 2.2 AA and keyboard first · English with every UI string in one catalog, ready for translation · signed macOS download for early users · solo build, no hard deadline.

**MVP** = Foundations through Slice 6 (features 1 to 25). Slices 7 to 12 follow, Limits and cost first.

## At a glance

| # | Feature | Phase | Status |
|---|---------|-------|--------|
| 1 | Stack & architecture | Foundation | done |
| 2 | Coding standards & tooling | Foundation | done |
| 3 | Data model | Foundation | in-progress |
| 4 | Agent adapter contract & fake agent | Foundation | planned |
| 5 | Agent process supervisor | Foundation | planned |
| 6 | Logging & diagnostics | Foundation | planned |
| 7 | Design system & UI foundation | Foundation | planned |
| 8 | Core task loop | Slice 1 | planned |
| 9 | Parallel workspace board | Slice 2 | planned |
| 10 | Attention notifications | Slice 2 | planned |
| 11 | Worktree sandbox & destructive action guard | Slice 2 | planned |
| 12 | Approvals inbox | Slice 2 | planned |
| 13 | Setup & run scripts | Slice 3 | planned |
| 14 | Environment variables & secrets | Slice 3 | planned |
| 15 | Codex adapter | Slice 4 | planned |
| 16 | Gemini CLI adapter | Slice 4 | planned |
| 17 | OpenCode adapter | Slice 4 | planned |
| 18 | Generic CLI adapter | Slice 4 | planned |
| 19 | Agent detection & onboarding | Slice 4 | planned |
| 20 | Line level diff review | Slice 5 | planned |
| 21 | Editor & terminal handoff | Slice 5 | planned |
| 22 | Compare agents & pick a winner | Slice 5 | planned |
| 23 | Crash safe resume | Slice 6 | planned |
| 24 | Signed macOS release | Slice 6 | planned |
| 25 | User & contributor docs | Slice 6 | planned |
| 26 | Concurrency limits & queue | Slice 7 | planned |
| 27 | Usage, cost & time tracking | Slice 7 | planned |
| 28 | Commit & pull request creation | Slice 8 | planned |
| 29 | CI status | Slice 8 | planned |
| 30 | Task history & lifecycle controls | Slice 9 | planned |
| 31 | Prompt templates | Slice 10 | planned |
| 32 | MCP servers & agent plugins | Slice 11 | planned |
| 33 | Agent managed workspaces | Slice 11 | planned |
| 34 | Safe auto update | Slice 12 | planned |
| 35 | Linux & Windows support | Slice 12 | planned |

## Epics

- [Foundations](foundations.md): 1 to 7 · 2 done, 1 in progress, 4 planned
- [Workspaces](workspaces.md): running tasks, environments, recovery, history, prompts (8, 9, 10, 13, 14, 23, 30, 31) · 8 planned
- [Control](control.md): sandbox, approvals, limits, cost (11, 12, 26, 27) · 4 planned
- [Agents](agents.md): adapters, onboarding, extensions (15 to 19, 32, 33) · 7 planned, 1 deferred
- [Review](review.md): diff review, handoff, compare, pull requests, CI (20, 21, 22, 28, 29) · 5 planned
- [Platform](platform.md): releases, docs, updates, more platforms, remote (24, 25, 34, 35) · 4 planned, 2 deferred

## Slices

- **Foundation:** the stack, conventions, data model, the adapter contract every agent plugs into, the process supervisor, logging and the design system. Nothing touches a real agent until these exist. (basis: foundations before features)
- **Slice 1, Core task loop:** one real thread through every layer: add a repo, run Claude Code in its own worktree, watch it live, review the diff, merge. This is also the walking skeleton.
- **Slice 2, Run many safely:** many workspaces side by side, notifications when an agent needs you, the worktree sandbox and one approvals inbox. (basis: parallelism multiplies the risk, so safety lands in the same slice)
- **Slice 3, Project environment:** setup and run scripts per worktree, environment variables and secrets.
- **Slice 4, More agents:** Codex, Gemini CLI, OpenCode, the generic adapter, then onboarding that detects all of them.
- **Slice 5, Review and compare:** line level review, handoff to your editor, and the same task across several agents with a winner merged.
- **Slice 6, Recovery and first release:** crash safe resume, a signed macOS build, user and contributor docs. MVP ships here.
- **Slice 7, Limits and cost** · **Slice 8, Pull requests and CI** · **Slice 9, History** · **Slice 10, Prompts** · **Slice 11, Extensions** · **Slice 12, Distribution**.

## Deferred

Out of scope for the current build pass, kept so the plan stays honest.
- **Remote & mobile control**: check on and steer agents on your machine from another device · needs a decision ([platform](platform.md))
- **pk & Cursor agent first class adapters**: both run through the generic adapter until then · needs a decision ([agents](agents.md))
- **UI translations**: languages beyond English, using the string catalog from feature 7 ([platform](platform.md))

## References

**Project sources**
- Your product description and requirements (the planning conversation of 2026-11-21): features, production requirements, and the decisions listed at the top of this file.

**Practices & standards**
- Vertical slices ship real value early (Tracer Bullet development, walking skeleton).
- Foundations before features; cross cutting tooling before the code that must follow it.
- The data model is the costliest thing to redo.
- Decide an interface before its first implementation (adapter contract before adapters).
- Never log secrets (OWASP logging guidance).
- WCAG 2.2 AA for accessibility.

**Links** (web verified)
- Tracer Bullets and Prototypes (Hunt and Thomas): https://www.artima.com/articles/tracer-bullets-and-prototypes
- git worktree manual: https://git-scm.com/docs/git-worktree
- WCAG 2.2, W3C Recommendation: https://www.w3.org/TR/2023/REC-WCAG22-20231005/
- OWASP Logging Cheat Sheet: https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html
- Claude Code headless mode: https://code.claude.com/docs/en/headless
- OpenAI Codex CLI: https://github.com/openai/codex
- Gemini CLI: https://github.com/google-gemini/gemini-cli
- OpenCode CLI: https://opencode.ai/docs/en/cli
- Cursor CLI: https://cursor.com/docs/cli/overview
- Model Context Protocol specification: https://modelcontextprotocol.io/specification
- Agent Client Protocol (worth a look when designing feature 4): https://agentclientprotocol.com/
- Apple, notarizing macOS software: https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution
- The Update Framework, for safe updates: https://theupdateframework.io/

## Legend

**The decision box.** Every feature carries exactly one, the sub task whose label ends with `(spec)`. Its wording varies (`Design it (spec)` normally, `Decide the stack (spec)` on Stack & architecture), so skills locate it by that `(spec)` suffix, never by an exact label. Every other box is an execution box and `/architect` never ticks one.

**Feature lifecycle**: the scope updates as a feature moves; each row is what it shows and who sets it:

| State | Set by | The feature shows |
|---|---|---|
| `planned` · needs a decision | `/scope` | one box: `Design it (spec): /architect <feature>` |
| `in-progress` (designed) | **`/architect` at spec capture** | `Design it` ticked; spec linked; `Build it: /develop <feature>` + **2 to 5 milestones**; the tier's closing boxes (`Verify it`, `Test it`, `Review it`, `Document it` at GA); any surfaced follow up enrolled |
| `in-progress` (building) | `/develop` | milestone sub boxes tick one by one; code pointer filled |
| `in-progress` (verified) | `/check verify` | `Build it` + milestones ticked; `Verify it` ticked |
| `done` | **you, when you decide it is** (any skill sets it when you say so); `/sync` reconciles | boxes you ran ticked, skipped ones marked skipped; at GA, after `/test` is the suggested point to call it done; `/sync` captures conventions |

- **Next step** = the first unticked box (always a command or a tracked milestone).
- **needs a decision** = run `/architect` first; otherwise straight to `/develop` (or `/audit` for standards & tooling). The tag drops once the spec is captured.
- **Atomic build tasks live in the spec's `## Build plan`, not here**: the scope carries only the milestone rollup.
- **Status** `planned` → `in-progress` → `done`, plus `existing` (pre workflow) and `dropped` (de scoped, kept for history).
- **Approach tag** beside a heading (e.g. `· Facade`) overrides the project default for that feature; no tag = inherits it.
- **Workflow tier tag** beside a heading (e.g. `· Beta`) sets that one feature's rigor above or below the project default; no tag inherits the default.
- **Workflow** (header line) is the project default, what runs after `/develop`: **Prototype** = nothing; **Alpha** = `/check verify`; **Beta** = `/check verify` then `/test`; **GA** = adds a fresh model `/check review` then `/document`. A feature built on an unratified decision (an `Assumed` spec) stays flagged, but that never blocks `done`.
- **Pointer line** (`spec <n> · code in <path>`): the spec link added by `/architect`, the code path by `/develop`.
