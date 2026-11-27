#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! `sy-fake-agent` as a process (spec 0003, *Fake agent*): each step's effect
//! on its output, its cwd and its exit, and the exact `sy-signal` calls its
//! hook steps make. A stub helper stands in for `sy-signal` and records
//! every call; the daemon's socket tests cover the real one.
//!
//! The fake runs on pipes here, not a PTY: raw mode is skipped, and input is
//! read the same way.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const FAKE: &str = env!("CARGO_BIN_EXE_sy-fake-agent");

/// A scratch dir under the system temp dir, removed on drop. (This crate
/// takes no dependencies, test ones included.)
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sy-fake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("cwd")).unwrap();
        Self(dir)
    }

    fn cwd(&self) -> PathBuf {
        self.0.join("cwd")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.0.join("calls.log")).unwrap_or_default()
    }

    /// A stub `sy-signal` that logs `<args joined by |>` then `stdin:<stdin>`
    /// per call, and answers `reply` on stdout.
    fn helper(&self, reply: &str) -> PathBuf {
        let path = self.0.join("sy-signal");
        let log = self.0.join("calls.log");
        let script = format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s|' \"$a\" >> '{log}'; done\n\
             printf '\\nstdin:' >> '{log}'\n\
             cat >> '{log}'\n\
             printf '\\n' >> '{log}'\n\
             printf '%s' '{reply}'\n",
            log = log.display(),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Run the fake on `script` with this `input`, a helper answering `reply`.
    fn run(&self, script: &serde_json::Value, input: &[u8], reply: &str) -> Output {
        let script_path = self.0.join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let helper = self.helper(reply);
        self.run_args(
            &[
                "--script".as_ref(),
                script_path.as_os_str(),
                "--signal".as_ref(),
                helper.as_os_str(),
                "--socket".as_ref(),
                "/tmp/sy-x/engine.sock".as_ref(),
                "--token".as_ref(),
                "00ff".as_ref(),
            ],
            input,
        )
    }

    fn run_args(&self, args: &[&std::ffi::OsStr], input: &[u8]) -> Output {
        let mut child = Command::new(FAKE)
            .args(args)
            .current_dir(self.cwd())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

const APPROVE: &str = r#"{"decision":"approve"}"#;

// ---- output and exit ----

#[test]
fn print_and_print_b64_should_write_their_bytes_and_exit_should_set_the_code() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([
            { "print": "hi\r\n" },
            { "print_b64": "AAH/" },
            { "exit": 7 },
            { "print": "never" },
        ]),
        b"",
        "",
    );
    assert_eq!(
        (out.stdout.as_slice(), out.status.code()),
        (b"hi\r\n\x00\x01\xff".as_slice(), Some(7))
    );
}

#[test]
fn a_script_that_runs_out_of_steps_should_exit_0() {
    let s = Scratch::new();
    let out = s.run(&serde_json::json!([{ "print": "done" }]), b"", "");
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("done".to_owned(), Some(0))
    );
}

// covers: AC-6 (a death by signal reports the signal and no code)
#[test]
fn die_by_signal_should_end_the_process_by_that_signal() {
    let s = Scratch::new();
    let codes: Vec<_> = ["SIGTERM", "SIGSEGV", "SIGKILL"]
        .iter()
        .map(|sig| {
            let out = s.run(&serde_json::json!([{ "die_by_signal": sig }]), b"", "");
            (out.status.code(), out.status.signal())
        })
        .collect();
    assert_eq!(codes, [(None, Some(15)), (None, Some(11)), (None, Some(9))]);
}

#[test]
fn a_flood_with_total_bytes_should_write_exactly_that_many_printable_bytes_then_go_on() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([
            { "flood": { "bytes_per_sec": 2_000_000, "total_bytes": 20_000 } },
            { "print": "END" },
        ]),
        b"",
        "",
    );
    let body = out.stdout.strip_suffix(b"END").unwrap();
    assert_eq!(
        (
            body.len(),
            body.iter()
                .all(|b| (b' '..=b'~').contains(b) || *b == b'\r' || *b == b'\n'),
            out.status.code(),
        ),
        (20_000, true, Some(0))
    );
}

