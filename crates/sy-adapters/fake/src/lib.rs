//! The fake agent adapter (spec 0003, *Fake agent*).
//!
//! It launches `sy-fake-agent`, a real binary in a real PTY that runs the
//! JSON steps in `agent_options.script` one by one. The fake reaches the
//! engine only through `sy-signal`, exactly as a real agent's hook does, so
//! orchestration tests drive the production path while staying fast and
//! deterministic. Registered only in `devtools` builds.
//!
//! Its hooks: `status`, `session` and `event`, each with the step's value as
//! JSON on stdin, and each with the fallback exit 0 and empty stdout (also
//! what `sy-fake-agent` passes to `sy-signal`). `approval` holds: its reply
//! is `{"decision":"approve"}` or `{"decision":"deny","message"?}`, and its
//! fallback `{"decision":"ask"}` makes the fake ask in its own terminal. With `quiet_after_ms` or
//! `waiting_pattern` set, the decoder also reads the output, as an adapter
//! for an agent without hooks would.

mod script;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use sy_adapter_api::{
    AdapterError, AgentAdapter, ApprovalRequest, Capabilities, Decision, Detection, ExitInfo,
    ExitOutcome, HookCall, HookOutcome, HookReply, HookResponse, Launch, LaunchFile, LaunchPlan,
    LaunchRequest, LiveStatus, OutputScanner, Probe, ProbeResult, Report, RunDecoder, SignedIn,
    StatusSource, StopPlan, Verdict,
};

pub use script::{Approval, ApprovalAsk, Event, Status, Step};

/// The `approval` hook's fallback stdout: ask in the terminal, never allow.
pub const APPROVAL_FALLBACK: &str = r#"{"decision":"ask"}"#;

/// The script file's name inside the launch dir.
pub const SCRIPT_FILE: &str = "script.json";

/// What the fake types to stop gracefully (Ctrl C), and how long it gets.
const STOP_INPUT: &[u8] = b"\x03";
const STOP_GRACE_MS: u32 = 5000;

/// `workspace.agent_options` for the fake.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    script: Vec<Step>,
    #[serde(default)]
    quiet_after_ms: Option<u32>,
    #[serde(default)]
    waiting_pattern: Option<String>,
}

/// Launches `sy-fake-agent` from `program`, an absolute path the engine
/// found beside its own executable.
#[derive(Debug, Clone)]
pub struct FakeAdapter {
    program: String,
}

impl FakeAdapter {
    /// The adapter for the `sy-fake-agent` binary at `program` (absolute).
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl AgentAdapter for FakeAdapter {
    fn id(&self) -> &str {
        "fake"
    }

    fn display_name(&self) -> &str {
        "Fake agent"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            launch: true,
            approvals: true,
            resume: false,
            events: true,
            status_source: StatusSource::Hooks,
        }
    }

    fn programs(&self) -> Vec<String> {
        vec![self.program.clone()]
    }

    fn detection_probes(&self) -> Vec<Probe> {
        Vec::new()
    }

    fn interpret_detection(&self, _results: &[ProbeResult]) -> Detection {
        Detection {
            installed: true,
            signed_in: SignedIn::Yes,
            version: None,
            hint: None,
        }
    }

    fn launch(&self, req: &LaunchRequest) -> Result<Launch, AdapterError> {
        let options: Options = serde_json::from_value(req.options.clone())
            .map_err(|e| AdapterError::InvalidOptions(format!("fake agent options: {e}")))?;
        script::validate(&options.script).map_err(AdapterError::InvalidOptions)?;
        let contents = serde_json::to_vec(&options.script)
            .map_err(|e| AdapterError::InvalidOptions(e.to_string()))?;
        let script_path = format!("{}/{SCRIPT_FILE}", req.launch_dir.trim_end_matches('/'));
        let plan = LaunchPlan {
            program: self.program.clone(),
            args: vec![
                "--script".to_owned(),
                script_path,
                "--signal".to_owned(),
                req.signal.helper.clone(),
                "--socket".to_owned(),
                req.signal.socket.clone(),
                "--token".to_owned(),
                req.signal.token.clone(),
            ],
            env_set: Vec::new(),
            env_remove: Vec::new(),
            files: vec![LaunchFile {
                rel_path: SCRIPT_FILE.to_owned(),
                contents,
            }],
            initial_input: None,
        };
        Ok(Launch {
            plan,
            decoder: Box::new(FakeDecoder {
                quiet_after_ms: options.quiet_after_ms.map(i64::from),
                waiting_pattern: options.waiting_pattern.filter(|p| !p.is_empty()),
                scanner: OutputScanner::new(),
                last: None,
                asked: 0,
            }),
        })
    }
}

