# Verify: Stack & architecture · spec 0001 · updated 2026-11-21
_Steps derived from spec 0001's "What the scaffold must prove" (points 1 to 10). `/check verify` runs these; `/test` locks the durable ones. Status from the `/develop` run on 2026-11-21: [x] = checked then, [ ] = still needs a person at the running app._

## UI / manual
- [ ] `pnpm app:build`, open `target/universal-apple-darwin/release/bundle/macos/switchyard.app` → the window shows "Connected", the engine version (`0.1.0`), protocol `1.0` and a process id → 2
- [ ] Close the window (red button or Cmd+W) → the menu bar item stays, `pgrep -x switchyardd` still lists the engine; click the menu bar item, choose "Open switchyard" → the window comes back with the **same** process id → 3
- [ ] Menu bar item text shows "Engine running · no agents" (or the agent count) → 3
- [ ] With agents running (devtools build, load check started), choose "Quit switchyard" → a confirm dialog says how many agents keep running; Cancel keeps the app open → 3
- [ ] `kill -9 $(pgrep -x switchyardd)` while the app is open → the window shows "Engine stopped" within a second; click "Restart the engine" → back to "Connected" with a new process id → 5
- [ ] `pnpm app:build:devtools`, open the app, choose "Load check", "Start 12 runs" on an Apple silicon Mac → all 12 terminals render live output and the p95 shown stays under 100 ms ("Pass") → 6
- [ ] In the load check, type into a terminal → the characters echo back → 6

## Commands
- [x] `pnpm app:build` → a universal `.app`; `pnpm release:check` → bundle id `dev.switchyard.app`, minimum macOS 13.0, app and `Contents/MacOS/switchyardd` both `arm64` + `x86_64`, signed with hardened runtime → 1
- [ ] `pnpm release:check --require-team-id` with a Developer ID identity (`APPLE_SIGNING_IDENTITY`) → app and engine share one Team ID (feature 24) → 1
- [x] `cargo build --workspace` and `pnpm build` → both pass clean → 1
- [x] Start the built app binary with `SWITCHYARD_DATA_DIR=/tmp/sy-app` → it spawns one `switchyardd`, `app.log` shows `state=Connected` → 2
- [x] Launch the app binary a second time → still exactly one `switchyardd`, same pid → 4
- [x] `cargo test -p sy-daemon --test engine` → a second engine logs "already running", exits 0, the socket inode is unchanged → 4
- [x] Kill the app processes (no agents) → the engine keeps running; relaunch → `engine.info` pid unchanged → 3
- [x] `kill -9` the engine → `app.log` shows `state=Stopped`; relaunching takes over the lock, `engine.log` shows "removed stale socket", new pid → 5
- [x] `cargo test -p sy-daemon --features devtools --test engine` → no gap and no duplicate across attach plus live output; a stalled client gets `run.resync_needed` while a second client keeps streaming → 6
- [x] `pnpm boundaries:check` → "Dependency rules hold"; planting `sy-infra` in `sy-core` or a `cfg(target_os)` in `sy-core` makes it fail → 7
- [x] `pnpm protocol:check` → "Protocol schema and TS types match"; edit a type in `crates/sy-protocol` without regenerating → it fails → 8
- [x] `cargo test -p sy-client` → the peer check rejects a different uid; `cargo test -p sy-daemon --test engine` → a line over 1 MiB closes only that connection → 9
- [x] `pnpm release:check` → the bundled engine answers `-32601` to every `dev.*` method and the bundle contains no `sy-devbridge` → 10

## Acceptance-criteria coverage
- 1 (universal signed bundle, clean builds) … release check, workspace build; Team ID pending a Developer ID certificate
- 2 (window shows hello) … app launch + app.log; rendered window still to eyeball
- 3 (close to menu bar, same engine) … app kill and relaunch; real window close and menu bar reopen still to eyeball
- 4 (single engine) … integration test + two launches
- 5 (kill -9 recovery) … app.log + relaunch; the Restart button still to click
- 6 (12 stream p95 under 100 ms) … engine side by integration tests; the UI measurement still to run
- 7 (dependency rules) … boundaries check
- 8 (schema drift) … protocol check plus the `committed_schema_should_match_the_rust_types` test
- 9 (peer check, 1 MiB line) … unit and integration tests
- 10 (no dev methods, no dev bridge in release) … release check