#[test]
fn a_flood_should_be_the_same_bytes_every_run() {
    let s = Scratch::new();
    let script =
        serde_json::json!([{ "flood": { "bytes_per_sec": 4_000_000, "total_bytes": 4_096 } }]);
    let (a, b) = (s.run(&script, b"", ""), s.run(&script, b"", ""));
    assert_eq!(a.stdout, b.stdout);
}

// ---- files ----

// covers: AC-5 (the fake's file steps land in its cwd, the worktree)
#[test]
fn write_file_and_delete_file_should_change_only_the_cwd() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([
            { "write_file": { "path": "a.txt", "contents": "first" } },
            { "write_file": { "path": "b.txt", "contents": "second" } },
            { "delete_file": { "path": "a.txt" } },
        ]),
        b"",
        "",
    );
    assert_eq!(
        (
            out.status.code(),
            exists(&s.cwd(), "a.txt"),
            std::fs::read_to_string(s.cwd().join("b.txt")).unwrap(),
        ),
        (Some(0), false, "second".to_owned())
    );
}

#[test]
fn a_path_leaving_the_cwd_should_fail_the_run_and_write_nothing() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([{ "write_file": { "path": "../escaped.txt", "contents": "x" } }]),
        b"",
        "",
    );
    assert_eq!(
        (out.status.code(), exists(&s.0, "escaped.txt")),
        (Some(1), false)
    );
    assert!(
        stdout(&out).contains("must be relative with no `..`"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn deleting_a_missing_file_should_fail_with_the_reason() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([{ "delete_file": { "path": "nope.txt" } }]),
        b"",
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).starts_with("sy-fake-agent: delete_file: "),
        "{}",
        stdout(&out)
    );
}

// ---- input ----

#[test]
fn expect_input_should_wait_for_the_literal_on_one_line() {
    let s = Scratch::new();
    // "g" then "o" on the next line is not "go"; the third line is.
    let out = s.run(
        &serde_json::json!([{ "expect_input": "go" }, { "print": "ok" }]),
        b"g\ro\rready? go\r",
        "",
    );
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("ok".to_owned(), Some(0))
    );
}

#[test]
fn a_terminal_closing_before_the_literal_should_fail_the_run() {
    let s = Scratch::new();
    let out = s.run(&serde_json::json!([{ "expect_input": "go" }]), b"no\r", "");
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("sy-fake-agent: the terminal closed\r\n".to_owned(), Some(1))
    );
}

#[test]
fn echo_until_should_echo_input_up_to_and_including_the_literal() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([{ "echo_until": "STOP" }, { "print": "|after" }]),
        b"hello STOP",
        "",
    );
    assert_eq!(stdout(&out), "hello STOP|after");
}

// covers: AC-12 (Ctrl C as a byte ends the fake with 130)
#[test]
fn ctrl_c_should_exit_130_whatever_step_is_running() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([{ "expect_input": "never" }]),
        b"\x03",
        "",
    );
    assert_eq!(out.status.code(), Some(130));
}

// ---- hooks through sy-signal ----

// covers: AC-7 (the fake reaches the engine only through `sy-signal`, with `hook_command`'s argv)
#[test]
fn hook_steps_should_call_the_helper_with_the_documented_argv_and_json_stdin() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([
            { "status": "waiting_input" },
            { "session": "s-1" },
            { "event": { "message": { "role": "agent", "text": "hi" } } },
        ]),
        b"",
        "",
    );
    let argv = |hook: &str| {
        format!(
            "--socket|/tmp/sy-x/engine.sock|--token|00ff|--hook|{hook}|--fallback-exit|0|--fallback-stdout-b64||--|"
        )
    };
    assert_eq!(
        (out.status.code(), s.calls()),
        (
            Some(0),
            format!(
                "{}\nstdin:\"waiting_input\"\n{}\nstdin:\"s-1\"\n{}\nstdin:{}\n",
                argv("status"),
                argv("session"),
                argv("event"),
                r#"{"message":{"role":"agent","text":"hi"}}"#,
            )
        )
    );
}