/// The reply to every hook the fake knows, and the fallback for any it
/// doesn't (or can't parse): exit 0, nothing on stdout.
fn quiet_reply() -> HookOutcome {
    HookOutcome {
        reports: Vec::new(),
        response: HookResponse::Reply(HookReply::default()),
    }
}

/// The `approval` hook's fallback, also its answer to a payload it can't read.
fn ask_in_terminal() -> HookOutcome {
    HookOutcome {
        reports: Vec::new(),
        response: HookResponse::Reply(HookReply {
            stdout: APPROVAL_FALLBACK.as_bytes().to_vec(),
            exit_code: 0,
        }),
    }
}

/// A hook's stdin as JSON of type `T`.
fn payload<T: DeserializeOwned>(call: &HookCall) -> Option<T> {
    serde_json::from_slice(&call.stdin).ok()
}

/// Reads a fake run: its hooks, and its output when a pattern option is set.
struct FakeDecoder {
    /// Report `waiting_input` once output has been quiet this long.
    quiet_after_ms: Option<i64>,
    /// Report `waiting_input` while the last text on screen holds this.
    waiting_pattern: Option<String>,
    scanner: OutputScanner,
    /// The last status reported, so each change is reported once.
    last: Option<LiveStatus>,
    /// Approvals asked so far: the next key is `a<asked + 1>`.
    asked: u64,
}

impl FakeDecoder {
    fn reads_output(&self) -> bool {
        self.quiet_after_ms.is_some() || self.waiting_pattern.is_some()
    }

    fn report(&mut self, status: LiveStatus) -> Vec<Report> {
        if self.last == Some(status) {
            return Vec::new();
        }
        self.last = Some(status);
        vec![Report::Status(status)]
    }
}

impl RunDecoder for FakeDecoder {
    fn on_output(&mut self, chunk: &[u8], now_ms: i64) -> Vec<Report> {
        if !self.reads_output() {
            return Vec::new();
        }
        let lines = self.scanner.feed(chunk, now_ms);
        // The last text on screen: the line being written, else the last one finished.
        let last = match self.scanner.partial() {
            "" => lines.last().map(String::as_str),
            partial => Some(partial),
        };
        let Some(last) = last else {
            return Vec::new();
        };
        let waiting = self
            .waiting_pattern
            .as_deref()
            .is_some_and(|pattern| last.contains(pattern));
        self.report(if waiting {
            LiveStatus::WaitingInput
        } else {
            LiveStatus::Running
        })
    }

    fn on_hook(&mut self, call: &HookCall, _now_ms: i64) -> HookOutcome {
        if call.hook == "approval" {
            let Some(ask) = payload::<ApprovalAsk>(call) else {
                return ask_in_terminal();
            };
            let Ok(detail) = script::approval_detail(&ask.kind, &ask.detail) else {
                return ask_in_terminal();
            };
            self.asked += 1;
            return HookOutcome {
                reports: Vec::new(),
                response: HookResponse::Hold(ApprovalRequest {
                    key: format!("a{}", self.asked),
                    summary: ask.summary.clone(),
                    detail,
                    raw: serde_json::to_value(&ask).unwrap_or_default(),
                }),
            };
        }
        let report = match call.hook.as_str() {
            "status" => payload::<Status>(call).map(|status| {
                self.last = Some(status.into());
                Report::Status(status.into())
            }),
            "session" => payload::<String>(call).map(Report::SessionRef),
            "event" => payload::<Event>(call).map(|event| Report::Event(event.into())),
            _ => None,
        };
        HookOutcome {
            reports: report.into_iter().collect(),
            ..quiet_reply()
        }
    }

    fn on_tick(&mut self, now_ms: i64) -> Vec<Report> {
        match self.quiet_after_ms {
            Some(period) if self.scanner.quiet_since(now_ms, period) => {
                self.report(LiveStatus::WaitingInput)
            }
            _ => Vec::new(),
        }
    }

    fn hold_reply(&mut self, _key: &str, decision: &Decision) -> HookReply {
        let reply = match (decision.verdict, &decision.message) {
            (Verdict::Approve, _) => json!({ "decision": "approve" }),
            (Verdict::Deny, None) => json!({ "decision": "deny" }),
            (Verdict::Deny, Some(message)) => json!({ "decision": "deny", "message": message }),
        };
        HookReply {
            stdout: reply.to_string().into_bytes(),
            exit_code: 0,
        }
    }

