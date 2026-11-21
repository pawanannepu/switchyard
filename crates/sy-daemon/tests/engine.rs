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
    tempfile::Builder::new().prefix("sy-").tempdir_in("/tmp").unwrap()
}

fn spawn_in(dir: &Path, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_switchyardd"));
    cmd.env("SWITCHYARD_DATA_DIR", dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
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
        let engine = Self { _dir: dir, paths, child };
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
        panic!("engine socket never came up at {}", self.paths.socket.display());
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
        Self { reader: BufReader::new(read), writer, next_id: 1 }
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string() + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
        id
    }

    /// Next message, or `None` on EOF.
    async fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line)).await.ok()?.ok()?;
        (n > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let msg = self.next().await.expect("connection closed before the response");
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
    assert_eq!((inode(&engine.paths.socket), info.pid), (before, engine.child.id()));
    assert!(engine.log().contains("already running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_after_kill_9_should_take_the_lock_and_replace_the_stale_socket() {
    let mut engine = Engine::start().await;
    let first_pid = engine.child.id();
    engine.child.kill().unwrap(); // SIGKILL
    engine.child.wait().unwrap();
    assert!(engine.paths.socket.exists(), "a killed engine leaves its socket behind");

    engine.child = spawn_in(&engine.paths.data_dir, &[]);
    engine.wait_for_socket().await;
    let info: EngineInfo = engine.client().await.call(methods::ENGINE_INFO, &json!({})).await.unwrap();
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

    assert_eq!(raw.next().await, None, "oversized line should close the connection");
    let info: Result<EngineInfo, _> = survivor.call(methods::ENGINE_INFO, &json!({})).await;
    assert!(info.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn non_stable_method_before_hello_should_need_hello() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    let reply = raw.call(methods::RUN_ATTACH, json!({ "run_id": "run-1" })).await;
    assert_eq!(reply["error"]["code"], json!(ErrorCode::HelloRequired.code()));
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_hello_should_keep_stable_methods_working() {
    let engine = Engine::start().await;
    let mut raw = engine.raw().await;
    raw.hello(PROTOCOL.major + 1).await;
    let info = raw.call(methods::ENGINE_INFO, json!({})).await;
    let attach = raw.call(methods::RUN_ATTACH, json!({ "run_id": "run-1" })).await;
    assert_eq!(
        (info["result"]["pid"].clone(), attach["error"]["code"].clone()),
        (json!(engine.child.id()), json!(ErrorCode::IncompatibleProtocol.code()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_should_be_method_not_found() {
    let engine = Engine::start().await;
    let err = engine.client().await.call_value("nope.nothing", Value::Null).await.unwrap_err();
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

#[cfg(not(feature = "devtools"))]
#[tokio::test(flavor = "multi_thread")]
async fn release_engine_should_not_answer_dev_methods() {
    let engine = Engine::start().await;
    let conn = engine.client().await;
    for method in [methods::DEV_SYNTHETIC_RUN, methods::DEV_STOP_RUNS] {
        let err = conn.call_value(method, json!({ "count": 1, "bytes_per_sec": 0, "echo": false })).await.unwrap_err();
        assert_eq!(rpc_code(&err), ErrorCode::MethodNotFound.code(), "{method}");
    }
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
        base64::engine::general_purpose::STANDARD.decode(text.as_str().unwrap()).unwrap()
    }

    async fn synthetic(raw: &mut Raw, count: u32, bytes_per_sec: u64, echo: bool) -> Vec<String> {
        let reply = raw.call(methods::DEV_SYNTHETIC_RUN, json!({ "count": count, "bytes_per_sec": bytes_per_sec, "echo": echo })).await;
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
        let id = raw.send(methods::RUN_ATTACH, json!({ "run_id": run })).await;

        let mut cursor = None;
        let mut live_bytes = 0_usize;
        while live_bytes < 100_000 {
            let msg = raw.next().await.unwrap();
            if msg["id"] == json!(id) {
                let attach: RunAttachResult = serde_json::from_value(msg["result"].clone()).unwrap();
                cursor = Some(attach.end_offset);
            } else if msg["method"] == json!(methods::RUN_OUTPUT) {
                let Some(at) = cursor else { continue };
                let offset = msg["params"]["offset"].as_u64().unwrap();
                let data = unb64(&msg["params"]["data_b64"]);
                let end = offset + data.len() as u64;
                assert!(offset <= at, "gap: chunk starts at {offset}, expected at most {at}");
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
        raw.call(methods::RUN_ATTACH, json!({ "run_id": run })).await;
        raw.call(methods::RUN_INPUT, json!({ "run_id": run, "data_b64": b64(b"ping-42") })).await;
        loop {
            let msg = raw.next().await.unwrap();
            if msg["method"] == json!(methods::RUN_OUTPUT) && unb64(&msg["params"]["data_b64"]).ends_with(b"ping-42") {
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
        let reply = raw.call(methods::RUN_INPUT, json!({ "run_id": run, "data_b64": b64(&big) })).await;
        assert_eq!(reply["error"]["code"], json!(ErrorCode::InputTooLarge.code()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_client_should_get_resync_while_a_fast_client_keeps_streaming() {
        let engine = Engine::start().await;
        let mut slow = engine.raw().await;
        slow.hello(PROTOCOL.major).await;
        let run = synthetic(&mut slow, 1, 8 * 1024 * 1024, false).await.remove(0);
        slow.call(methods::RUN_ATTACH, json!({ "run_id": run })).await;
        // Stop reading `slow` and let its queue pass the 8 MiB cap.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        let mut fast = engine.raw().await;
        fast.hello(PROTOCOL.major).await;
        fast.call(methods::RUN_ATTACH, json!({ "run_id": run })).await;
        let started = Instant::now();
        let mut fast_bytes = 0_usize;
        while started.elapsed() < Duration::from_secs(1) {
            let msg = fast.next().await.unwrap();
            assert_ne!(msg["method"], json!(methods::RUN_RESYNC_NEEDED), "the fast client fell behind too");
            if msg["method"] == json!(methods::RUN_OUTPUT) {
                fast_bytes += msg["params"]["data_b64"].as_str().unwrap().len();
            }
        }
        assert!(fast_bytes > 4 * 1024 * 1024, "fast client only got {fast_bytes} base64 bytes in 1 s");

        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        loop {
            let Some(msg) = slow.next().await else { panic!("slow client never saw run.resync_needed; saw {seen:?}") };
            *seen.entry(msg["method"].as_str().unwrap_or("response").to_owned()).or_default() += 1;
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
        let refused = raw.call(methods::DEV_SYNTHETIC_RUN, json!({ "count": 1, "bytes_per_sec": 0, "echo": false })).await;
        assert_eq!(refused["error"]["code"], json!(ErrorCode::EngineDraining.code()));
        assert!(engine.wait_exit(Duration::from_millis(500)).is_none(), "exited while runs were active");

        raw.call(methods::DEV_STOP_RUNS, json!({})).await;
        assert!(engine.wait_exit(Duration::from_secs(5)).is_some_and(|s| s.success()));
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
}
