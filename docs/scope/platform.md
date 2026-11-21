# Platform

Releases, documentation, updates and reach. Back to the [index](index.md).

## Slice 6: Recovery and first release

### 24. Signed macOS release · needs a decision
A signed, notarized macOS download for early users, built by a repeatable release pipeline.
**Done when:** one release command or CI run produces a signed and notarized macOS build; it installs and opens with no security warnings; each release has a version and notes.
- [ ] Design it (spec): `/architect signed macos release`

### 25. User & contributor docs · needs a decision
Clear docs for people using switchyard and for people contributing to it, especially new adapters.
**Done when:** the user guide covers install, a quickstart and every MVP feature; the contributor guide covers the architecture, writing an adapter, running tests with the fake agent and the release process; docs live in the repo and are updated as features ship.
- [ ] Design it (spec): `/architect user & contributor docs`

## Slice 12: Distribution

### 34. Safe auto update · needs a decision
The app keeps itself up to date without ever breaking a running task.
**Done when:** the app checks for updates, verifies each one's signature before installing, and rolls back a failed update; it never restarts while agents run without asking; you can turn checks off.
- [ ] Design it (spec): `/architect safe auto update`

### 35. Linux & Windows support · needs a decision
Bring switchyard to Linux and Windows with the same behavior as macOS.
**Done when:** installers exist for Linux and Windows; worktrees, terminals, notifications, secure storage and the sandbox work on each; CI runs the test suite on all three platforms.
- [ ] Design it (spec): `/architect linux & windows support`

## Deferred
- **Remote & mobile control**: optionally check on and steer agents on your own machine from another device; off by default, nothing exposed without consent · needs a decision
- **UI translations**: languages beyond English, built on the string catalog from feature 7