// covers: AC-9 (an approval holds the fake on the hook and runs the answer's branch)
#[test]
fn an_approval_should_send_its_ask_with_the_ask_fallback_and_run_the_answers_branch() {
    let script = serde_json::json!([{ "approval": {
        "kind": "command",
        "summary": "deploy",
        "detail": { "command": "make deploy" },
        "on_approve": [{ "print": "approved" }, { "exit": 0 }],
        "on_deny": [{ "print": "denied" }, { "exit": 4 }],
    } }]);
    let s = Scratch::new();
    let approved = s.run(&script, b"", APPROVE);
    let calls = s.calls();
    let denied = s.run(&script, b"", r#"{"decision":"deny","message":"no"}"#);
    assert_eq!(
        (
            (stdout(&approved), approved.status.code()),
            (stdout(&denied), denied.status.code()),
        ),
        (
            ("approved".to_owned(), Some(0)),
            ("denied".to_owned(), Some(4))
        )
    );
    assert_eq!(
        calls,
        "--socket|/tmp/sy-x/engine.sock|--token|00ff|--hook|approval|--fallback-exit|0|\
         --fallback-stdout-b64|eyJkZWNpc2lvbiI6ImFzayJ9|--|\n\
         stdin:{\"detail\":{\"command\":\"make deploy\"},\"kind\":\"command\",\"summary\":\"deploy\"}\n"
    );
}

// covers: AC-7, AC-9 (no usable answer means the fake asks itself, never goes ahead unasked)
#[test]
fn an_ask_or_unreadable_reply_should_make_the_fake_ask_in_its_terminal() {
    let script = serde_json::json!([{ "approval": {
        "kind": "fetch",
        "summary": "fetch docs",
        "detail": { "url": "https://example.com" },
        "on_approve": [{ "print": "yes" }],
        "on_deny": [{ "print": "no" }],
    } }]);
    let s = Scratch::new();
    let runs = [
        (r#"{"decision":"ask"}"#, &b"y\r"[..]),
        (r#"{"decision":"ask"}"#, &b"n\r"[..]),
        ("", &b"Y\r"[..]),
        ("not json", &b"maybe\r"[..]),
    ]
    .map(|(reply, input)| stdout(&s.run(&script, input, reply)));
    let prompt = "Allow fetch docs? [y/n] \r\n";
    assert_eq!(
        runs,
        [
            format!("{prompt}yes"),
            format!("{prompt}no"),
            format!("{prompt}yes"),
            format!("{prompt}no"),
        ]
    );
}

// ---- arguments and scripts ----

#[test]
fn missing_or_unknown_arguments_should_exit_1_with_the_reason() {
    let s = Scratch::new();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "--script is required"),
        (vec!["--script", "x.json"], "--signal is required"),
        (vec!["--script"], "--script needs a value"),
        (vec!["--verbose", "1"], "unknown argument --verbose"),
    ];
    for (args, reason) in cases {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_ref()).collect();
        let out = s.run_args(&args, b"");
        assert_eq!(
            (stdout(&out), out.status.code()),
            (format!("sy-fake-agent: {reason}\r\n"), Some(1)),
            "{args:?}"
        );
    }
}

#[test]
fn an_unreadable_or_malformed_script_should_exit_1_before_any_step() {
    let s = Scratch::new();
    let bad = s.run(
        &serde_json::json!([{ "print": "x" }, { "dance": true }]),
        b"",
        "",
    );
    let missing = s.run_args(
        &[
            "--script".as_ref(),
            "/nonexistent/script.json".as_ref(),
            "--signal".as_ref(),
            "h".as_ref(),
            "--socket".as_ref(),
            "s".as_ref(),
            "--token".as_ref(),
            "t".as_ref(),
        ],
        b"",
    );
    assert_eq!(
        (bad.status.code(), missing.status.code()),
        (Some(1), Some(1))
    );
    assert!(
        stdout(&bad).starts_with("sy-fake-agent: parsing the script: "),
        "{}",
        stdout(&bad)
    );
    assert!(
        stdout(&missing).starts_with("sy-fake-agent: reading the script: "),
        "{}",
        stdout(&missing)
    );
}

