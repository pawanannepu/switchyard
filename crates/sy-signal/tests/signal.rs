#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! Runs the real `sy-signal` binary against a tiny stand in engine socket
//! that records what it was sent and answers as each test says.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sy_protocol::{ErrorCode, MAX_SIGNAL_PAYLOAD_BYTES, PROTOCOL, methods};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// How the stand in engine answers `signal.hook`.
#[derive(Clone)]
enum Answer {
    Result(Value),
    Error(ErrorCode),
}

struct Engine {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    /// Every request it got, as (method, params).
    seen: mpsc::UnboundedReceiver<(String, Value)>,
}

/// A stand in engine. `major` is the protocol major its hello reports.
fn engine(answer: Answer, major: u32) -> Engine {
    let dir = tempfile::Builder::new()
        .prefix("sy-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (tx, answer) = (tx.clone(), answer.clone());
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let msg: Value = serde_json::from_str(&line).unwrap();
                    let method = msg["method"].as_str().unwrap().to_owned();
                    tx.send((method.clone(), msg["params"].clone())).unwrap();
                    let outcome = match (method.as_str(), &answer) {
                        (methods::SESSION_HELLO, _) => json!({ "result": {
                            "engine_version": "test", "pid": 1,
                            "protocol": { "major": major, "minor": 0 },
                        } }),
                        (_, Answer::Result(result)) => json!({ "result": result }),
                        (_, Answer::Error(code)) => {
                            json!({ "error": { "code": code.code(), "message": "no" } })
                        }
                    };
                    let mut reply = json!({ "jsonrpc": "2.0", "id": msg["id"] });
                    reply
                        .as_object_mut()
                        .unwrap()
                        .extend(outcome.as_object().unwrap().clone());
                    let line = reply.to_string() + "\n";
                    write.write_all(line.as_bytes()).await.unwrap();
                }
            });
        }
    });
    Engine {
        _dir: dir,
        socket,
        seen,
    }
}

/// The fallback every test passes: exit 7, stdout `fallback`.
const FALLBACK_B64: &str = "ZmFsbGJhY2s=";

/// Run the helper with `stdin`; returns (stdout, exit code).
async fn run(socket: &Path, stdin: Vec<u8>, extra: &[&str]) -> (String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sy-signal"));
    cmd.arg("--socket")
        .arg(socket)
        .args(["--token", "t0k", "--hook", "status"])
        .args([
            "--fallback-exit",
            "7",
            "--fallback-stdout-b64",
            FALLBACK_B64,
        ])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    tokio::task::spawn_blocking(move || {
        let mut child = cmd.spawn().unwrap();
        // A helper that stops reading early closes the pipe; that's fine.
        let _ = child.stdin.take().unwrap().write_all(&stdin);
        let out = child.wait_with_output().unwrap();
        (
            String::from_utf8(out.stdout).unwrap(),
            out.status.code().unwrap(),
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn it_should_say_hello_as_signal_relay_the_call_and_answer_with_the_reply() {
    let mut e = engine(
        Answer::Result(json!({ "stdout_b64": "aGk=", "exit_code": 3 })),
        PROTOCOL.major,
    );
    let answer = run(
        &e.socket,
        b"{\"a\":1}".to_vec(),
        &["--", "one", "two words"],
    )
    .await;
    let (hello, call) = (e.seen.recv().await.unwrap(), e.seen.recv().await.unwrap());
    assert_eq!(
        (answer, hello.0, hello.1["client"].clone(), call),
        (
            ("hi".to_owned(), 3),
            methods::SESSION_HELLO.to_owned(),
            json!("signal"),
            (
                methods::SIGNAL_HOOK.to_owned(),
                json!({
                    "token": "t0k",
                    "hook": "status",
                    "args": ["one", "two words"],
                    "stdin_b64": "eyJhIjoxfQ==",
                })
            )
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn any_error_reply_should_use_the_fallback() {
    for code in [
        ErrorCode::SignalTokenUnknown,
        ErrorCode::SignalRateLimited,
        ErrorCode::StorageUnavailable,
    ] {
        let e = engine(Answer::Error(code), PROTOCOL.major);
        assert_eq!(
            run(&e.socket, Vec::new(), &[]).await,
            ("fallback".to_owned(), 7),
            "{code:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_engine_or_incompatible_hello_should_use_the_fallback() {
    let gone = Path::new("/tmp/sy-no-such-dir/engine.sock");
    let incompatible = engine(
        Answer::Result(json!({ "stdout_b64": "", "exit_code": 0 })),
        PROTOCOL.major + 1,
    );
    assert_eq!(
        [
            run(gone, Vec::new(), &[]).await,
            run(&incompatible.socket, Vec::new(), &[]).await,
        ],
        [("fallback".to_owned(), 7), ("fallback".to_owned(), 7)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_payload_over_256_kib_should_use_the_fallback_without_calling() {
    let mut e = engine(
        Answer::Result(json!({ "stdout_b64": "", "exit_code": 0 })),
        PROTOCOL.major,
    );
    let over = vec![b'x'; MAX_SIGNAL_PAYLOAD_BYTES + 1];
    let at_cap = vec![b'x'; MAX_SIGNAL_PAYLOAD_BYTES - 2];
    let answers = [
        run(&e.socket, over, &[]).await,
        // Its arguments count too: 2 bytes under the cap plus "abc" is over.
        run(&e.socket, at_cap.clone(), &["--", "abc"]).await,
        run(&e.socket, at_cap, &["--", "ab"]).await,
    ];
    let mut methods_seen = Vec::new();
    while let Ok((method, _)) = e.seen.try_recv() {
        methods_seen.push(method);
    }
    assert_eq!(
        (answers, methods_seen),
        (
            [
                ("fallback".to_owned(), 7),
                ("fallback".to_owned(), 7),
                (String::new(), 0)
            ],
            vec![
                methods::SESSION_HELLO.to_owned(),
                methods::SIGNAL_HOOK.to_owned()
            ]
        )
    );
}

#[test]
fn unusable_fallback_arguments_should_exit_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_sy-signal"))
        .args(["--socket", "/tmp/x", "--fallback-exit", "seven"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!((out.status.code(), out.stdout.len()), (Some(2), 0));
}
