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

fn spawn_in(dir: &Path, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_switchyardd"));
    cmd.env("SWITCHYARD_DATA_DIR", dir)
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
}

impl Raw {
    async fn connect(socket: &Path) -> Self {
        let (read, writer) = UnixStream::connect(socket).await.unwrap().into_split();
        Self {
            reader: BufReader::new(read),
            writer,
            next_id: 1,
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

    /// Next message, or `None` on EOF.
    async fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .ok()?
            .ok()?;
        (n > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let msg = self
                .next()
                .await
                .expect("connection closed before the response");
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    async fn hello(&mut self, major: u32) -> Value {
        self.call(methods::SESSION_HELLO, json!({ "client": "test", "client_version": "raw", "protocol": { "major": major, "minor": 0 } }))
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
    for method in [methods::DEV_SYNTHETIC_RUN, methods::DEV_STOP_RUNS] {
        let err = conn
            .call_value(
                method,
                json!({ "count": 1, "bytes_per_sec": 0, "echo": false }),
            )
            .await
            .unwrap_err();
        assert_eq!(rpc_code(&err), ErrorCode::MethodNotFound.code(), "{method}");
    }
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
        (json!("ok"), json!(1))
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
        (json!("corrupt"), json!("ok"), json!(1), true)
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

#[cfg(feature = "devtools")]
mod devtools {
    use super::*;
    use base64::Engine as _;
    use sy_protocol::{RunAttachResult, SyntheticRunResult};

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn unb64(text: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(text.as_str().unwrap())
            .unwrap()
    }

    async fn synthetic(raw: &mut Raw, count: u32, bytes_per_sec: u64, echo: bool) -> Vec<String> {
        let reply = raw
            .call(
                methods::DEV_SYNTHETIC_RUN,
                json!({ "count": count, "bytes_per_sec": bytes_per_sec, "echo": echo }),
            )
            .await;
        let result: SyntheticRunResult = serde_json::from_value(reply["result"].clone()).unwrap();
        result.run_ids.into_iter().map(|id| id.0).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_then_live_output_should_have_no_gap_and_no_duplicate() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = synthetic(&mut raw, 1, 200_000, false).await.remove(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let id = raw
            .send(methods::RUN_ATTACH, json!({ "run_id": run }))
            .await;

        let mut cursor = None;
        let mut live_bytes = 0_usize;
        while live_bytes < 100_000 {
            let msg = raw.next().await.unwrap();
            if msg["id"] == json!(id) {
                let attach: RunAttachResult =
                    serde_json::from_value(msg["result"].clone()).unwrap();
                cursor = Some(attach.end_offset);
            } else if msg["method"] == json!(methods::RUN_OUTPUT) {
                let Some(at) = cursor else { continue };
                let offset = msg["params"]["offset"].as_u64().unwrap();
                let data = unb64(&msg["params"]["data_b64"]);
                let end = offset + data.len() as u64;
                assert!(
                    offset <= at,
                    "gap: chunk starts at {offset}, expected at most {at}"
                );
                if end > at {
                    live_bytes += (end - at) as usize;
                    cursor = Some(end);
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn echo_run_should_write_input_back_as_output() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let run = synthetic(&mut raw, 1, 0, true).await.remove(0);
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
        let run = synthetic(&mut raw, 1, 0, true).await.remove(0);
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
        let run = synthetic(&mut slow, 1, 8 * 1024 * 1024, false)
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
        synthetic(&mut raw, 2, 1000, false).await;
        raw.call(methods::ENGINE_DRAIN, json!({})).await;
        let refused = raw
            .call(
                methods::DEV_SYNTHETIC_RUN,
                json!({ "count": 1, "bytes_per_sec": 0, "echo": false }),
            )
            .await;
        assert_eq!(
            refused["error"]["code"],
            json!(ErrorCode::EngineDraining.code())
        );
        assert!(
            engine.wait_exit(Duration::from_millis(500)).is_none(),
            "exited while runs were active"
        );

        raw.call(methods::DEV_STOP_RUNS, json!({})).await;
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
    async fn synthetic_runs_should_be_saved_and_every_client_should_get_run_status_in_event_order()
    {
        use std::collections::HashMap;
        let engine = Engine::start().await;
        let mut viewer = engine.raw().await;
        viewer.hello(PROTOCOL.major).await;
        let mut watcher = engine.raw().await; // never attaches
        watcher.hello(PROTOCOL.major).await;

        let reply = viewer
            .call(
                methods::DEV_SYNTHETIC_RUN,
                json!({ "count": 3, "bytes_per_sec": 50_000, "echo": false }),
            )
            .await;
        let result: SyntheticRunResult = serde_json::from_value(reply["result"].clone()).unwrap();
        let runs: Vec<String> = result.run_ids.iter().map(|r| r.0.clone()).collect();
        let mut seen = HashMap::new();
        attach_all(&mut viewer, &runs, &mut seen).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stop = viewer.send(methods::DEV_STOP_RUNS, json!({})).await;

        // The viewer: output until each run's `stopped`, which must come after its last output.
        let mut stopped = std::collections::HashSet::new();
        let mut stop_answered = false;
        while stopped.len() < runs.len() || !stop_answered {
            let msg = viewer.next().await.expect("viewer connection closed");
            if msg["id"] == json!(stop) {
                stop_answered = true;
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
            let msg = watcher
                .next()
                .await
                .expect("watcher got too few run.status");
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
                (status.as_str(), end_reason.as_str(), pid),
                ("stopped", "user_stop", None)
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
        // Each call journals a task plus 4 events per run: over 500 events in all.
        let first = raw
            .call(
                methods::DEV_SYNTHETIC_RUN,
                json!({ "count": 64, "bytes_per_sec": 0, "echo": false }),
            )
            .await;
        let first: SyntheticRunResult = serde_json::from_value(first["result"].clone()).unwrap();
        let second = synthetic(&mut raw, 64, 0, false).await;
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
        let run = synthetic(&mut raw, 1, 2_000_000, false).await.remove(0);
        tokio::time::sleep(Duration::from_millis(400)).await;
        // Live: what is saved so far reads back, and `eof` means "the end so far".
        let live = raw
            .call(methods::RUN_LOG_READ, json!({ "run_id": run, "offset": 0 }))
            .await;
        assert!(live["result"]["log_bytes"].as_u64().unwrap() > 0, "{live}");
        raw.call(methods::DEV_STOP_RUNS, json!({})).await;

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
    async fn second_synthetic_call_should_reuse_the_fixture_project() {
        let engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        synthetic(&mut raw, 1, 0, false).await;
        synthetic(&mut raw, 2, 0, false).await;
        let (projects, tasks, runs): (i64, i64, i64) = db(&engine)
            .query_row(
                "SELECT (SELECT count(*) FROM project), (SELECT count(*) FROM task), (SELECT count(*) FROM run WHERE kind = 'agent' AND status = 'running')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((projects, tasks, runs), (1, 2, 3));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_9_should_lose_nothing_a_client_already_saw() {
        use std::collections::HashMap;
        let mut engine = Engine::start().await;
        let mut raw = engine.raw().await;
        raw.hello(PROTOCOL.major).await;
        let runs = synthetic(&mut raw, 3, 200_000, false).await;
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
        assert_eq!(synthetic(&mut again, 1, 0, false).await.len(), 1);
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
        let first = synthetic(&mut raw, 1, 0, false).await.remove(0);
        raw.call(methods::DEV_STOP_RUNS, json!({})).await;
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
        let second = synthetic(&mut raw, 1, 0, false).await.remove(0);
        raw.call(methods::DEV_STOP_RUNS, json!({})).await;
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
        synthetic(&mut raw, 3, 0, false).await;
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
        let run = synthetic(&mut viewer, 1, 200_000, false).await.remove(0);
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
            .call(
                methods::DEV_SYNTHETIC_RUN,
                json!({ "count": 1, "bytes_per_sec": 0, "echo": false }),
            )
            .await;
        let info = storage_of(&mut viewer).await;
        viewer.call(methods::DEV_STOP_RUNS, json!({})).await;

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
        let run = synthetic(&mut viewer, 1, 100_000, false).await.remove(0);
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
        let stop_id = viewer.send(methods::DEV_STOP_RUNS, json!({})).await;
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
        let run = synthetic(&mut raw, 1, 0, true).await.remove(0);
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
        raw.call(methods::DEV_STOP_RUNS, json!({})).await;
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
}