// ---- spec 0004 steps ----

/// The children of `pid`, through `pgrep -P`.
fn children_of(pid: u32) -> Vec<i32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    stdout(&out)
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

fn pid(raw: i32) -> rustix::process::Pid {
    rustix::process::Pid::from_raw(raw).unwrap()
}

/// Running, not a zombie (the fake never waits on its children).
fn alive(raw: i32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &raw.to_string()])
        .output()
        .unwrap();
    let stat = stdout(&out);
    !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
}

/// Start the fake on `script` and wait for `n` children.
fn start_with_children(
    s: &Scratch,
    script: &serde_json::Value,
    n: usize,
) -> (std::process::Child, Vec<i32>) {
    let script_path = s.0.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let helper = s.helper("");
    let mut fake = Command::new(FAKE)
        .arg("--script")
        .arg(&script_path)
        .arg("--signal")
        .arg(&helper)
        .args(["--socket", "/tmp/sy-x/engine.sock", "--token", "00ff"])
        .current_dir(s.cwd())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        let kids = children_of(fake.id());
        if kids.len() >= n {
            return (fake, kids);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fake.kill().unwrap();
    fake.wait().unwrap();
    panic!("the fake never started {n} children");
}

#[test]
fn spawn_should_start_a_blocking_child_in_the_group_or_detached_from_it() {
    let s = Scratch::new();
    let (mut fake, kids) = start_with_children(
        &s,
        &serde_json::json!([
            { "spawn": {} },
            { "spawn": { "detach": true, "ignore_sigterm": true } },
            { "expect_input": "never-typed" },
        ]),
        2,
    );
    let fake_group = rustix::process::getpgid(Some(pid(fake.id().cast_signed()))).unwrap();
    let groups: Vec<bool> = kids
        .iter()
        .map(|&k| rustix::process::getpgid(Some(pid(k))).unwrap() == fake_group)
        .collect();
    assert_eq!(groups.iter().filter(|g| **g).count(), 1, "{groups:?}");
    // The detached one ignores SIGTERM; the other doesn't.
    for &k in &kids {
        rustix::process::kill_process(pid(k), rustix::process::Signal::TERM).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let survivors: Vec<i32> = kids.iter().copied().filter(|&k| alive(k)).collect();
    assert_eq!(survivors.len(), 1, "{survivors:?}");
    assert_ne!(
        rustix::process::getpgid(Some(pid(survivors[0]))).unwrap(),
        fake_group
    );
    rustix::process::kill_process(pid(survivors[0]), rustix::process::Signal::KILL).unwrap();
    fake.kill().unwrap();
    fake.wait().unwrap();
}

#[test]
fn ignore_sigterm_should_keep_the_fake_running_through_sigterm() {
    let s = Scratch::new();
    let (mut fake, kids) = start_with_children(
        &s,
        &serde_json::json!([
            { "ignore_sigterm": true },
            { "spawn": {} },
            { "expect_input": "never-typed" },
        ]),
        1,
    );
    let me = pid(fake.id().cast_signed());
    rustix::process::kill_process(me, rustix::process::Signal::TERM).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(fake.try_wait().unwrap().is_none(), "SIGTERM ended it");
    fake.kill().unwrap();
    fake.wait().unwrap();
    for k in kids {
        let _ = rustix::process::kill_process(pid(k), rustix::process::Signal::KILL);
    }
}

#[test]
fn panic_decoder_should_call_the_panic_hook_and_go_on() {
    let s = Scratch::new();
    let out = s.run(
        &serde_json::json!([{ "panic_decoder": true }, { "print": "after" }]),
        b"",
        "",
    );
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("after".to_owned(), Some(0))
    );
    assert!(s.calls().contains("--hook|panic|"), "{}", s.calls());
}
