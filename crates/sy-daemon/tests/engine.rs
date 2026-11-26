#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! Drives the real `switchyardd` binary over its socket, each test with its own
//! short `/tmp/sy-<random>` data dir.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sy_client::{CallError, Connection, EnginePaths};
use sy_protocol::{ClientKind, EngineInfo, ErrorCode, HelloResult, PROTOCOL, methods};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Engine {
    _dir: tempfile::TempDir,
    paths: EnginePaths,
    child: Child,
}

fn data_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("sy-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Build the helper binaries the engine finds beside itself (`sy-signal`
/// and `sy-fake-agent`), once per test process.
#[cfg(feature = "devtools")]
fn build_helpers() {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "-p", "sy-signal", "-p", "sy-fake-agent"])
            .status()
            .expect("running cargo build for the helper binaries");
        assert!(status.success(), "building the helper binaries failed");
    });
}

fn spawn_in(dir: &Path, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_switchyardd"));
    // Devtools engines skip your login shell unless a test says otherwise
    // (`SWITCHYARD_LOGIN_ENV=shell`); release engines ignore this.
    cmd.env("SWITCHYARD_LOGIN_ENV", "inherit")
        .env("SWITCHYARD_DATA_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

impl Engine {
    async fn start() -> Self {
        Self::start_with(&[]).await
    }

    async fn start_with(env: &[(&str, &str)]) -> Self {
        #[cfg(feature = "devtools")]
        build_helpers();
        let dir = data_dir();
        let paths = EnginePaths::for_data_dir(dir.path().to_path_buf());
        let child = spawn_in(dir.path(), env);
        let engine = Self {
            _dir: dir,
            paths,
            child,
        };
        engine.wait_for_socket().await;
        engine
    }

    async fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if UnixStream::connect(&self.paths.socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "engine socket never came up at {}",
            self.paths.socket.display()
        );
    }

    async fn client(&self) -> Connection {
        let (conn, _notifications) = Connection::connect(&self.paths.socket).await.unwrap();
        conn.hello(ClientKind::Test, "test").await.unwrap();
        conn
    }

    async fn raw(&self) -> Raw {
        Raw::connect(&self.paths.socket).await
    }

    fn wait_exit(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.paths.logs_dir.join("engine.log")).unwrap_or_default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A line level client, for sending what `Connection` never would.
struct Raw {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
    /// Messages that arrived while waiting for a response, for `next`.
    pending: std::collections::VecDeque<Value>,
}

impl Raw {
    async fn connect(socket: &Path) -> Self {
        let (read, writer) = UnixStream::connect(socket).await.unwrap().into_split();
        Self {
            reader: BufReader::new(read),
            writer,
            next_id: 1,
            pending: std::collections::VecDeque::new(),
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            .to_string()
            + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
        id
    }

    /// Next message (first any that arrived while waiting for a response),
    /// or `None` on EOF or after 10 s of silence.
    async fn next(&mut self) -> Option<Value> {
        if let Some(msg) = self.pending.pop_front() {
            return Some(msg);
        }
        self.read().await
    }

    async fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .ok()?
            .ok()?;
        (n > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    async fn response(&mut self, id: u64) -> Value {
        if let Some(at) = self.pending.iter().position(|m| m["id"] == json!(id)) {
            return self.pending.remove(at).unwrap_or_default();
        }
        loop {
            let msg = self
                .read()
                .await
                .expect("connection closed before the response");
            if msg["id"] == json!(id) {
                return msg;
            }
            self.pending.push_back(msg);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    async fn hello(&mut self, major: u32) -> Value {
        self.hello_as("test", major).await
    }

    async fn hello_as(&mut self, client: &str, major: u32) -> Value {
        self.call(methods::SESSION_HELLO, json!({ "client": client, "client_version": "raw", "protocol": { "major": major, "minor": 0 } }))
            .await
    }
}

fn rpc_code(err: &CallError) -> i32 {
    match err {
        CallError::Rpc(e) => e.code,
        other => panic!("expected an RPC error, got {other}"),
    }
}

fn inode(path: &PathBuf) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_should_report_engine_version_pid_and_protocol() {
    let engine = Engine::start().await;
    let (conn, _) = Connection::connect(&engine.paths.socket).await.unwrap();
    let hello: HelloResult = conn.hello(ClientKind::Test, "test").await.unwrap();
    assert_eq!(
        (hello.engine_version.as_str(), hello.pid, hello.protocol),
        (env!("CARGO_PKG_VERSION"), engine.child.id(), PROTOCOL)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_and_data_dir_should_be_private() {
    let engine = Engine::start().await;
    let socket_mode = std::fs::metadata(&engine.paths.socket).unwrap().mode() & 0o777;
    let dir_mode = std::fs::metadata(&engine.paths.data_dir).unwrap().mode() & 0o777;
    assert_eq!((socket_mode, dir_mode), (0o600, 0o700));
}

#[tokio::test(flavor = "multi_thread")]
async fn second_engine_should_exit_zero_and_leave_the_socket_untouched() {
    let engine = Engine::start().await;
    let conn = engine.client().await;
    let before = inode(&engine.paths.socket);

    let status = spawn_in(&engine.paths.data_dir, &[]).wait().unwrap();

    let info: EngineInfo = conn.call(methods::ENGINE_INFO, &json!({})).await.unwrap();
    assert!(status.success(), "second engine exit: {status}");
    assert_eq!(
        (inode(&engine.paths.socket), info.pid),
        (before, engine.child.id())
    );
    assert!(engine.log().contains("already running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_after_kill_9_should_take_the_lock_and_replace_the_stale_socket() {
    let mut engine = Engine::start().await;
    let first_pid = engine.child.id();
    engine.child.kill().unwrap(); // SIGKILL
    engine.child.wait().unwrap();
    assert!(
        engine.paths.socket.exists(),
        "a killed engine leaves its socket behind"
    );

    engine.child = spawn_in(&engine.paths.data_dir, &[]);
    engine.wait_for_socket().await;
    let info: EngineInfo = engine
        .client()
        .await
        .call(methods::ENGINE_INFO, &json!({}))
        .await
        .unwrap();
    assert_ne!(info.pid, first_pid);
}

#[tokio::test(flavor = "multi_thread")]
async fn line_over_1_mib_should_close_only_that_connection() {
    let engine = Engine::start().await;
    let survivor = engine.client().await;
    let mut raw = engine.raw().await;
    let huge = vec![b'a'; sy_protocol::MAX_LINE_BYTES + 1];
    raw.writer.write_all(&huge).await.unwrap();
    let _ = raw.writer.write_all(b"\n").await;

    assert_eq!(
        raw.next().await,
        None,
        "oversized line should close the connection"
    );
    let info: Result<EngineInfo, _> = survivor.call(methods::ENGINE_INFO, &json!({})).await;
    assert!(info.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn non_stable_method_before_hello_should_need_hello() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    let reply = raw
        .call(methods::RUN_ATTACH, json!({ "run_id": "run-1" }))
        .await;
    assert_eq!(
        reply["error"]["code"],
        json!(ErrorCode::HelloRequired.code())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_hello_should_keep_stable_methods_working() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major + 1).await;
    let info = raw.call(methods::ENGINE_INFO, json!({})).await;
    let attach = raw
        .call(methods::RUN_ATTACH, json!({ "run_id": "run-1" }))
        .await;
    assert_eq!(
        (
            info["result"]["pid"].clone(),
            attach["error"]["code"].clone()
        ),
        (
            json!(engine.child.id()),
            json!(ErrorCode::IncompatibleProtocol.code())
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_should_be_method_not_found() {
    let engine = Engine::start().await;
    let err = engine
        .client()
        .await
        .call_value("nope.nothing", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(rpc_code(&err), ErrorCode::MethodNotFound.code());
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_should_answer_remove_the_socket_and_exit_zero() {
    let mut engine = Engine::start().await;
    let conn = engine.client().await;
    let reply: Result<Value, _> = conn.call(methods::ENGINE_SHUTDOWN, &json!({})).await;
    let status = engine.wait_exit(Duration::from_secs(5));
    assert!(reply.is_ok() && status.is_some_and(|s| s.success()) && !engine.paths.socket.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_engine_should_exit_on_its_own() {
    let mut engine = Engine::start_with(&[("SWITCHYARD_IDLE_EXIT_SECS", "1")]).await;
    let status = engine.wait_exit(Duration::from_secs(5));
    assert!(status.is_some_and(|s| s.success()));
}

#[tokio::test(flavor = "multi_thread")]
async fn connected_client_should_keep_an_idle_engine_alive() {
    let mut engine = Engine::start_with(&[("SWITCHYARD_IDLE_EXIT_SECS", "1")]).await;
    let _conn = engine.client().await;
    assert!(engine.wait_exit(Duration::from_secs(3)).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn event_since_should_refuse_a_negative_cursor_and_echo_the_cursor_when_empty() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    let negative = raw
        .call(methods::EVENT_SINCE, json!({ "cursor": -1 }))
        .await;
    let past_the_end = raw
        .call(methods::EVENT_SINCE, json!({ "cursor": 1_000_000 }))
        .await;
    let bad_filter = raw
        .call(
            methods::EVENT_SINCE,
            json!({ "cursor": 0, "run_id": "not-an-id" }),
        )
        .await;
    assert_eq!(
        (
            &negative["error"]["code"],
            &past_the_end["result"],
            &bad_filter["error"]["code"]
        ),
        (
            &json!(ErrorCode::InvalidParams.code()),
            &json!({ "events": [], "next_cursor": 1_000_000, "has_more": false }),
            &json!(ErrorCode::InvalidParams.code())
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn log_read_of_an_unknown_run_should_be_run_not_found() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    for run in [
        "run_0193f0a0b1c27d3e8f9a0b1c2d3e4f50".to_owned(),
        "run-1".to_owned(),
    ] {
        let reply = raw
            .call(methods::RUN_LOG_READ, json!({ "run_id": run, "offset": 0 }))
            .await;
        assert_eq!(
            reply["error"]["code"],
            json!(ErrorCode::RunNotFound.code()),
            "{run}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_read_methods_should_need_hello() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    for (method, params) in [
        (methods::EVENT_SINCE, json!({ "cursor": 0 })),
        (methods::RUN_LOG_READ, json!({ "run_id": "x", "offset": 0 })),
    ] {
        let reply = raw.call(method, params).await;
        assert_eq!(
            reply["error"]["code"],
            json!(ErrorCode::HelloRequired.code()),
            "{method}"
        );
    }
}

#[cfg(not(feature = "devtools"))]
#[tokio::test(flavor = "multi_thread")]
async fn release_engine_should_not_answer_dev_methods() {
    let engine = Engine::start().await;
    let conn = engine.client().await;
    let err = conn
        .call_value(methods::DEV_AGENT_RUN, json!({ "agent_id": "fake" }))
        .await
        .unwrap_err();
    assert_eq!(rpc_code(&err), ErrorCode::MethodNotFound.code());
}

// ---- storage failure handling (spec 0002, AC-1, AC-3, AC-4) ----

/// Ask the engine to shut down and wait for it to exit.
async fn stop(engine: &mut Engine) {
    let mut raw = engine.raw().await;
    raw.call(methods::ENGINE_SHUTDOWN, json!({})).await;
    assert!(
        engine.wait_exit(Duration::from_secs(10)).is_some(),
        "engine did not exit"
    );
}

/// Start a new engine on the same data dir.
async fn restart(engine: &mut Engine) {
    engine.child = spawn_in(&engine.paths.data_dir, &[]);
    engine.wait_for_socket().await;
}

fn db_path(engine: &Engine) -> PathBuf {
    engine.paths.data_dir.join("switchyard.db")
}

/// Names in the data dir that start with `prefix`.
fn names_starting(engine: &Engine, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&engine.paths.data_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    names
}

/// An engine that started twice and stopped, so its database is at the newest
/// version and has one backup (the second start found none and wrote one).
async fn stopped_with_a_backup() -> Engine {
    let mut engine = Engine::start().await;
    stop(&mut engine).await;
    restart(&mut engine).await;
    stop(&mut engine).await;
    engine
}

async fn storage_of(raw: &mut Raw) -> Value {
    raw.call(methods::ENGINE_INFO, json!({})).await["result"]["storage"].clone()
}

/// Call `storage.recover` and also wait for the `storage.status` it broadcasts.
async fn recover(raw: &mut Raw, action: &str) -> (Value, Option<Value>) {
    let id = raw
        .send(
            methods::STORAGE_RECOVER,
            json!({ "action": action, "confirm": true }),
        )
        .await;
    let mut reply = None;
    let mut status = None;
    while reply.is_none()
        || (status.is_none()
            && reply
                .as_ref()
                .is_some_and(|r: &Value| r.get("result").is_some()))
    {
        let msg = raw.next().await.expect("closed during storage.recover");
        if msg["id"] == json!(id) {
            reply = Some(msg);
        } else if msg["method"] == json!(methods::STORAGE_STATUS) {
            status = Some(msg["params"].clone());
        }
    }
    (reply.unwrap(), status)
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_info_should_report_ok_storage_with_its_schema_version() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    let storage = storage_of(&mut raw).await;
    assert_eq!(
        (storage["status"].clone(), storage["schema_version"].clone()),
        (json!("ok"), json!(sy_infra::storage::MIGRATIONS.len()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_shutdown_should_checkpoint_the_wal_to_empty() {
    let mut engine = Engine::start().await;
    stop(&mut engine).await;
    let wal = engine.paths.data_dir.join("switchyard.db-wal");
    let wal_len = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(wal_len, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn recover_should_refuse_when_storage_is_ok_or_confirm_is_missing() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    let unconfirmed = raw
        .call(
            methods::STORAGE_RECOVER,
            json!({ "action": "start_fresh", "confirm": false }),
        )
        .await;
    let not_needed = raw
        .call(
            methods::STORAGE_RECOVER,
            json!({ "action": "start_fresh", "confirm": true }),
        )
        .await;
    assert_eq!(
        (
            unconfirmed["error"]["code"].clone(),
            not_needed["error"]["code"].clone()
        ),
        (
            json!(ErrorCode::InvalidParams.code()),
            json!(ErrorCode::StorageRecoverNotNeeded.code())
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn newer_schema_should_serve_only_stable_methods_touch_nothing_and_restore_a_backup() {
    let mut engine = stopped_with_a_backup().await;
    rusqlite::Connection::open(db_path(&engine))
        .unwrap()
        .pragma_update(None, "user_version", 999)
        .unwrap();
    let before = std::fs::read(db_path(&engine)).unwrap();
    restart(&mut engine).await;

    let mut raw = engine.raw().await;
    let storage = storage_of(&mut raw).await; // stable: works before hello
    raw.hello(PROTOCOL.major).await;
    let since = raw.call(methods::EVENT_SINCE, json!({ "cursor": 0 })).await;
    let attach = raw
        .call(
            methods::RUN_ATTACH,
            json!({ "run_id": "run_00000000000000000000000000000000" }),
        )
        .await;
    let unavailable = json!(ErrorCode::StorageUnavailable.code());
    assert_eq!(
        (
            storage["status"].clone(),
            storage["schema_version"].clone(),
            storage["latest_backup"].is_string(),
            since["error"]["code"].clone(),
            attach["error"]["code"].clone(),
            std::fs::read(db_path(&engine)).unwrap() == before,
        ),
        (
            json!("newer_schema"),
            json!(999),
            true,
            unavailable.clone(),
            unavailable,
            true
        )
    );

    let (reply, notified) = recover(&mut raw, "restore_backup").await;
    let after = raw.call(methods::EVENT_SINCE, json!({ "cursor": 0 })).await;
    let bad = names_starting(&engine, "switchyard.db");
    assert_eq!(
        (
            reply["result"]["storage"]["status"].clone(),
            notified.map(|n| n["status"].clone()),
            after.get("error").is_none(),
            bad.iter().any(|n| n.starts_with("switchyard.db.bad-")),
            names_starting(&engine, "runs.bad-").len(),
        ),
        (json!("ok"), Some(json!("ok")), true, true, 1)
    );
    let set_aside = engine.paths.data_dir.join(
        bad.iter()
            .find(|n| n.starts_with("switchyard.db.bad-"))
            .unwrap(),
    );
    assert_eq!(
        std::fs::read(set_aside).unwrap(),
        before,
        "the bad file was changed"
    );
    let again = raw
        .call(
            methods::STORAGE_RECOVER,
            json!({ "action": "restore_backup", "confirm": true }),
        )
        .await;
    assert_eq!(
        again["error"]["code"],
        json!(ErrorCode::StorageRecoverNotNeeded.code())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_database_should_be_set_aside_untouched_and_start_fresh() {
    let mut engine = stopped_with_a_backup().await;
    let mut bytes = std::fs::read(db_path(&engine)).unwrap();
    bytes[..16].copy_from_slice(b"definitely not!!");
    std::fs::write(db_path(&engine), &bytes).unwrap();
    restart(&mut engine).await;

    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    let status = storage_of(&mut raw).await["status"].clone();
    let (reply, _) = recover(&mut raw, "start_fresh").await;
    let bad = names_starting(&engine, "switchyard.db.bad-");
    let kept = std::fs::read(engine.paths.data_dir.join(&bad[0])).unwrap();
    assert_eq!(
        (
            status,
            reply["result"]["storage"]["status"].clone(),
            reply["result"]["storage"]["schema_version"].clone(),
            kept == bytes,
        ),
        (
            json!("corrupt"),
            json!("ok"),
            json!(sy_infra::storage::MIGRATIONS.len()),
            true
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_database_should_be_unavailable_and_restore_the_backup() {
    use std::os::unix::fs::PermissionsExt;
    let mut engine = stopped_with_a_backup().await;
    std::fs::set_permissions(db_path(&engine), std::fs::Permissions::from_mode(0o000)).unwrap();
    restart(&mut engine).await;

    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    let status = storage_of(&mut raw).await["status"].clone();
    let (reply, _) = recover(&mut raw, "restore_backup").await;
    let bad = names_starting(&engine, "switchyard.db.bad-");
    let bad_mode = std::fs::metadata(engine.paths.data_dir.join(&bad[0]))
        .unwrap()
        .mode()
        & 0o777;
    assert_eq!(
        (
            status,
            reply["result"]["storage"]["status"].clone(),
            bad_mode
        ),
        (json!("unavailable"), json!("ok"), 0o600)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_with_no_backup_should_be_backup_missing_and_move_nothing() {
    let mut engine = Engine::start().await;
    stop(&mut engine).await; // a fresh database: no backup
    let mut bytes = std::fs::read(db_path(&engine)).unwrap();
    bytes[..16].copy_from_slice(b"definitely not!!");
    std::fs::write(db_path(&engine), &bytes).unwrap();
    restart(&mut engine).await;

    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major).await;
    let reply = raw
        .call(
            methods::STORAGE_RECOVER,
            json!({ "action": "restore_backup", "confirm": true }),
        )
        .await;
    assert_eq!(
        (
            reply["error"]["code"].clone(),
            names_starting(&engine, "switchyard.db.bad-").len(),
            names_starting(&engine, "runs.bad-").len(),
            std::fs::read(db_path(&engine)).unwrap() == bytes,
            storage_of(&mut raw).await["status"].clone(),
        ),
        (
            json!(ErrorCode::BackupMissing.code()),
            0,
            0,
            true,
            json!("corrupt")
        )
    );
}

#[test]
fn no_engine_code_should_change_the_process_umask() {
    // Agents inherit the engine's umask, so every file gets explicit mode bits instead (AC-1).
    fn scan(dir: &Path, hits: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if !path.ends_with("tests") && !path.ends_with("target") {
                    scan(&path, hits);
                }
            } else if path.extension().is_some_and(|e| e == "rs")
                && std::fs::read_to_string(&path).unwrap().contains("umask(")
            {
                hits.push(path);
            }
        }
    }
    let mut hits = Vec::new();
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    scan(crates, &mut hits);
    assert!(hits.is_empty(), "umask changed in {hits:?}");
}

// ---- the login environment (spec 0003, AC-16) ----

/// An executable stub script in `dir`.
fn stub(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// `engine.info.login_env`, once the first capture finished (within 10 s).
async fn login_env(raw: &mut Raw) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = raw.call(methods::ENGINE_INFO, json!({})).await;
        if let Some(env) = info["result"].get("login_env") {
            return env.clone();
        }
        assert!(Instant::now() < deadline, "no login_env in {info}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_login_shell_should_be_asked_and_a_hanging_one_should_fall_back_after_5_s() {
    let stubs = tempfile::tempdir().unwrap();
    let noisy = stub(
        stubs.path(),
        "noisy",
        "echo 'hello from .zshrc'\neval \"$4\"",
    );
    let hangs = stub(stubs.path(), "hangs", "sleep 30");
    let mut results = Vec::new();
    for shell in [&noisy, &hangs] {
        let engine =
            Engine::start_with(&[("SWITCHYARD_LOGIN_ENV", "shell"), ("SHELL", shell)]).await;
        let mut raw = engine.raw().await;
        let started = Instant::now();
        let env = login_env(&mut raw).await;
        results.push((env, started.elapsed()));
    }
    let (ok, fallback) = (&results[0].0, &results[1]);
    assert_eq!(ok, &json!({ "status": "ok" }));
    assert_eq!(fallback.0["status"], json!("fallback"));
    assert!(
        fallback.0["message"]
            .as_str()
            .is_some_and(|m| m.contains("did not answer") && m.contains("/opt/homebrew/bin")),
        "{}",
        fallback.0
    );
    assert!(fallback.1 < Duration::from_secs(8), "{:?}", fallback.1);
}

#[cfg(feature = "devtools")]
mod devtools {
    use super::*;
    use base64::Engine as _;
    use sy_protocol::{AgentRunResult, RunAttachResult};

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn unb64(text: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(text.as_str().unwrap())
            .unwrap()
    }

    /// A literal no test ever types, so a step waiting for it waits forever.
    const NEVER: &str = "never-typed-literal-5d1c";

    /// A fake agent script: a `flood` at `bytes_per_sec` (echoing input with
    /// `echo`), or with no rate, one line and then echo (or quiet) until stopped.
    fn script(bytes_per_sec: u64, echo: bool) -> Value {
        match (bytes_per_sec, echo) {
            (0, true) => json!([{ "print": "ready\r\n" }, { "echo_until": NEVER }]),
            (0, false) => json!([{ "print": "ready\r\n" }, { "expect_input": NEVER }]),
            (rate, echo) => json!([{ "flood": { "bytes_per_sec": rate, "echo": echo } }]),
        }
    }

    /// `dev.agent_run` with the fake agent and `steps`.
    async fn fake_runs(raw: &mut Raw, count: u32, steps: Value) -> AgentRunResult {
        let reply = raw
            .call(
                methods::DEV_AGENT_RUN,
                json!({ "agent_id": "fake", "count": count, "options": { "script": steps } }),
            )
            .await;
        serde_json::from_value(reply["result"].clone()).unwrap_or_else(|_| panic!("{reply}"))
    }

    /// Fake runs that stream (or echo, or idle) until stopped.
    async fn agent_runs(raw: &mut Raw, count: u32, bytes_per_sec: u64, echo: bool) -> Vec<String> {
        fake_runs(raw, count, script(bytes_per_sec, echo))
            .await
            .run_ids
            .into_iter()
            .map(|id| id.0)
            .collect()
    }

    /// `run.stop` each run, then wait for every answer and each run's final
    /// `run.status`. `raw` must have said hello.
    async fn stop_runs(raw: &mut Raw, runs: &[String]) {
        let mut stops = Vec::new();
        for run in runs {
            stops.push(raw.send(methods::RUN_STOP, json!({ "run_id": run })).await);
        }
        let mut answered = 0;
        let mut ended = std::collections::HashSet::new();
        while answered < stops.len() || ended.len() < runs.len() {
            let msg = raw.next().await.expect("closed while stopping runs");
            if stops.iter().any(|id| msg["id"] == json!(id)) {
                assert!(msg.get("error").is_none(), "{msg}");
                answered += 1;
            } else if msg["method"] == json!(methods::RUN_STATUS)
                && ["finished", "failed", "stopped"]
                    .contains(&msg["params"]["status"].as_str().unwrap_or(""))
                && runs.iter().any(|r| msg["params"]["run_id"] == json!(r))
            {
                ended.insert(msg["params"]["run_id"].as_str().unwrap().to_owned());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_then_live_output_should_have_no_gap_and_no_duplicate() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 200_000, false).await.remove(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let id = raw
            .send(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;

        let mut cursor = None;
        let mut live_bytes = 0_usize;
        // A live chunk can beat its own attach reply; it applies after the reply.
        let mut early: Vec<(u64, usize)> = Vec::new();
        let take = |cursor: &mut u64, offset: u64, len: usize, live: &mut usize| {
            let end = offset + len as u64;
            assert!(
                offset <= *cursor,
                "gap: chunk starts at {offset}, expected at most {cursor}"
            );
            if end > *cursor {
                *live += (end - *cursor) as usize;
                *cursor = end;
            }
        };
        while live_bytes < 100_000 {
            let msg = raw.next().await.unwrap();
            if msg["id"] == json!(id) {
                let attach: RunAttachResult =
                    serde_json::from_value(msg["result"].clone()).unwrap();
                let mut at = attach.end_offset;
                for (offset, len) in early.drain(..) {
                    take(&mut at, offset, len, &mut live_bytes);
                }
                cursor = Some(at);
            } else if msg["method"] == json!(methods::RUN_OUTPUT) {
                let offset = msg["params"]["offset"].as_u64().unwrap();
                let len = unb64(&msg["params"]["data_b64"]).len();
                match cursor.as_mut() {
                    Some(at) => take(at, offset, len, &mut live_bytes),
                    None => early.push((offset, len)),
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn echo_run_should_write_input_back_as_output() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, true).await.remove(0);
        raw.call(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(b"ping-42") }),
        )
        .await;
        loop {
            let msg = raw.next().await.unwrap();
            if msg["method"] == json!(methods::RUN_OUTPUT)
                && unb64(&msg["params"]["data_b64"]).ends_with(b"ping-42")
            {
                break;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn input_over_64_kib_should_be_refused() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, true).await.remove(0);
        let big = vec![b'x'; sy_protocol::MAX_INPUT_BYTES + 1];
        let reply = raw
            .call(
                methods::RUN_INPUT,
                json!({ "run_id": run, "data_b64": b64(&big) }),
            )
            .await;
        assert_eq!(
            reply["error"]["code"],
            json!(ErrorCode::InputTooLarge.code())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_client_should_get_resync_while_a_fast_client_keeps_streaming() {
        let engine = Engine::start().await;
        let mut slow = engine.raw().await;
        slow.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut slow, 1, 8 * 1024 * 1024, false)
            .await
            .remove(0);
        slow.call(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;
        // Stop reading `slow` and let its queue pass the 8 MiB cap.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        let mut fast = engine.raw().await;
        fast.hello(PROTOCOL.major).await;
        fast.call(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;
        let started = Instant::now();
        let mut fast_bytes = 0_usize;
        while started.elapsed() < Duration::from_secs(1) {
            let msg = fast.next().await.unwrap();
            assert_ne!(
                msg["method"],
                json!(methods::RUN_RESYNC_NEEDED),
                "the fast client fell behind too"
            );
            if msg["method"] == json!(methods::RUN_OUTPUT) {
                fast_bytes += msg["params"]["data_b64"].as_str().unwrap().len();
            }
        }
        assert!(
            fast_bytes > 4 * 1024 * 1024,
            "fast client only got {fast_bytes} base64 bytes in 1 s"
        );

        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        loop {
            let Some(msg) = slow.next().await else {
                panic!("slow client never saw run.resync_needed; saw {seen:?}")
            };
            *seen
                .entry(msg["method"].as_str().unwrap_or("response").to_owned())
                .or_default() += 1;
            if msg["method"] == json!(methods::RUN_RESYNC_NEEDED) {
                assert_eq!(msg["params"]["run_id"], json!(run));
                break;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_should_wait_for_runs_then_exit() {
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let runs = agent_runs(&mut raw, 2, 1000, false).await;
        raw.call(methods::ENGINE_DRAIN, json!({})).await;
        let refused = raw
            .call(methods::DEV_AGENT_RUN, json!({ "agent_id": "fake" }))
            .await;
        assert_eq!(
            refused["error"]["code"],
            json!(ErrorCode::EngineDraining.code())
        );
        assert!(
            engine.wait_exit(Duration::from_millis(500)).is_none(),
            "exited while runs were active"
        );

        stop_runs(&mut raw, &runs).await;
        assert!(
            engine
                .wait_exit(Duration::from_secs(5))
                .is_some_and(|s| s.success())
        );
    }

    /// What one client saw of each run: reassembled bytes from attach replay
    /// plus live chunks, and the highest `offset + length` it received.
    #[derive(Default, Debug)]
    struct Seen {
        bytes: Vec<u8>,
        cursor: u64,
        highest: u64,
    }

    impl Seen {
        fn take(&mut self, offset: u64, data: &[u8]) {
            assert!(
                offset <= self.cursor,
                "gap: chunk at {offset}, client at {}",
                self.cursor
            );
            let end = offset + data.len() as u64;
            self.highest = self.highest.max(end);
            if end <= self.cursor {
                return;
            }
            let skip = usize::try_from(self.cursor.saturating_sub(offset)).unwrap();
            self.bytes.extend_from_slice(&data[skip..]);
            self.cursor = end;
        }
    }

    /// Attach to each run and record its replay, keeping every live chunk that
    /// arrives meanwhile (a chunk can beat its own attach reply; it is applied after).
    async fn attach_all(
        raw: &mut Raw,
        runs: &[String],
        seen: &mut std::collections::HashMap<String, Seen>,
    ) {
        let mut early: Vec<(String, u64, Vec<u8>)> = Vec::new();
        for run in runs {
            let id = raw
                .send(methods::RUN_ATTACH, json!({ "run_id": run }))
                .await;
            loop {
                let msg = raw.next().await.expect("closed while attaching");
                if msg["id"] == json!(id) {
                    let attach: RunAttachResult =
                        serde_json::from_value(msg["result"].clone()).unwrap();
                    let entry = seen.entry(run.clone()).or_default();
                    entry.bytes = unb64(&json!(attach.replay_b64));
                    entry.cursor = attach.end_offset;
                    entry.highest = entry.highest.max(attach.end_offset);
                    break;
                }
                if msg["method"] == json!(methods::RUN_OUTPUT) {
                    let chunk_run = msg["params"]["run_id"].as_str().unwrap().to_owned();
                    let offset = msg["params"]["offset"].as_u64().unwrap();
                    let data = unb64(&msg["params"]["data_b64"]);
                    match seen.get_mut(&chunk_run) {
                        Some(entry) => entry.take(offset, &data),
                        None => early.push((chunk_run, offset, data)),
                    }
                }
            }
        }
        for (run, offset, data) in early {
            seen.entry(run).or_default().take(offset, &data);
        }
    }

    /// Read a run's whole log through `run.log_read`, `page` bytes at a time.
    async fn read_all(raw: &mut Raw, run: &str, page: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let reply = raw
                .call(
                    methods::RUN_LOG_READ,
                    json!({ "run_id": run, "offset": bytes.len(), "max_bytes": page }),
                )
                .await;
            let result = &reply["result"];
            assert_eq!(result["offset"], json!(bytes.len()), "{reply}");
            let data = unb64(&result["data_b64"]);
            assert!(data.len() <= page as usize);
            bytes.extend_from_slice(&data);
            if result["eof"] == json!(true) {
                assert_eq!(result["log_bytes"], json!(bytes.len()));
                return bytes;
            }
        }
    }

    /// Wait until `engine.info` counts no running agent.
    async fn wait_no_live_runs(raw: &mut Raw) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let info = raw.call(methods::ENGINE_INFO, json!({})).await;
            if info["result"]["running_agents"] == json!(0) {
                // The hub entry goes just before the end commits.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return;
            }
            assert!(Instant::now() < deadline, "runs never ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Every journal event, through `event.since` pages.
    async fn all_events(raw: &mut Raw, filter: Value) -> Vec<Value> {
        let mut events = Vec::new();
        let mut cursor = 0;
        loop {
            let mut params = filter.clone();
            params["cursor"] = json!(cursor);
            params["limit"] = json!(500);
            let reply = raw.call(methods::EVENT_SINCE, params).await;
            let result = &reply["result"];
            events.extend(result["events"].as_array().unwrap().iter().cloned());
            cursor = result["next_cursor"].as_i64().unwrap();
            if result["has_more"] != json!(true) {
                return events;
            }
        }
    }

    fn db(engine: &Engine) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            engine.paths.data_dir.join("switchyard.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
    }

    fn log_file(engine: &Engine, run: &str) -> Vec<u8> {
        std::fs::read(
            engine
                .paths
                .data_dir
                .join("runs")
                .join(format!("{run}.log")),
        )
        .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_runs_should_be_saved_and_every_client_should_get_run_status_in_event_order() {
        use std::collections::HashMap;
        let engine = Engine::start().await;
        let mut viewer = engine.raw().await;
        viewer.hello(PROTOCOL.major).await;
        let mut watcher = engine.raw().await; // never attaches
        watcher.hello(PROTOCOL.major).await;

        let result = fake_runs(&mut viewer, 3, script(50_000, false)).await;
        let runs: Vec<String> = result.run_ids.iter().map(|r| r.0.clone()).collect();
        let mut seen = HashMap::new();
        attach_all(&mut viewer, &runs, &mut seen).await;
        // Each run's first output makes it `running`; a run stopped before
        // that rightly skips `running`, so wait for all three.
        for run in &runs {
            until_running(&engine, run).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut stops = Vec::new();
        for run in &runs {
            stops.push(
                viewer
                    .send(methods::RUN_STOP, json!({ "run_id": run }))
                    .await,
            );
        }

        // The viewer: output until each run's `stopped`, which must come after its last output.
        let mut stopped = std::collections::HashSet::new();
        let mut answered = 0;
        while stopped.len() < runs.len() || answered < runs.len() {
            let msg = viewer.next().await.expect("viewer connection closed");
            if stops.iter().any(|id| msg["id"] == json!(id)) {
                answered += 1;
            } else if msg["method"] == json!(methods::RUN_OUTPUT) {
                let run = msg["params"]["run_id"].as_str().unwrap().to_owned();
                assert!(
                    !stopped.contains(&run),
                    "output for {run} after its stopped status"
                );
                let offset = msg["params"]["offset"].as_u64().unwrap();
                seen.entry(run)
                    .or_default()
                    .take(offset, &unb64(&msg["params"]["data_b64"]));
            } else if msg["method"] == json!(methods::RUN_STATUS)
                && msg["params"]["status"] == json!("stopped")
            {
                stopped.insert(msg["params"]["run_id"].as_str().unwrap().to_owned());
            }
        }

        // The watcher: every status, in event_id order, and nothing else.
        let mut statuses = Vec::new();
        while statuses.len() < runs.len() * 3 {
            let Some(msg) = watcher.next().await else {
                panic!("watcher got too few run.status: {statuses:#?}");
            };
            assert_eq!(
                msg["method"],
                json!(methods::RUN_STATUS),
                "watcher got {msg}"
            );
            statuses.push(msg["params"].clone());
        }
        let event_ids: Vec<i64> = statuses
            .iter()
            .map(|s| s["event_id"].as_i64().unwrap())
            .collect();
        assert!(
            event_ids.windows(2).all(|w| w[0] < w[1]),
            "out of order: {event_ids:?}"
        );
        // The watcher said hello before the runs began, so it sees starting, running and stopped for each.
        while let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(300), watcher.next()).await
        {
            statuses.push(msg["params"].clone());
        }
        assert_eq!(statuses.len(), runs.len() * 3, "{statuses:#?}");

        let db = db(&engine);
        for status in &statuses {
            let kind: String = db
                .query_row(
                    "SELECT kind FROM event WHERE id = ?1 AND run_id = ?2",
                    rusqlite::params![
                        status["event_id"].as_i64().unwrap(),
                        status["run_id"].as_str().unwrap()
                    ],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(kind, "run_status_changed");
        }
        for run in &runs {
            let (status, end_reason, log_bytes, pid): (String, String, i64, Option<i64>) = db
                .query_row(
                    "SELECT status, end_reason, log_bytes, pid FROM run WHERE id = ?1",
                    [run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            let file = log_file(&engine, run);
            let saw = &seen[run];
            assert_eq!(
                (status.as_str(), end_reason.as_str(), pid.is_some()),
                ("stopped", "user_stop", true)
            );
            assert_eq!(
                (log_bytes as usize, saw.highest as usize),
                (file.len(), file.len()),
                "{run}"
            );
            assert!(
                file == saw.bytes,
                "{run}: the log differs from what the client saw"
            );
        }
        let counts: (i64, i64, i64) = db
            .query_row(
                "SELECT (SELECT count(*) FROM project WHERE name = 'Dev fixture'), (SELECT count(*) FROM task), (SELECT count(*) FROM workspace WHERE project_id = (SELECT id FROM project) AND state = 'ready')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 1, 3));
        assert_eq!(result.workspace_ids.len(), 3);

        // `event.since` returns each notified event under its id, with the notification's fields.
        let journal = all_events(&mut watcher, json!({})).await;
        for status in &statuses {
            let event_id = status["event_id"].as_i64().unwrap();
            let event = journal
                .iter()
                .find(|e| e["id"] == json!(event_id))
                .unwrap_or_else(|| panic!("event {event_id} is not in event.since"));
            let mut fields = status.clone();
            fields.as_object_mut().unwrap().remove("event_id");
            assert_eq!(
                (&event["kind"], &event["payload"], &event["run_id"]),
                (&json!("run_status_changed"), &fields, &status["run_id"])
            );
        }
        // And each stopped run's log reads back byte for byte.
        for run in &runs {
            assert!(read_all(&mut viewer, run, 4096).await == log_file(&engine, run));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_since_should_default_to_200_cap_at_500_and_and_its_filters() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        // Each call journals a task plus 4 events per run (workspace, run,
        // starting, finished: the script exits at once with no output), so
        // four calls of 32 make over 500 events in all.
        let exits = json!([{ "exit": 0 }]);
        let first = fake_runs(&mut raw, 32, exits.clone()).await;
        let second: Vec<String> = fake_runs(&mut raw, 32, exits.clone())
            .await
            .run_ids
            .into_iter()
            .map(|id| id.0)
            .collect();
        for _ in 0..2 {
            fake_runs(&mut raw, 32, exits.clone()).await;
        }
        wait_no_live_runs(&mut raw).await;
        let page = |limit: Value| json!({ "cursor": 0, "limit": limit });
        let default = raw.call(methods::EVENT_SINCE, page(json!(0))).await;
        let absent = raw.call(methods::EVENT_SINCE, json!({ "cursor": 0 })).await;
        let capped = raw.call(methods::EVENT_SINCE, page(json!(10_000))).await;
        let count = |r: &Value| r["result"]["events"].as_array().unwrap().len();
        assert_eq!(
            (
                count(&default),
                count(&absent),
                count(&capped),
                &capped["result"]["has_more"]
            ),
            (200, 200, 500, &json!(true))
        );
        let ids: Vec<i64> = capped["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_i64().unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(capped["result"]["next_cursor"], json!(ids[ids.len() - 1]));

        let one_run = &first.run_ids[0].0;
        let run_in_task = all_events(
            &mut raw,
            json!({ "task_id": first.task_id.0, "run_id": one_run }),
        )
        .await;
        let mismatched = all_events(
            &mut raw,
            json!({ "task_id": first.task_id.0, "run_id": second[0] }),
        )
        .await;
        let kinds: Vec<&str> = run_in_task
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            (kinds, mismatched.len()),
            (
                vec!["run_created", "run_status_changed", "run_status_changed"],
                0
            )
        );
        assert!(run_in_task.iter().all(|e| e["run_id"] == json!(one_run)
            && e["task_id"] == json!(first.task_id.0)
            && e["project_id"].is_string()
            && e["workspace_id"].is_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_read_should_cap_its_page_and_refuse_a_vanished_log() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 2_000_000, false).await.remove(0);
        tokio::time::sleep(Duration::from_millis(400)).await;
        // Live: what is saved so far reads back, and `eof` means "the end so far".
        let live = raw
            .call(methods::RUN_LOG_READ, json!({ "run_id": run, "offset": 0 }))
            .await;
        assert!(live["result"]["log_bytes"].as_u64().unwrap() > 0, "{live}");
        stop_runs(&mut raw, std::slice::from_ref(&run)).await;

        let file = log_file(&engine, &run);
        assert!(
            file.len() > sy_protocol::MAX_LOG_READ_BYTES,
            "{}",
            file.len()
        );
        let read = |max: Value| json!({ "run_id": run, "offset": 0, "max_bytes": max });
        let default = raw.call(methods::RUN_LOG_READ, read(json!(0))).await;
        let huge = raw.call(methods::RUN_LOG_READ, read(json!(u32::MAX))).await;
        let past_end = raw
            .call(
                methods::RUN_LOG_READ,
                json!({ "run_id": run, "offset": file.len() + 10 }),
            )
            .await;
        let len = |r: &Value| unb64(&r["result"]["data_b64"]).len();
        assert_eq!(
            (len(&default), len(&huge), &default["result"]["eof"]),
            (
                sy_protocol::MAX_LOG_READ_BYTES,
                sy_protocol::MAX_LOG_READ_BYTES,
                &json!(false)
            )
        );
        assert_eq!(
            past_end["result"],
            json!({ "offset": file.len() + 10, "data_b64": "", "eof": true, "log_bytes": file.len() })
        );
        assert!(read_all(&mut raw, &run, 65_536).await == file);

        // The file vanishes between calls: the run had output, so its log is unavailable.
        std::fs::remove_file(
            engine
                .paths
                .data_dir
                .join("runs")
                .join(format!("{run}.log")),
        )
        .unwrap();
        let gone = raw
            .call(methods::RUN_LOG_READ, json!({ "run_id": run, "offset": 0 }))
            .await;
        assert_eq!(
            gone["error"]["code"],
            json!(ErrorCode::RunLogUnavailable.code())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_agent_run_call_should_reuse_the_fixture_project() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        agent_runs(&mut raw, 1, 0, false).await;
        agent_runs(&mut raw, 2, 0, false).await;
        let counts = || -> (i64, i64, i64) {
            db(&engine)
                .query_row(
                    "SELECT (SELECT count(*) FROM project), (SELECT count(*) FROM task), (SELECT count(*) FROM run WHERE kind = 'agent' AND status = 'running')",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };
        // Each run becomes `running` on its first output byte.
        let deadline = Instant::now() + Duration::from_secs(5);
        while counts().2 < 3 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(counts(), (1, 2, 3));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_9_should_lose_nothing_a_client_already_saw() {
        use std::collections::HashMap;
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let runs = agent_runs(&mut raw, 3, 200_000, false).await;
        let mut seen: HashMap<String, Seen> = HashMap::new();
        let mut statuses = Vec::new();
        attach_all(&mut raw, &runs, &mut seen).await;
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            let msg = raw.next().await.unwrap();
            if msg["method"] == json!(methods::RUN_OUTPUT) {
                let run = msg["params"]["run_id"].as_str().unwrap().to_owned();
                let offset = msg["params"]["offset"].as_u64().unwrap();
                seen.entry(run)
                    .or_default()
                    .take(offset, &unb64(&msg["params"]["data_b64"]));
            } else if msg["method"] == json!(methods::RUN_STATUS) {
                statuses.push(msg["params"]["event_id"].as_i64().unwrap());
            }
        }
        engine.child.kill().unwrap(); // SIGKILL
        engine.child.wait().unwrap();

        let db = db(&engine);
        for run in &runs {
            let file = log_file(&engine, run);
            let saw = &seen[run];
            assert!(
                file.len() as u64 >= saw.highest && saw.highest > 0,
                "{run}: log has {} bytes, the client saw up to {}",
                file.len(),
                saw.highest
            );
            assert!(
                file.starts_with(&saw.bytes),
                "{run}: the log differs from what the client saw"
            );
            let status: String = db
                .query_row("SELECT status FROM run WHERE id = ?1", [run], |r| r.get(0))
                .unwrap();
            assert_eq!(status, "running");
        }
        for id in statuses {
            let found: i64 = db
                .query_row("SELECT count(*) FROM event WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(found, 1, "event {id} a client saw is missing");
        }
        drop(db);

        // Restart: recovery marks the runs lost, and their logs read back what the client saw.
        engine.child = spawn_in(&engine.paths.data_dir, &[]);
        engine.wait_for_socket().await;
        let mut again = engine.raw().await;
        again.hello(PROTOCOL.major).await;
        let db = super::devtools::db(&engine);
        for run in &runs {
            let saw = &seen[run];
            let read = read_all(&mut again, run, 65_536).await;
            assert!(
                read.len() as u64 >= saw.highest && read.starts_with(&saw.bytes),
                "{run}: run.log_read differs from what the client saw"
            );
            let (status, end_reason, ended_at, log_bytes): (String, String, i64, i64) = db
                .query_row(
                    "SELECT status, end_reason, ended_at, log_bytes FROM run WHERE id = ?1",
                    [run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(
                (status.as_str(), end_reason.as_str(), log_bytes as usize),
                ("lost", "engine_restart", read.len())
            );
            assert!(ended_at > 0);
            let journal = all_events(&mut again, json!({ "run_id": run })).await;
            let last = journal.last().unwrap();
            assert_eq!(
                (
                    &last["actor"],
                    &last["payload"]["from"],
                    &last["payload"]["status"],
                    &last["payload"]["end_reason"]
                ),
                (
                    &json!("engine"),
                    &json!("running"),
                    &json!("lost"),
                    &json!("engine_restart")
                )
            );
        }
        // The fixture's live agent runs are gone, so new runs start cleanly.
        assert_eq!(agent_runs(&mut again, 1, 0, false).await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_should_sweep_orphan_logs_and_unfinished_backups() {
        let mut engine = Engine::start().await;
        let runs_dir = engine.paths.data_dir.join("runs");
        let orphan = runs_dir.join("run_0193f0a0b1c27d3e8f9a0b1c2d3e4f50.log");
        let not_a_log = runs_dir.join("notes.txt");
        let tmp = engine
            .paths
            .data_dir
            .join("backups")
            .join("switchyard-v1-1.db.tmp");
        std::fs::write(&orphan, "left by a crash").unwrap();
        std::fs::write(&not_a_log, "mine").unwrap();
        std::fs::write(&tmp, "partial").unwrap();
        engine.child.kill().unwrap();
        engine.child.wait().unwrap();
        engine.child = spawn_in(&engine.paths.data_dir, &[]);
        engine.wait_for_socket().await;
        assert_eq!(
            (orphan.exists(), tmp.exists(), not_a_log.exists()),
            (false, false, true)
        );
    }

    /// The run's `log_state`, polled until it is `want` or 5 s pass.
    async fn wait_log_state(engine: &Engine, run: &str, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state: String = db(engine)
                .query_row("SELECT log_state FROM run WHERE id = ?1", [run], |r| {
                    r.get(0)
                })
                .unwrap();
            if state == want || Instant::now() > deadline {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Archive the task a run belongs to, from outside the engine (no
    /// protocol method archives until feature 8).
    fn archive_task_of(engine: &Engine, run: &str) {
        let conn = rusqlite::Connection::open(engine.paths.data_dir.join("switchyard.db")).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute(
            "UPDATE task SET archived_at = 1 WHERE id =
               (SELECT w.task_id FROM run r JOIN workspace w ON w.id = r.workspace_id WHERE r.id = ?1)",
            [run],
        )
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_cap_should_trim_archived_logs_when_a_run_ends_and_at_startup() {
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let first = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        stop_runs(&mut raw, std::slice::from_ref(&first)).await;
        assert!(!log_file(&engine, &first).is_empty());
        // A 1 byte cap, and the first run's task archived.
        let conn = rusqlite::Connection::open(engine.paths.data_dir.join("switchyard.db")).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute(
            "INSERT INTO setting (key, value, updated_at) VALUES ('storage.log_cap_bytes', '1', 1)",
            [],
        )
        .unwrap();
        drop(conn);
        archive_task_of(&engine, &first);

        // A run ending triggers the check: the archived log goes, the active one stays.
        let second = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        stop_runs(&mut raw, std::slice::from_ref(&second)).await;
        assert_eq!(wait_log_state(&engine, &first, "trimmed").await, "trimmed");
        assert!(log_file(&engine, &first).is_empty());
        assert_eq!(wait_log_state(&engine, &second, "present").await, "present");
        assert!(!log_file(&engine, &second).is_empty());
        let trims = all_events(&mut raw, json!({ "run_id": first })).await;
        assert!(
            trims
                .iter()
                .any(|e| e["kind"] == json!("run_log_trimmed") && e["actor"] == json!("engine"))
        );

        // Archived while the engine is down: the startup check trims it.
        drop(raw);
        engine.child.kill().unwrap();
        engine.child.wait().unwrap();
        archive_task_of(&engine, &second);
        engine.child = spawn_in(&engine.paths.data_dir, &[]);
        engine.wait_for_socket().await;
        assert_eq!(wait_log_state(&engine, &second, "trimmed").await, "trimmed");
        assert!(log_file(&engine, &second).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_info_should_count_running_agents() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        agent_runs(&mut raw, 3, 0, false).await;
        let info = raw.call(methods::ENGINE_INFO, json!({})).await;
        assert_eq!(info["result"]["running_agents"], json!(3));
    }

    // ---- disk full (spec 0002, AC-13) and logging (AC-17) ----

    /// An engine whose storage fails on demand: `faults/store` fails every
    /// database write, `faults/run_log` every log append.
    async fn faulty_engine() -> (Engine, tempfile::TempDir) {
        let faults = data_dir();
        let engine =
            Engine::start_with(&[("SWITCHYARD_DEV_FAULTS", faults.path().to_str().unwrap())]).await;
        (engine, faults)
    }

    /// Read until a `storage.status` with `status`, keeping output chunks.
    async fn until_storage(raw: &mut Raw, status: &str, seen: &mut Seen, run: &str) {
        loop {
            let msg = raw
                .next()
                .await
                .unwrap_or_else(|| panic!("no storage.status {status}"));
            if msg["method"] == json!(methods::RUN_OUTPUT) && msg["params"]["run_id"] == json!(run)
            {
                seen.take(
                    msg["params"]["offset"].as_u64().unwrap(),
                    &unb64(&msg["params"]["data_b64"]),
                );
            }
            if msg["method"] == json!(methods::STORAGE_STATUS)
                && msg["params"]["status"] == json!(status)
            {
                return;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disk_full_should_keep_streaming_queue_changes_and_land_them_in_order_after() {
        use std::collections::HashMap;
        let (engine, faults) = faulty_engine().await;
        let mut viewer = engine.raw().await;
        viewer.hello(PROTOCOL.major).await;
        let mut watcher = engine.raw().await; // never attaches
        watcher.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut viewer, 1, 200_000, false).await.remove(0);
        let mut seen = HashMap::new();
        attach_all(&mut viewer, std::slice::from_ref(&run), &mut seen).await;
        let mut seen = seen.remove(&run).unwrap();
        while seen.bytes.len() < 20_000 {
            let msg = viewer.next().await.unwrap();
            if msg["method"] == json!(methods::RUN_OUTPUT) {
                seen.take(
                    msg["params"]["offset"].as_u64().unwrap(),
                    &unb64(&msg["params"]["data_b64"]),
                );
            }
        }

        std::fs::write(faults.path().join("store"), b"").unwrap();
        std::fs::write(faults.path().join("run_log"), b"").unwrap();
        until_storage(&mut viewer, "degraded_writes", &mut seen, &run).await;
        let at_failure = seen.bytes.len();
        while seen.bytes.len() < at_failure + 20_000 {
            let msg = viewer.next().await.expect("output stopped streaming");
            if msg["method"] == json!(methods::RUN_OUTPUT) {
                seen.take(
                    msg["params"]["offset"].as_u64().unwrap(),
                    &unb64(&msg["params"]["data_b64"]),
                );
            }
        }
        let create = viewer
            .call(methods::DEV_AGENT_RUN, json!({ "agent_id": "fake" }))
            .await;
        let info = storage_of(&mut viewer).await;
        stop_runs(&mut viewer, std::slice::from_ref(&run)).await;

        // The watcher saw degraded, then the stop with no journal id.
        let mut statuses = Vec::new();
        loop {
            let msg = watcher.next().await.expect("watcher never saw storage ok");
            if msg["method"] == json!(methods::RUN_STATUS) {
                // Journaled ones carry their id; the one queued while degraded carries null.
                statuses.push((
                    msg["params"]["status"].clone(),
                    msg["params"]["event_id"].is_i64(),
                ));
            }
            if msg["method"] == json!(methods::STORAGE_STATUS)
                && msg["params"]["status"] == json!("degraded_writes")
            {
                // Room again: the retry lands the queue and the probe passes.
                std::fs::remove_file(faults.path().join("store")).ok();
                std::fs::remove_file(faults.path().join("run_log")).ok();
            }
            if msg["method"] == json!(methods::STORAGE_STATUS)
                && msg["params"]["status"] == json!("ok")
            {
                break;
            }
        }
        // The replayed commit is not announced a second time.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let file = log_file(&engine, &run);
        let (status, log_state, log_bytes): (String, String, i64) = db(&engine)
            .query_row(
                "SELECT status, log_state, log_bytes FROM run WHERE id = ?1",
                [&run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let journaled: Vec<Value> = all_events(&mut viewer, json!({ "run_id": run }))
            .await
            .into_iter()
            .filter(|e| e["kind"] == json!("run_status_changed"))
            .map(|e| e["payload"]["status"].clone())
            .collect();
        assert_eq!(
            (
                create["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("disk is full"),
                info["status"].clone(),
                statuses,
                status.as_str(),
                log_state.as_str(),
                log_bytes == file.len() as i64,
                journaled,
            ),
            (
                true,
                json!("degraded_writes"),
                vec![
                    (json!("starting"), true),
                    (json!("running"), true),
                    (json!("stopped"), false)
                ],
                "stopped",
                "incomplete",
                true,
                vec![json!("starting"), json!("running"), json!("stopped")],
            )
        );
        assert!(
            !file.is_empty() && file.len() < seen.bytes.len() && seen.bytes.starts_with(&file),
            "the log should be the good prefix: {} of {} bytes",
            file.len(),
            seen.bytes.len()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_log_only_failure_should_recover_through_the_probe_and_leave_the_log_incomplete() {
        let (engine, faults) = faulty_engine().await;
        let mut viewer = engine.raw().await;
        viewer.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut viewer, 1, 100_000, false).await.remove(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(faults.path().join("run_log"), b"").unwrap();
        // Only waiting here, not reassembling.
        let mut seen = Seen {
            cursor: u64::MAX,
            ..Seen::default()
        };
        until_storage(&mut viewer, "degraded_writes", &mut seen, "-").await;
        // The database takes writes, so the next retry's probe passes with the log still failing.
        until_storage(&mut viewer, "ok", &mut seen, "-").await;
        let stop_id = viewer
            .send(methods::RUN_STOP, json!({ "run_id": run }))
            .await;
        // The broadcast can land just after the response, so wait for both.
        let (mut answered, mut event_id) = (false, None);
        while !answered || event_id.is_none() {
            let msg = viewer.next().await.expect("no stopped run.status");
            if msg["method"] == json!(methods::RUN_STATUS)
                && msg["params"]["status"] == json!("stopped")
            {
                event_id = Some(msg["params"]["event_id"].clone());
            }
            answered |= msg["id"] == json!(stop_id);
        }
        let (log_state, log_bytes): (String, i64) = db(&engine)
            .query_row(
                "SELECT log_state, log_bytes FROM run WHERE id = ?1",
                [&run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (
                log_state.as_str(),
                log_bytes == log_file(&engine, &run).len() as i64,
                event_id.is_some_and(|id| id.is_i64())
            ),
            ("incomplete", true, true)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_log_should_never_hold_output_or_input_bytes() {
        const MARKER: &[u8] = b"sy-secret-marker-7f3a";
        let mut engine = Engine::start_with(&[("SWITCHYARD_LOG", "trace")]).await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, true).await.remove(0);
        raw.call(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(MARKER) }),
        )
        .await;
        loop {
            let msg = raw.next().await.unwrap();
            if msg["method"] == json!(methods::RUN_OUTPUT)
                && unb64(&msg["params"]["data_b64"]).ends_with(MARKER)
            {
                break;
            }
        }
        stop_runs(&mut raw, std::slice::from_ref(&run)).await;
        drop(raw);
        stop(&mut engine).await;
        let log = engine.log();
        let marker = std::str::from_utf8(MARKER).unwrap();
        assert!(!log.is_empty(), "trace logging wrote nothing");
        assert!(
            !log.contains(marker) && !log.contains(&b64(MARKER)),
            "engine.log holds run bytes"
        );
    }

    // ---- the fake agent through a real PTY (spec 0003, milestone 1) ----

    /// Every `run.status` of `runs`, read until each one ended, by run.
    async fn until_ended(raw: &mut Raw, runs: &[String]) -> HashMap<String, Vec<Value>> {
        let mut statuses: HashMap<String, Vec<Value>> = HashMap::new();
        let ended = |s: &HashMap<String, Vec<Value>>| {
            runs.iter().all(|r| {
                s.get(r).and_then(|v| v.last()).is_some_and(|last| {
                    ["finished", "failed", "stopped"]
                        .contains(&last["status"].as_str().unwrap_or(""))
                })
            })
        };
        while !ended(&statuses) {
            let msg = raw.next().await.expect("closed before the runs ended");
            if msg["method"] == json!(methods::RUN_STATUS) {
                let run = msg["params"]["run_id"].as_str().unwrap().to_owned();
                if runs.contains(&run) {
                    statuses.entry(run).or_default().push(msg["params"].clone());
                }
            }
        }
        statuses
    }

    fn status_names(statuses: &[Value]) -> Vec<&str> {
        statuses
            .iter()
            .map(|s| s["status"].as_str().unwrap())
            .collect()
    }

    /// (status, end_reason, exit_code, log_bytes) of a run row.
    fn row(engine: &Engine, run: &str) -> (String, String, Option<i64>, i64) {
        let (status, reason, code, bytes): (String, Option<String>, Option<i64>, i64) = db(engine)
            .query_row(
                "SELECT status, end_reason, exit_code, log_bytes FROM run WHERE id = ?1",
                [run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        (status, reason.unwrap_or_default(), code, bytes)
    }

    /// Wait (up to 5 s) until the run is `running`: the fake is past its terminal setup.
    async fn until_running(engine: &Engine, run: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while row(engine, run).0 != "running" {
            assert!(Instant::now() < deadline, "{run} never ran");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn launch_dirs(engine: &Engine) -> usize {
        std::fs::read_dir(engine.paths.data_dir.join("agent-runtime")).map_or(0, Iterator::count)
    }

    use std::collections::HashMap;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fake_run_should_print_write_a_file_finish_and_leave_no_launch_dir() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let result = fake_runs(
            &mut raw,
            1,
            json!([
                { "print": "hello from the fake\r\n" },
                { "write_file": { "path": "out.txt", "contents": "made by the fake" } },
                { "exit": 0 }
            ]),
        )
        .await;
        let run = result.run_ids[0].0.clone();
        let statuses = until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let worktree: String = db(&engine)
            .query_row(
                "SELECT worktree_path FROM workspace WHERE id = ?1",
                [&result.workspace_ids[0].0],
                |r| r.get(0),
            )
            .unwrap();
        let log = log_file(&engine, &run);
        assert_eq!(
            (
                status_names(&statuses[&run]),
                row(&engine, &run),
                String::from_utf8_lossy(&log).into_owned(),
                std::fs::read_to_string(Path::new(&worktree).join("out.txt")).unwrap(),
                launch_dirs(&engine),
            ),
            (
                vec!["starting", "running", "finished"],
                (
                    "finished".into(),
                    "exited".into(),
                    Some(0),
                    log.len() as i64
                ),
                "hello from the fake\r\n".to_owned(),
                "made by the fake".to_owned(),
                0,
            )
        );
        let mode = std::fs::metadata(&worktree).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exit_codes_and_signals_should_classify_the_end() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let mut runs = Vec::new();
        for steps in [
            json!([{ "exit": 3 }]),
            json!([{ "print": "x" }, { "die_by_signal": "SIGSEGV" }]),
            json!([{ "die_by_signal": "SIGKILL" }]),
        ] {
            runs.push(fake_runs(&mut raw, 1, steps).await.run_ids[0].0.clone());
        }
        until_ended(&mut raw, &runs).await;
        let ends: Vec<_> = runs.iter().map(|r| row(&engine, r)).collect();
        assert_eq!(
            ends.iter()
                .map(|(s, e, c, _)| (s.as_str(), e.as_str(), *c))
                .collect::<Vec<_>>(),
            [
                ("failed", "exited", Some(3)),
                ("failed", "exited", None),
                ("failed", "exited", None)
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_script_should_fail_each_run_with_its_reason_in_the_log() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let result = fake_runs(&mut raw, 2, json!([{ "nope": 1 }])).await;
        let runs: Vec<String> = result.run_ids.iter().map(|r| r.0.clone()).collect();
        let statuses = until_ended(&mut raw, &runs).await;
        for run in &runs {
            let log = String::from_utf8_lossy(&log_file(&engine, run)).into_owned();
            let (status, reason, code, log_bytes) = row(&engine, run);
            assert_eq!(
                (
                    status_names(&statuses[run]),
                    status.as_str(),
                    reason.as_str(),
                    code,
                    log_bytes as usize
                ),
                (
                    vec!["starting", "failed"],
                    "failed",
                    "spawn_failed",
                    None,
                    log.len()
                )
            );
            assert!(
                log.starts_with("switchyard: invalid agent options") && log.ends_with("\r\n"),
                "{log:?}"
            );
        }
        assert_eq!(launch_dirs(&engine), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_agent_program_should_fail_the_run_with_spawn_failed() {
        let helpers = data_dir(); // empty: no sy-fake-agent here
        let engine =
            Engine::start_with(&[("SWITCHYARD_HELPER_DIR", helpers.path().to_str().unwrap())])
                .await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let log = String::from_utf8_lossy(&log_file(&engine, &run)).into_owned();
        assert_eq!(
            (
                row(&engine, &run).1.as_str(),
                log.contains("could not start the agent")
            ),
            ("spawn_failed", true),
            "{log:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_or_detection_only_agents_should_be_refused_before_any_row() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let unknown = raw
            .call(methods::DEV_AGENT_RUN, json!({ "agent_id": "nobody" }))
            .await;
        let claude = raw
            .call(methods::DEV_AGENT_RUN, json!({ "agent_id": "claude" }))
            .await;
        let too_many = raw
            .call(
                methods::DEV_AGENT_RUN,
                json!({ "agent_id": "fake", "count": 33 }),
            )
            .await;
        let rows: (i64, i64) = db(&engine)
            .query_row(
                "SELECT (SELECT count(*) FROM task), (SELECT count(*) FROM run)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (
                &unknown["error"]["code"],
                &claude["error"]["code"],
                &too_many["error"]["code"],
                rows
            ),
            (
                &json!(ErrorCode::AgentNotFound.code()),
                &json!(ErrorCode::AgentLaunchUnsupported.code()),
                &json!(ErrorCode::InvalidParams.code()),
                (0, 0)
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_stop_should_ctrl_c_the_fake_and_refuse_ended_or_unknown_runs() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        until_running(&engine, &run).await;
        let stop = raw.call(methods::RUN_STOP, json!({ "run_id": run })).await;
        let statuses = until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let again = raw.call(methods::RUN_STOP, json!({ "run_id": run })).await;
        let unknown = raw
            .call(
                methods::RUN_STOP,
                json!({ "run_id": "run_0193f0a0b1c27d3e8f9a0b1c2d3e4f50" }),
            )
            .await;
        let (status, reason, code, _) = row(&engine, &run);
        assert_eq!(
            (
                &stop["result"],
                statuses[&run].last().unwrap()["status"].as_str(),
                status.as_str(),
                reason.as_str(),
                code,
                &again["error"]["code"],
                &unknown["error"]["code"],
            ),
            (
                &json!({}),
                Some("stopped"),
                "stopped",
                "user_stop",
                Some(130),
                &json!(ErrorCode::RunNotLive.code()),
                &json!(ErrorCode::RunNotFound.code()),
            )
        );
    }

    /// A fake that prints, then ignores Ctrl C (it reads no input) and SIGHUP.
    fn stubborn() -> Value {
        json!([{ "print": "ready\r\n" }, { "ignore_sighup": true }, { "hang": true }])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_ignoring_ctrl_c_and_sighup_should_get_sigkill_after_about_10_s() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = fake_runs(&mut raw, 1, stubborn()).await.run_ids[0]
            .0
            .clone();
        // Let it reach its hang (its first output means it is past the setup).
        until_running(&engine, &run).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        raw.call(methods::RUN_STOP, json!({ "run_id": run })).await;
        let mut ended = None;
        while ended.is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the run never ended"
            );
            // The stop is quiet for about 10 s, longer than one `next` waits.
            let Some(msg) = raw.next().await else {
                continue;
            };
            if msg["method"] == json!(methods::RUN_STATUS)
                && msg["params"]["status"] == json!("stopped")
            {
                ended = Some(started.elapsed());
            }
        }
        let took = ended.unwrap();
        let (_, reason, code, _) = row(&engine, &run);
        assert!(
            took >= Duration::from_millis(9_500) && took < Duration::from_secs(13),
            "{took:?}"
        );
        assert_eq!((reason.as_str(), code), ("user_stop", None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_should_stop_every_run_within_about_8_s() {
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let runs: Vec<String> = fake_runs(&mut raw, 2, stubborn())
            .await
            .run_ids
            .into_iter()
            .map(|r| r.0)
            .collect();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = Instant::now();
        raw.call(methods::ENGINE_SHUTDOWN, json!({})).await;
        let exited = engine.wait_exit(Duration::from_secs(15));
        let took = started.elapsed();
        eprintln!("1500 hook calls took {took:?}");
        assert!(exited.is_some_and(|s| s.success()), "{exited:?}");
        assert!(took < Duration::from_millis(10_500), "{took:?}");
        for run in &runs {
            let (status, reason, _, _) = row(&engine, run);
            assert_eq!((status.as_str(), reason.as_str()), ("stopped", "user_stop"));
        }
        assert_eq!(launch_dirs(&engine), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_should_sweep_launch_dirs_left_by_a_crash() {
        let mut engine = Engine::start().await;
        let left = engine
            .paths
            .data_dir
            .join("agent-runtime")
            .join("run_0193f0a0b1c27d3e8f9a0b1c2d3e4f50");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::write(left.join("script.json"), "[]").unwrap();
        engine.child.kill().unwrap();
        engine.child.wait().unwrap();
        restart(&mut engine).await;
        assert!(!left.exists());
    }

    // ---- the signal side channel (spec 0003, milestone 2) ----

    /// The `agent_session_ref` of a run row.
    fn session_ref(engine: &Engine, run: &str) -> Option<String> {
        db(engine)
            .query_row(
                "SELECT agent_session_ref FROM run WHERE id = ?1",
                [run],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The run's journal events other than its status changes, as
    /// (kind, actor, actor_ref, payload).
    async fn agent_events(raw: &mut Raw, run: &str) -> Vec<(Value, Value, Value, Value)> {
        all_events(raw, json!({ "run_id": run }))
            .await
            .into_iter()
            .filter(|e| {
                e["kind"] != json!("run_status_changed") && e["kind"] != json!("run_created")
            })
            .map(|e| {
                (
                    e["kind"].clone(),
                    e["actor"].clone(),
                    e["actor_ref"].clone(),
                    e["payload"].clone(),
                )
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_steps_should_set_status_journal_agent_events_and_save_the_last_session() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let result = fake_runs(
            &mut raw,
            1,
            json!([
                { "print": "hi\r\n" },
                { "status": "waiting_input" },
                { "session": "s-1" },
                { "event": { "message": { "role": "agent", "text": "hello" } } },
                { "event": { "command_run": { "argv": ["cargo", "test"], "exit_code": 1 } } },
                { "event": { "file_changed": { "path": "b.txt", "change": "renamed", "from_path": "a.txt" } } },
                { "session": "s-2" },
                { "status": "running" },
                { "exit": 0 }
            ]),
        )
        .await;
        let (run, workspace) = (
            result.run_ids[0].0.clone(),
            json!(result.workspace_ids[0].0),
        );
        let statuses = until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let agent = json!("agent");
        assert_eq!(
            (
                status_names(&statuses[&run]),
                session_ref(&engine, &run),
                agent_events(&mut raw, &run).await,
            ),
            (
                vec![
                    "starting",
                    "running",
                    "waiting_input",
                    "running",
                    "finished"
                ],
                Some("s-2".to_owned()),
                vec![
                    (
                        json!("message"),
                        agent.clone(),
                        workspace.clone(),
                        json!({ "role": "agent", "text": "hello" })
                    ),
                    (
                        json!("command_run"),
                        agent.clone(),
                        workspace.clone(),
                        json!({ "argv": ["cargo", "test"], "exit_code": 1 })
                    ),
                    (
                        json!("file_changed"),
                        agent,
                        workspace,
                        json!({ "path": "b.txt", "change": "renamed", "from_path": "a.txt" })
                    ),
                ]
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_pattern_on_screen_should_mean_waiting_input_until_new_output() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let result = raw
            .call(
                methods::DEV_AGENT_RUN,
                json!({ "agent_id": "fake", "options": {
                    "waiting_pattern": "[y/n]",
                    "script": [
                        { "print": "working\r\nAllow \u{1b}[1mrm\u{1b}[0m? [y/n] " },
                        { "expect_input": "y" },
                        { "print": "\r\nremoving\r\n" },
                        { "expect_input": "q" },
                    ],
                } }),
            )
            .await;
        let run = result["result"]["run_ids"][0].as_str().unwrap().to_owned();
        let deadline = Instant::now() + Duration::from_secs(5);
        while row(&engine, &run).0 != "waiting_input" {
            assert!(Instant::now() < deadline, "never waiting: {result}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(b"y") }),
        )
        .await;
        until_running(&engine, &run).await;
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(b"q") }),
        )
        .await;
        let statuses = until_ended(&mut raw, std::slice::from_ref(&run)).await;
        assert_eq!(
            status_names(&statuses[&run]),
            [
                "starting",
                "running",
                "waiting_input",
                "running",
                "finished"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hook_payload_over_256_kib_should_fall_back_and_journal_nothing() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let big = "x".repeat(300 * 1024);
        let result = fake_runs(
            &mut raw,
            1,
            json!([
                { "event": { "message": { "role": "agent", "text": big } } },
                { "event": { "message": { "role": "agent", "text": "small" } } },
                { "exit": 0 }
            ]),
        )
        .await;
        let run = result.run_ids[0].0.clone();
        until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let events = agent_events(&mut raw, &run).await;
        assert_eq!(
            (
                row(&engine, &run).0,
                events
                    .iter()
                    .map(|e| e.3["text"].clone())
                    .collect::<Vec<_>>()
            ),
            ("finished".to_owned(), vec![json!("small")])
        );
    }

    /// The next message within `within`, if any.
    async fn next_within(raw: &mut Raw, within: Duration) -> Option<Value> {
        tokio::time::timeout(within, raw.next())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signal_connection_should_only_relay_hooks_and_get_no_notifications() {
        let engine = Engine::start().await;
        let mut signal = engine.raw().await;
        let before = signal
            .call(
                methods::SIGNAL_HOOK,
                json!({ "token": "t", "hook": "status" }),
            )
            .await;
        signal.hello_as("signal", PROTOCOL.major).await;
        let info = signal.call(methods::ENGINE_INFO, json!({})).await;
        let attach = signal
            .call(methods::RUN_ATTACH, json!({ "run_id": "run_x" }))
            .await;
        let unknown = signal
            .call(
                methods::SIGNAL_HOOK,
                json!({ "token": "0".repeat(32), "hook": "status", "args": [], "stdin_b64": "" }),
            )
            .await;
        let too_big = signal
            .call(
                methods::SIGNAL_HOOK,
                json!({
                    "token": "0".repeat(32),
                    "hook": "event",
                    "args": ["ab"],
                    "stdin_b64": b64(&vec![b'x'; sy_protocol::MAX_SIGNAL_PAYLOAD_BYTES - 1]),
                }),
            )
            .await;

        // Another client's run: its status changes reach that client, not this one.
        let mut other = engine.raw().await;
        other.hello(PROTOCOL.major).await;
        let not_signal = other
            .call(
                methods::SIGNAL_HOOK,
                json!({ "token": "t", "hook": "status" }),
            )
            .await;
        let run = fake_runs(&mut other, 1, json!([{ "print": "x" }, { "exit": 0 }]))
            .await
            .run_ids[0]
            .0
            .clone();
        until_ended(&mut other, std::slice::from_ref(&run)).await;
        let code = |reply: &Value| reply["error"]["code"].clone();
        assert_eq!(
            (
                code(&before),
                info["result"]["pid"].is_number(),
                code(&attach),
                code(&unknown),
                code(&too_big),
                code(&not_signal),
                next_within(&mut signal, Duration::from_millis(300)).await,
            ),
            (
                json!(ErrorCode::HelloRequired.code()),
                true,
                json!(ErrorCode::NotAllowedForClient.code()),
                json!(ErrorCode::SignalTokenUnknown.code()),
                json!(ErrorCode::SignalPayloadTooLarge.code()),
                json!(ErrorCode::NotAllowedForClient.code()),
                None,
            )
        );
    }

    /// A live fake run's signal token, read from its argv (visible to your own
    /// processes through `ps`, which spec 0003's security model accepts).
    fn fake_token(engine: &Engine, run: &str) -> String {
        let pid: i64 = db(engine)
            .query_row("SELECT pid FROM run WHERE id = ?1", [run], |r| r.get(0))
            .unwrap();
        let out = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let command = String::from_utf8(out.stdout).unwrap();
        let mut words = command.split_whitespace();
        words.find(|w| *w == "--token");
        words
            .next()
            .expect("no --token in the fake's argv")
            .to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_calls_past_the_burst_should_be_rate_limited_with_one_warning() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        until_running(&engine, &run).await;
        let token = fake_token(&engine, &run);
        let mut signal = engine.raw().await;
        signal.hello_as("signal", PROTOCOL.major).await;
        let (mut answered, mut limited) = (0, 0);
        let started = Instant::now();
        for _ in 0..700 {
            let reply = signal
                .call(
                    methods::SIGNAL_HOOK,
                    json!({ "token": token, "hook": "status", "stdin_b64": b64(br#""running""#) }),
                )
                .await;
            if reply["result"]["exit_code"] == json!(0) {
                answered += 1;
            } else if reply["error"]["code"] == json!(ErrorCode::SignalRateLimited.code()) {
                limited += 1;
            } else {
                panic!("{reply}");
            }
        }
        // The bucket refills 100 a second while the calls go out.
        let refill = (started.elapsed().as_millis() / 10) as u32 + 1;
        // A held call is never charged: it still holds with the bucket empty.
        let ask = br#"{"kind":"other","summary":"after the burst","detail":{"tool":"t"}}"#;
        signal
            .send(
                methods::SIGNAL_HOOK,
                json!({ "token": token, "hook": "approval", "stdin_b64": b64(ask) }),
            )
            .await;
        let (held, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        assert_eq!(held["approval"]["summary"], json!("after the burst"));
        stop_runs(&mut raw, std::slice::from_ref(&run)).await;
        let warnings = engine.log().matches("over their rate limit").count();
        assert!(
            (500..=500 + refill).contains(&answered) && answered + limited == 700,
            "{answered} answered, {limited} limited, refill up to {refill}"
        );
        assert_eq!(warnings, 1);
    }

    // ---- approvals (spec 0003, milestone 3) ----

    /// An `approval` step: approve prints `approved` and exits 0; deny
    /// prints `denied` and exits 4.
    fn ask(summary: &str) -> Value {
        json!({ "approval": {
            "kind": "command",
            "summary": summary,
            "detail": { "command": "rm -rf build", "cwd": "/w" },
            "on_approve": [{ "print": "approved\r\n" }, { "exit": 0 }],
            "on_deny": [{ "print": "denied\r\n" }, { "exit": 4 }],
        } })
    }

    /// Read until a `method` notification (for `run` when given); returns its
    /// params and every message seen before it.
    async fn until_note(raw: &mut Raw, method: &str, run: Option<&str>) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let msg = raw
                .next()
                .await
                .unwrap_or_else(|| panic!("no {method}; saw {seen:?}"));
            let for_run = run.is_none_or(|r| {
                msg["params"]["run_id"] == json!(r)
                    || msg["params"]["approval"]["run_id"] == json!(r)
            });
            if msg["method"] == json!(method) && for_run {
                return (msg["params"].clone(), seen);
            }
            seen.push(msg);
        }
    }

    /// Attach and read the run's output until it holds `needle`.
    async fn until_output(raw: &mut Raw, run: &str, needle: &str) {
        let reply = raw
            .call(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;
        let mut text = String::from_utf8_lossy(&unb64(&reply["result"]["replay_b64"])).into_owned();
        while !text.contains(needle) {
            let msg = raw
                .next()
                .await
                .unwrap_or_else(|| panic!("no {needle:?} in {text:?}"));
            if msg["method"] == json!(methods::RUN_OUTPUT) && msg["params"]["run_id"] == json!(run)
            {
                text.push_str(&String::from_utf8_lossy(&unb64(&msg["params"]["data_b64"])));
            }
        }
    }

    fn approval_status(engine: &Engine, id: &Value) -> String {
        db(engine)
            .query_row(
                "SELECT status FROM approval WHERE id = ?1",
                [id.as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap()
    }

    async fn respond(raw: &mut Raw, id: &Value, decision: &str, message: Option<&str>) -> Value {
        let mut params = json!({ "approval_id": id, "decision": decision });
        if let Some(m) = message {
            params["message"] = json!(m);
        }
        raw.call(methods::APPROVAL_RESPOND, params).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_approval_should_hold_the_fake_until_answered_then_run_that_branch() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let mut ends = Vec::new();
        for (decision, message) in [("approve", None), ("deny", Some("not today"))] {
            let result = fake_runs(
                &mut raw,
                1,
                json!([{ "print": "start\r\n" }, ask("clean build")]),
            )
            .await;
            let (run, workspace) = (
                result.run_ids[0].0.clone(),
                result.workspace_ids[0].0.clone(),
            );
            let (requested, _) =
                until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
            let id = requested["approval"]["id"].clone();
            let (waiting, _) = until_note(&mut raw, methods::RUN_STATUS, Some(&run)).await;
            let waiting = if waiting["status"] == json!("running") {
                until_note(&mut raw, methods::RUN_STATUS, Some(&run))
                    .await
                    .0
            } else {
                waiting
            };
            let answer = respond(&mut raw, &id, decision, message).await;
            let (decided, _) = until_note(&mut raw, methods::APPROVAL_DECIDED, Some(&run)).await;
            until_ended(&mut raw, std::slice::from_ref(&run)).await;
            let a = &requested["approval"];
            assert_eq!(
                (
                    (&a["kind"], &a["status"], &a["summary"], &a["workspace_id"]),
                    (&a["detail"]["command"], &a["detail"]["cwd"]),
                    requested["event_id"].is_i64(),
                    &waiting["status"],
                ),
                (
                    (
                        &json!("command"),
                        &json!("pending"),
                        &json!("clean build"),
                        &json!(workspace)
                    ),
                    (&json!("rm -rf build"), &json!("/w")),
                    true,
                    &json!("waiting_input"),
                ),
                "{requested}"
            );
            let status = if decision == "approve" {
                "approved"
            } else {
                "denied"
            };
            assert_eq!(
                (
                    &answer["result"]["approval"]["status"],
                    &answer["result"]["approval"]["decided_by"],
                    &answer["result"]["approval"]["decided_by_ref"],
                    &decided["approval"]["status"],
                    &decided["approval"]["message"],
                    decided["event_id"].is_i64(),
                ),
                (
                    &json!(status),
                    &json!("user"),
                    &json!("inbox"),
                    &json!(status),
                    &message.map_or(Value::Null, |m| json!(m)),
                    true,
                ),
                "{answer}"
            );
            let again = respond(&mut raw, &id, "approve", None).await;
            assert_eq!(
                again["error"]["code"],
                json!(ErrorCode::ApprovalNotPending.code())
            );
            ends.push((
                row(&engine, &run),
                String::from_utf8_lossy(&log_file(&engine, &run)).into_owned(),
            ));
        }
        assert_eq!(
            ends.iter()
                .map(|((s, _, c, _), log)| (
                    s.as_str(),
                    *c,
                    log.contains("approved"),
                    log.contains("denied")
                ))
                .collect::<Vec<_>>(),
            [
                ("finished", Some(0), true, false),
                ("failed", Some(4), false, true)
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_held_approval_whose_helper_dies_should_expire_and_the_fake_should_ask_itself() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = fake_runs(&mut raw, 1, json!([ask("deploy")])).await.run_ids[0]
            .0
            .clone();
        let (requested, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let id = requested["approval"]["id"].clone();
        let pid: i64 = db(&engine)
            .query_row("SELECT pid FROM run WHERE id = ?1", [&run], |r| r.get(0))
            .unwrap();
        // The fake's only child is its `sy-signal`.
        let killed = Command::new("pkill")
            .args(["-KILL", "-P", &pid.to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let (expired, _) = until_note(&mut raw, methods::APPROVAL_DECIDED, Some(&run)).await;
        until_output(&mut raw, &run, "Allow deploy? [y/n] ").await;
        let late = respond(&mut raw, &id, "approve", None).await;
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(b"y\r") }),
        )
        .await;
        until_ended(&mut raw, std::slice::from_ref(&run)).await;
        assert_eq!(
            (
                &expired["approval"]["status"],
                &expired["approval"].get("decided_by"),
                &late["error"]["code"],
                row(&engine, &run).0,
                String::from_utf8_lossy(&log_file(&engine, &run)).contains("approved"),
            ),
            (
                &json!("expired"),
                &None,
                &json!(ErrorCode::ApprovalNotPending.code()),
                "finished".to_owned(),
                true,
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_approval_the_engine_cannot_take_should_fall_back_to_the_agents_own_prompt() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        // Over 256 KiB: `sy-signal` answers with its fallback without calling.
        let huge = "z".repeat(300 * 1024);
        let run = fake_runs(&mut raw, 1, json!([ask(&huge)])).await.run_ids[0]
            .0
            .clone();
        until_output(&mut raw, &run, "? [y/n] ").await;
        raw.call(
            methods::RUN_INPUT,
            json!({ "run_id": run, "data_b64": b64(b"n\r") }),
        )
        .await;
        until_ended(&mut raw, std::slice::from_ref(&run)).await;
        let rows: i64 = db(&engine)
            .query_row("SELECT count(*) FROM approval", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            (row(&engine, &run).0, row(&engine, &run).2, rows),
            ("failed".to_owned(), Some(4), 0)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_a_run_should_deny_and_cancel_its_held_approval_before_it_ends() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let steps = json!([{ "approval": {
            "kind": "fetch", "summary": "fetch it", "detail": { "url": "https://example.com" },
            "on_deny": [{ "print": "told no\r\n" }, { "expect_input": NEVER }],
        } }]);
        let run = fake_runs(&mut raw, 1, steps).await.run_ids[0].0.clone();
        let (requested, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let stop = raw.send(methods::RUN_STOP, json!({ "run_id": run })).await;
        let (cancelled, _) = until_note(&mut raw, methods::APPROVAL_DECIDED, Some(&run)).await;
        let (ended, before) = loop {
            let (status, before) = until_note(&mut raw, methods::RUN_STATUS, Some(&run)).await;
            if status["status"] == json!("stopped") {
                break (status, before);
            }
        };
        assert!(
            before
                .iter()
                .all(|m| m["id"] != json!(stop) || m.get("error").is_none())
        );
        assert_eq!(
            (
                &cancelled["approval"]["id"],
                &cancelled["approval"]["status"],
                &cancelled["approval"]["message"],
                &ended["end_reason"],
                approval_status(&engine, &requested["approval"]["id"]),
            ),
            (
                &requested["approval"]["id"],
                &json!("cancelled"),
                &json!("run stopped"),
                &json!("user_stop"),
                "cancelled".to_owned(),
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_that_dies_holding_an_approval_should_cancel_it_before_its_end_and_revoke_the_token()
     {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = fake_runs(&mut raw, 1, json!([ask("held")])).await.run_ids[0]
            .0
            .clone();
        until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let token = fake_token(&engine, &run);
        let pid: i64 = db(&engine)
            .query_row("SELECT pid FROM run WHERE id = ?1", [&run], |r| r.get(0))
            .unwrap();
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap();
        let (decided, _) = until_note(&mut raw, methods::APPROVAL_DECIDED, Some(&run)).await;
        let (status, _) = loop {
            let (status, seen) = until_note(&mut raw, methods::RUN_STATUS, Some(&run)).await;
            if status["status"] == json!("failed") {
                break (status, seen);
            }
        };
        let mut signal = engine.raw().await;
        signal.hello_as("signal", PROTOCOL.major).await;
        let after = signal
            .call(
                methods::SIGNAL_HOOK,
                json!({ "token": token, "hook": "status", "stdin_b64": b64(br#""running""#) }),
            )
            .await;
        assert_eq!(
            (
                &decided["approval"]["status"],
                &status["end_reason"],
                &after["error"]["code"],
                launch_dirs(&engine),
            ),
            (
                &json!("cancelled"),
                &json!("exited"),
                &json!(ErrorCode::SignalTokenUnknown.code()),
                0,
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_approval_should_be_expired_after_an_engine_restart() {
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = fake_runs(&mut raw, 1, json!([ask("left behind")]))
            .await
            .run_ids[0]
            .0
            .clone();
        let (requested, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let id = requested["approval"]["id"].clone();
        engine.child.kill().unwrap();
        engine.child.wait().unwrap();
        restart(&mut engine).await;
        let mut again = engine.raw().await;
        again.hello(PROTOCOL.major).await;
        let late = respond(&mut again, &id, "approve", None).await;
        let expired = again
            .call(methods::APPROVAL_LIST, json!({ "status": "expired" }))
            .await;
        assert_eq!(
            (
                approval_status(&engine, &id),
                &late["error"]["code"],
                &expired["result"]["approvals"][0]["id"],
            ),
            (
                "expired".to_owned(),
                &json!(ErrorCode::ApprovalNotPending.code()),
                &id,
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approval_list_should_page_pending_filter_by_run_and_check_its_params() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let runs: Vec<String> = fake_runs(&mut raw, 3, json!([ask("one of three")]))
            .await
            .run_ids
            .into_iter()
            .map(|r| r.0)
            .collect();
        for _ in 0..3 {
            until_note(&mut raw, methods::APPROVAL_REQUESTED, None).await;
        }
        let first = raw
            .call(methods::APPROVAL_LIST, json!({ "limit": 2 }))
            .await;
        let second = raw
            .call(
                methods::APPROVAL_LIST,
                json!({ "limit": 2, "after": first["result"]["next"] }),
            )
            .await;
        let of_run = raw
            .call(methods::APPROVAL_LIST, json!({ "run_id": runs[1] }))
            .await;
        let none = raw
            .call(methods::APPROVAL_LIST, json!({ "status": "denied" }))
            .await;
        let bad_status = raw
            .call(methods::APPROVAL_LIST, json!({ "status": "asleep" }))
            .await;
        let bad_after = raw
            .call(methods::APPROVAL_LIST, json!({ "after": { "id": "x" } }))
            .await;
        let id = &first["result"]["approvals"][0]["id"];
        let long = respond(&mut raw, id, "deny", Some(&"m".repeat(4097))).await;
        let unknown = respond(&mut raw, &json!("apr_0"), "approve", None).await;
        let count = |r: &Value| r["result"]["approvals"].as_array().map_or(0, Vec::len);
        let code = |r: &Value| r["error"]["code"].clone();
        let invalid = json!(ErrorCode::InvalidParams.code());
        assert_eq!(
            (
                (count(&first), &first["result"]["has_more"]),
                (count(&second), &second["result"]["has_more"]),
                (count(&of_run), &of_run["result"]["approvals"][0]["run_id"]),
                (count(&none), none["result"].get("next").is_none()),
                (code(&bad_status), code(&bad_after), code(&long)),
                code(&unknown),
            ),
            (
                (2, &json!(true)),
                (1, &json!(false)),
                (1, &json!(runs[1])),
                (0, true),
                (invalid.clone(), invalid.clone(), invalid),
                json!(ErrorCode::ApprovalNotFound.code()),
            )
        );
        stop_runs(&mut raw, &runs).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_held_signal_call_should_answer_over_the_wire_and_refuse_a_second_call_meanwhile() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = agent_runs(&mut raw, 1, 0, false).await.remove(0);
        until_running(&engine, &run).await;
        let token = fake_token(&engine, &run);
        let mut signal = engine.raw().await;
        signal.hello_as("signal", PROTOCOL.major).await;
        let ask = br#"{"kind":"other","summary":"by hand","detail":{"tool":"t"}}"#;
        let held = signal
            .send(
                methods::SIGNAL_HOOK,
                json!({ "token": token, "hook": "approval", "stdin_b64": b64(ask) }),
            )
            .await;
        let (requested, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let second = signal
            .call(
                methods::SIGNAL_HOOK,
                json!({ "token": token, "hook": "status", "stdin_b64": "" }),
            )
            .await;
        respond(&mut raw, &requested["approval"]["id"], "deny", Some("nope")).await;
        let reply = signal.response(held).await;
        stop_runs(&mut raw, std::slice::from_ref(&run)).await;
        assert_eq!(
            (
                &second["error"]["code"],
                String::from_utf8(unb64(&reply["result"]["stdout_b64"])).unwrap(),
                &reply["result"]["exit_code"],
            ),
            (
                &json!(ErrorCode::InvalidRequest.code()),
                r#"{"decision":"deny","message":"nope"}"#.to_owned(),
                &json!(0),
            )
        );
    }

    // ---- detection (spec 0003, milestone 4) ----

    /// An engine whose login `PATH` starts with `stubs`.
    async fn engine_with_path(stubs: &Path) -> Engine {
        let path = format!("{}:/usr/bin:/bin", stubs.display());
        Engine::start_with(&[("PATH", &path)]).await
    }

    fn by_id<'a>(agents: &'a Value, id: &str) -> &'a Value {
        agents["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == json!(id))
            .unwrap_or_else(|| panic!("no {id} in {agents}"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detection_should_ask_claude_for_its_version_on_the_login_path() {
        let stubs = tempfile::tempdir().unwrap();
        let claude = stub(stubs.path(), "claude", "echo '2.1.325 (Claude Code)'");
        let engine = engine_with_path(stubs.path()).await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let detected = raw.call(methods::AGENT_DETECT, json!({})).await;
        let listed = raw.call(methods::AGENT_LIST, json!({})).await;
        let only = raw
            .call(methods::AGENT_DETECT, json!({ "agent_id": "claude" }))
            .await;
        let unknown = raw
            .call(methods::AGENT_DETECT, json!({ "agent_id": "nope" }))
            .await;
        let ids: Vec<_> = listed["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].clone())
            .collect();
        let c = by_id(&detected, "claude");
        let fake = by_id(&detected, "fake");
        assert_eq!(
            (
                ids,
                &c["detection"]["installed"],
                &c["detection"]["version"],
                &c["detection"]["signed_in"],
                &c["detection"]["path"],
                c["detection"].get("hint"),
                c["detection"]["checked_at"].as_i64().unwrap() > 0,
                &c["capabilities"]["launch"],
            ),
            (
                vec![json!("claude"), json!("fake")],
                &json!(true),
                &json!("2.1.325"),
                &json!("unknown"),
                &json!(claude),
                None,
                true,
                &json!(false),
            )
        );
        assert_eq!(
            (
                &fake["detection"]["installed"],
                fake["detection"].get("path"),
                only["result"]["agents"].as_array().unwrap().len(),
                &unknown["error"]["code"],
            ),
            (
                &json!(true),
                None,
                1,
                &json!(ErrorCode::AgentNotFound.code()),
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_claude_should_be_not_installed_with_no_path() {
        let stubs = tempfile::tempdir().unwrap();
        let engine = engine_with_path(stubs.path()).await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let detected = raw
            .call(methods::AGENT_DETECT, json!({ "agent_id": "claude" }))
            .await;
        let d = &by_id(&detected, "claude")["detection"];
        assert_eq!(
            (&d["installed"], d.get("path"), d.get("version")),
            (&json!(false), None, None)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hanging_claude_should_be_killed_at_5_s_and_concurrent_detects_share_one_pass() {
        let stubs = tempfile::tempdir().unwrap();
        let count = stubs.path().join("count");
        stub(
            stubs.path(),
            "claude",
            &format!("echo run >> '{}'\nsleep 30", count.display()),
        );
        let engine = engine_with_path(stubs.path()).await;
        let started = Instant::now();
        let mut conns = Vec::new();
        for _ in 0..3 {
            let mut raw = engine.raw().await;
            raw.hello(PROTOCOL.major).await;
            conns.push(raw);
        }
        let mut ids = Vec::new();
        for raw in &mut conns {
            ids.push(raw.send(methods::AGENT_DETECT, json!({})).await);
        }
        let mut answers = Vec::new();
        for (raw, id) in conns.iter_mut().zip(ids) {
            answers.push(raw.response(id).await);
        }
        let took = started.elapsed();
        let runs = std::fs::read_to_string(&count).unwrap().lines().count();
        let d = &by_id(&answers[0], "claude")["detection"];
        assert_eq!(
            (&d["installed"], &d["hint"], d.get("version"), runs),
            (
                &json!(true),
                &json!("claude --version did not answer"),
                None,
                1
            )
        );
        assert!(
            took >= Duration::from_millis(4500) && took < Duration::from_secs(9),
            "{took:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn env_refresh_should_report_the_capture_and_rerun_detection() {
        let stubs = tempfile::tempdir().unwrap();
        stub(stubs.path(), "claude", "echo '1.0.0'");
        let engine = engine_with_path(stubs.path()).await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let first = raw.call(methods::AGENT_DETECT, json!({})).await;
        let at = by_id(&first, "claude")["detection"]["checked_at"]
            .as_i64()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let refreshed = raw.call(methods::ENV_REFRESH, json!({})).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let listed = raw.call(methods::AGENT_LIST, json!({})).await;
            if by_id(&listed, "claude")["detection"]["checked_at"]
                .as_i64()
                .unwrap()
                > at
            {
                break;
            }
            assert!(Instant::now() < deadline, "detection never reran");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            (refreshed["result"].clone(), login_env(&mut raw).await),
            (
                json!({ "login_env": { "status": "ok" } }),
                json!({ "status": "ok" })
            )
        );
    }

    // ---- what is never logged (spec 0003, *Observability*) ----

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_log_should_never_hold_payloads_tokens_env_values_or_prompts() {
        const MARK: &str = "sy-never-logged-41c9";
        let stubs = tempfile::tempdir().unwrap();
        stub(
            stubs.path(),
            "claude",
            &format!("echo '9.9.9 {MARK}-probe'"),
        );
        let path = format!("{}:/usr/bin:/bin", stubs.path().display());
        let env_value = format!("{MARK}-env");
        let mut engine = Engine::start_with(&[
            ("SWITCHYARD_LOG", "trace"),
            ("PATH", &path),
            ("SY_TEST_SECRET", &env_value),
        ])
        .await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        raw.call(methods::AGENT_DETECT, json!({})).await;
        let steps = json!([
            { "session": format!("{MARK}-session") },
            { "event": { "message": { "role": "agent", "text": format!("{MARK}-event") } } },
            { "approval": {
                "kind": "command",
                "summary": format!("{MARK}-summary"),
                "detail": { "command": format!("{MARK}-command") },
                "on_approve": [{ "expect_input": NEVER }],
            } },
        ]);
        let result = raw
            .call(
                methods::DEV_AGENT_RUN,
                json!({ "agent_id": "fake", "prompt": format!("{MARK}-prompt"), "options": { "script": steps } }),
            )
            .await;
        let run = result["result"]["run_ids"][0].as_str().unwrap().to_owned();
        let (requested, _) = until_note(&mut raw, methods::APPROVAL_REQUESTED, Some(&run)).await;
        let token = fake_token(&engine, &run);
        respond(
            &mut raw,
            &requested["approval"]["id"],
            "approve",
            Some(&format!("{MARK}-message")),
        )
        .await;
        stop_runs(&mut raw, std::slice::from_ref(&run)).await;
        drop(raw);
        stop(&mut engine).await;
        let log = engine.log();
        let leaks: Vec<&str> = log
            .lines()
            .filter(|l| l.contains(MARK) || l.contains(&token))
            .collect();
        assert!(
            log.contains("hook call"),
            "no hook call debug lines at trace level"
        );
        assert!(log.contains(&run), "the run's span fields are missing");
        assert!(leaks.is_empty(), "engine.log leaks: {leaks:#?}");
    }
}