    fn input_reply(&mut self, _key: &str, _decision: &Decision) -> Vec<u8> {
        Vec::new()
    }

    fn stop(&mut self) -> StopPlan {
        StopPlan {
            input: STOP_INPUT.to_vec(),
            grace_ms: STOP_GRACE_MS,
        }
    }

    fn on_exit(&mut self, exit: &ExitInfo) -> ExitOutcome {
        if exit.code == Some(0) {
            ExitOutcome::Finished
        } else {
            ExitOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sy_adapter_api::{PtySize, SignalEndpoint};

    use super::*;

    fn request(options: Value) -> LaunchRequest {
        LaunchRequest {
            run_id: "run_1".into(),
            prompt: String::new(),
            cwd: "/w".into(),
            options,
            resume: None,
            launch_dir: "/d/agent-runtime/run_1".into(),
            signal: SignalEndpoint {
                helper: "/bin/sy-signal".into(),
                socket: "/tmp/s".into(),
                token: "ab".into(),
            },
            size: PtySize { cols: 80, rows: 24 },
        }
    }

    fn adapter() -> FakeAdapter {
        FakeAdapter::new("/bin/sy-fake-agent")
    }

    #[test]
    fn launch_should_write_the_script_and_run_the_fake_with_its_endpoint() {
        let launch = adapter()
            .launch(&request(
                json!({ "script": [{ "print": "hi\r\n" }, { "exit": 3 }] }),
            ))
            .unwrap();
        let written: Value = serde_json::from_slice(&launch.plan.files[0].contents).unwrap();
        assert_eq!(
            (
                launch.plan.program.as_str(),
                launch.plan.args.join(" "),
                launch.plan.files[0].rel_path.as_str(),
                written,
            ),
            (
                "/bin/sy-fake-agent",
                "--script /d/agent-runtime/run_1/script.json --signal /bin/sy-signal --socket /tmp/s --token ab"
                    .to_owned(),
                "script.json",
                json!([{ "print": "hi\r\n" }, { "exit": 3 }]),
            )
        );
    }

    #[test]
    fn a_malformed_script_should_be_invalid_options() {
        let bad = [
            json!({}),
            json!({ "script": "print" }),
            json!({ "script": [{ "nope": 1 }] }),
            json!({ "script": [{ "write_file": { "path": "../x", "contents": "" } }] }),
            json!({ "script": [{ "write_file": { "path": "/etc/x", "contents": "" } }] }),
            json!({ "script": [{ "die_by_signal": "SIGUSR1" }] }),
            json!({ "script": [], "extra": true }),
        ];
        for options in bad {
            let err = adapter().launch(&request(options.clone())).unwrap_err();
            assert!(
                matches!(err, AdapterError::InvalidOptions(_)),
                "{options}: {err:?}"
            );
        }
    }

    #[test]
    fn exit_zero_should_finish_and_anything_else_should_fail() {
        let mut d = adapter()
            .launch(&request(json!({ "script": [] })))
            .unwrap()
            .decoder;
        let outcomes = [
            ExitInfo {
                code: Some(0),
                signal: None,
            },
            ExitInfo {
                code: Some(3),
                signal: None,
            },
            ExitInfo {
                code: None,
                signal: Some(11),
            },
        ]
        .map(|e| d.on_exit(&e));
        assert_eq!(
            outcomes,
            [
                ExitOutcome::Finished,
                ExitOutcome::Failed,
                ExitOutcome::Failed
            ]
        );
    }

    fn decoder(options: Value) -> Box<dyn RunDecoder> {
        adapter().launch(&request(options)).unwrap().decoder
    }

    fn call(hook: &str, stdin: &str) -> HookCall {
        HookCall {
            hook: hook.into(),
            args: Vec::new(),
            stdin: stdin.as_bytes().to_vec(),
        }
    }

    #[test]
    fn hook_steps_should_parse_as_options() {
        let launch = adapter().launch(&request(json!({ "script": [
            { "status": "waiting_input" },
            { "session": "s-1" },
            { "event": { "message": { "role": "agent", "text": "hi" } } },
            { "event": { "command_run": { "argv": ["ls"], "exit_code": 0 } } },
            { "event": { "file_changed": { "path": "a", "change": "renamed", "from_path": "b" } } },
        ] })));
        assert!(launch.is_ok(), "{launch:?}");
        for bad in [
            json!({ "status": "asleep" }),
            json!({ "event": { "message": { "role": "agent" } } }),
            json!({ "event": { "message": { "role": "agent", "text": "", "extra": 1 } } }),
            json!({ "event": { "file_changed": { "path": "a", "change": "eaten" } } }),
        ] {
            let err = adapter()
                .launch(&request(json!({ "script": [bad] })))
                .unwrap_err();
            assert!(matches!(err, AdapterError::InvalidOptions(_)), "{bad}");
        }
    }

    #[test]
    fn hooks_should_map_to_status_session_and_event_reports() {
        let mut d = decoder(json!({ "script": [] }));
        let outcomes = [
            d.on_hook(&call("status", r#""waiting_input""#), 1),
            d.on_hook(&call("session", r#""s-1""#), 2),
            d.on_hook(
                &call(
                    "event",
                    r#"{"command_run":{"argv":["cargo","test"],"exit_code":1}}"#,
                ),
                3,
            ),
        ];
        assert_eq!(
            outcomes.map(|o| (o.reports, o.response)),
            [
                vec![Report::Status(LiveStatus::WaitingInput)],
                vec![Report::SessionRef("s-1".into())],
                vec![Report::Event(sy_adapter_api::AgentEvent::CommandRun {
                    argv: vec!["cargo".into(), "test".into()],
                    exit_code: Some(1),
                })],
            ]
            .map(|reports| (reports, HookResponse::Reply(HookReply::default())))
        );
    }

    #[test]
    fn an_unknown_hook_or_bad_payload_should_get_the_fallback_with_no_reports() {
        let mut d = decoder(json!({ "script": [] }));
        for c in [
            call("nope", "{}"),
            call("status", "running"),
            call("event", r#"{"message":{}}"#),
            call("session", "42"),
        ] {
            assert_eq!(d.on_hook(&c, 1), quiet_reply(), "{c:?}");
        }
    }

    #[test]
    fn without_pattern_options_output_should_report_nothing() {
        let mut d = decoder(json!({ "script": [] }));
        assert_eq!(
            (d.on_output(b"Allow? [y/n] ", 1), d.on_tick(100_000)),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn a_waiting_pattern_split_across_chunks_with_escapes_should_report_waiting() {
        let mut d = decoder(json!({ "script": [], "waiting_pattern": "[y/n]" }));
        let reports = [
            d.on_output(b"working\r\n", 1),
            d.on_output(b"Allow? \x1b[1m[y", 2),
            d.on_output(b"\x1b[0m/n] ", 3),
            // The same screen again: no new report.
            d.on_output(b"\x1b[0m", 4),
            d.on_output(b"y\r\nok, running it\r\n", 5),
        ];
        let running = vec![Report::Status(LiveStatus::Running)];
        assert_eq!(
            reports,
            [
                running.clone(),
                Vec::new(),
                vec![Report::Status(LiveStatus::WaitingInput)],
                Vec::new(),
                running,
            ]
        );
    }

    #[test]
    fn quiet_output_should_report_waiting_only_after_the_period_then_running_on_new_output() {
        let mut d = decoder(json!({ "script": [], "quiet_after_ms": 500 }));
        let before_output = d.on_tick(10_000);
        let first = d.on_output(b"thinking", 10_000);
        let reports = [
            d.on_tick(10_250),
            d.on_tick(10_499),
            d.on_tick(10_500),
            d.on_tick(10_750),
            d.on_output(b"more", 11_000),
            d.on_tick(11_250),
        ];
        assert_eq!(
            (before_output, first, reports),
            (
                Vec::new(),
                vec![Report::Status(LiveStatus::Running)],
                [
                    Vec::new(),
                    Vec::new(),
                    vec![Report::Status(LiveStatus::WaitingInput)],
                    Vec::new(),
                    vec![Report::Status(LiveStatus::Running)],
                    Vec::new(),
                ]
            )
        );
    }

    #[test]
    fn an_approval_hook_should_hold_with_a_fresh_key_and_reply_with_the_decision() {
        let mut d = decoder(json!({ "script": [] }));
        let ask = r#"{"kind":"command","summary":"run ls","detail":{"command":"ls","cwd":"/w"}}"#;
        let (first, second) = (
            d.on_hook(&call("approval", ask), 1),
            d.on_hook(&call("approval", ask), 2),
        );
        let HookResponse::Hold(request) = first.response else {
            panic!("{first:?}")
        };
        let HookResponse::Hold(again) = second.response else {
            panic!("{second:?}")
        };
        let deny = d.hold_reply(
            "a1",
            &Decision {
                verdict: Verdict::Deny,
                message: Some("no".into()),
            },
        );
        let approve = d.hold_reply(
            "a1",
            &Decision {
                verdict: Verdict::Approve,
                message: Some("ignored".into()),
            },
        );
        assert_eq!(
            (
                request.key.as_str(),
                again.key.as_str(),
                request.summary.as_str(),
                request.detail,
                request.raw["detail"]["cwd"].clone(),
                String::from_utf8(deny.stdout).unwrap(),
                String::from_utf8(approve.stdout).unwrap(),
            ),
            (
                "a1",
                "a2",
                "run ls",
                sy_adapter_api::ApprovalDetail::Command {
                    command: "ls".into(),
                    cwd: Some("/w".into())
                },
                json!("/w"),
                r#"{"decision":"deny","message":"no"}"#.to_owned(),
                r#"{"decision":"approve"}"#.to_owned(),
            )
        );
    }

    #[test]
    fn an_unreadable_approval_should_get_the_ask_fallback() {
        let mut d = decoder(json!({ "script": [] }));
        for stdin in [
            "",
            r#"{"kind":"command","summary":"x","detail":{}}"#,
            r#"{"kind":"dance","summary":"x"}"#,
        ] {
            assert_eq!(d.on_hook(&call("approval", stdin), 1), ask_in_terminal());
        }
    }

    #[test]
    fn approval_steps_should_validate_their_detail_and_branches() {
        let good = json!({ "script": [{ "approval": {
            "kind": "file_edit", "summary": "edit", "detail": { "path": "a.rs" },
            "on_approve": [{ "print": "ok" }], "on_deny": [{ "exit": 1 }],
        } }] });
        assert!(adapter().launch(&request(good)).is_ok());
        for bad in [
            json!({ "kind": "fetch", "summary": "s", "detail": {} }),
            json!({ "kind": "nope", "summary": "s" }),
            json!({ "kind": "other", "summary": "s", "detail": { "tool": "t" },
                    "on_deny": [{ "write_file": { "path": "../x", "contents": "" } }] }),
        ] {
            let err = adapter()
                .launch(&request(json!({ "script": [{ "approval": bad }] })))
                .unwrap_err();
            assert!(matches!(err, AdapterError::InvalidOptions(_)), "{bad}");
        }
    }

    /// The tick reports of a decoder with `quiet_after_ms`, after output at
    /// 10 000 ms, ticked at each of `ticks`.
    fn quiet_ticks(quiet_after_ms: Option<u32>, ticks: &[i64]) -> Vec<Vec<Report>> {
        let mut options = json!({ "script": [] });
        if let Some(ms) = quiet_after_ms {
            options["quiet_after_ms"] = json!(ms);
        }
        let mut d = decoder(options);
        d.on_output(b"thinking", 10_000);
        ticks.iter().map(|&now| d.on_tick(now)).collect()
    }

    // covers: AC-6 (verify: quiet timer now and period, 500 versus 2000 on one clock)
    #[test]
    fn the_quiet_period_should_decide_when_waiting_input_comes_on_the_same_clock() {
        let ticks = [10_499, 10_500, 11_999, 12_000];
        let waiting = vec![Report::Status(LiveStatus::WaitingInput)];
        assert_eq!(
            (
                quiet_ticks(Some(500), &ticks),
                quiet_ticks(Some(2000), &ticks),
                quiet_ticks(None, &ticks),
            ),
            (
                vec![Vec::new(), waiting.clone(), Vec::new(), Vec::new()],
                vec![Vec::new(), Vec::new(), Vec::new(), waiting],
                vec![Vec::new(); 4],
            )
        );
    }

    // covers: AC-6 (a status hook and the screen agree: one report per change)
    #[test]
    fn a_status_hook_should_count_as_the_last_report_so_the_screen_does_not_repeat_it() {
        let mut d = decoder(json!({ "script": [], "waiting_pattern": "[y/n]" }));
        let hook = d.on_hook(&call("status", r#""waiting_input""#), 1).reports;
        let same_screen = d.on_output(b"Allow? [y/n] ", 2);
        let new_output = d.on_output(b"y\r\nworking\r\n", 3);
        assert_eq!(
            (hook, same_screen, new_output),
            (
                vec![Report::Status(LiveStatus::WaitingInput)],
                Vec::new(),
                vec![Report::Status(LiveStatus::Running)],
            )
        );
    }

    #[test]
    fn an_empty_waiting_pattern_should_mean_no_output_reading() {
        let mut d = decoder(json!({ "script": [], "waiting_pattern": "" }));
        assert_eq!(d.on_output(b"anything at all\r\n", 1), Vec::new());
    }

    // covers: AC-9 (value sourcing: each kind's typed detail follows the variant)
    #[test]
    fn every_approval_kind_should_map_to_its_own_typed_detail() {
        use sy_adapter_api::ApprovalDetail as D;
        let input = json!({ "q": 1 });
        let cases = [
            (
                "command",
                json!({ "command": "rm -rf build" }),
                D::Command {
                    command: "rm -rf build".into(),
                    cwd: None,
                },
            ),
            (
                "file_edit",
                json!({ "path": "a.rs", "diff": "+x" }),
                D::FileEdit {
                    path: "a.rs".into(),
                    diff: Some("+x".into()),
                },
            ),
            (
                "fetch",
                json!({ "url": "https://example.com" }),
                D::Fetch {
                    url: "https://example.com".into(),
                },
            ),
            (
                "mcp_tool",
                json!({ "server": "db", "tool": "query", "input": input }),
                D::McpTool {
                    server: "db".into(),
                    tool: "query".into(),
                    input: input.clone(),
                },
            ),
            (
                "other",
                json!({ "tool": "Task" }),
                D::Other {
                    tool: "Task".into(),
                    input: Value::Null,
                },
            ),
        ];
        for (kind, detail, want) in cases {
            let got = script::approval_detail(kind, &detail).unwrap();
            assert_eq!((got.kind(), &got), (kind, &want), "{kind}");
        }
    }

    #[test]
    fn approval_detail_should_name_the_missing_or_mistyped_field() {
        assert_eq!(
            [
                script::approval_detail("mcp_tool", &json!({ "server": "db" })),
                script::approval_detail("command", &json!({ "command": 7 })),
            ],
            [
                Err("detail.tool must be a string".to_owned()),
                Err("detail.command must be a string".to_owned()),
            ]
        );
    }

    #[test]
    fn validate_should_refuse_false_flags_bad_flood_rates_and_escaping_deletes() {
        let max = script::MAX_FLOOD_BYTES_PER_SEC;
        let bad = [
            json!({ "hang": false }),
            json!({ "ignore_sighup": false }),
            json!({ "flood": { "bytes_per_sec": 0 } }),
            json!({ "flood": { "bytes_per_sec": max + 1 } }),
            json!({ "delete_file": { "path": "a/../../x" } }),
            json!({ "delete_file": { "path": "" } }),
        ];
        for step in bad {
            let err = adapter()
                .launch(&request(json!({ "script": [{ "print": "x" }, step] })))
                .unwrap_err();
            let AdapterError::InvalidOptions(message) = &err else {
                panic!("{step}: {err:?}")
            };
            assert!(message.starts_with("step 1: "), "{step}: {message}");
        }
        let good = json!({ "script": [
            { "hang": true },
            { "ignore_sighup": true },
            { "flood": { "bytes_per_sec": max, "total_bytes": 10 } },
            { "delete_file": { "path": "dir/file..txt" } },
        ] });
        assert!(adapter().launch(&request(good)).is_ok());
    }

    #[test]
    fn a_launch_dir_with_a_trailing_slash_should_not_double_it() {
        let mut req = request(json!({ "script": [] }));
        req.launch_dir = "/d/agent-runtime/run_1/".into();
        let launch = adapter().launch(&req).unwrap();
        assert_eq!(launch.plan.args[1], "/d/agent-runtime/run_1/script.json");
    }

    // covers: AC-2 (the fake needs nothing but its own program; it probes nothing)
    #[test]
    fn the_fake_should_list_only_its_program_and_be_installed_without_probes() {
        let a = adapter();
        assert_eq!(
            (
                a.id(),
                a.programs(),
                a.detection_probes(),
                a.interpret_detection(&[]).installed,
                a.capabilities().status_source,
            ),
            (
                "fake",
                vec!["/bin/sy-fake-agent".to_owned()],
                Vec::new(),
                true,
                StatusSource::Hooks,
            )
        );
    }

    #[test]
    fn stop_should_type_ctrl_c_with_a_5_second_grace() {
        let mut d = adapter()
            .launch(&request(json!({ "script": [] })))
            .unwrap()
            .decoder;
        assert_eq!(
            d.stop(),
            StopPlan {
                input: vec![3],
                grace_ms: 5000
            }
        );
    }
}
