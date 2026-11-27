//! The start path and the driver against port doubles and a scripted decoder.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sy_adapter_api::{
    AdapterError, AgentAdapter, Capabilities, Decision, Detection, ExitInfo, ExitOutcome, HookCall,
    HookOutcome, HookReply, HookResponse, Launch, LaunchFile, LaunchPlan, LaunchRequest,
    LiveStatus, Probe, ProbeResult, PtySize, Report, RunDecoder, SignedIn, StatusSource, StopPlan,
};

use super::ports::*;
use super::*;
use crate::ids::{ApprovalId, ProjectId, TaskId, WorkspaceId};
use crate::model::{LimitKind, LogState, MessageRole, WorkspaceState};
use crate::ports::{RunLog, RunLogError, TrackedProcess};
use supervise::LimitOverride;

// ---- port doubles ----

#[derive(Default)]
struct MemLog(Mutex<HashMap<RunId, Vec<u8>>>);

impl RunLog for MemLog {
    fn append(&self, run: &RunId, offset: u64, bytes: &[u8]) -> Result<(), RunLogError> {
        let mut files = lock(&self.0);
        let file = files.entry(run.clone()).or_default();
        file.truncate(usize::try_from(offset).unwrap());
        file.extend_from_slice(bytes);
        Ok(())
    }
    fn truncate(&self, _: &RunId, _: u64) -> Result<(), RunLogError> {
        Ok(())
    }
    fn read(&self, _: &RunId, _: u64, _: usize) -> Result<Vec<u8>, RunLogError> {
        Ok(Vec::new())
    }
    fn len(&self, run: &RunId) -> Result<u64, RunLogError> {
        Ok(lock(&self.0).get(run).map_or(0, |f| f.len() as u64))
    }
    fn delete(&self, _: &RunId) -> Result<(), RunLogError> {
        Ok(())
    }
    fn list(&self) -> Result<Vec<(RunId, u64)>, RunLogError> {
        Ok(Vec::new())
    }
}

/// Runs in memory, through the real status machine.
#[derive(Default)]
struct MemWrites {
    runs: Mutex<HashMap<RunId, Run>>,
    moves: Mutex<Vec<(RunStatus, Actor)>>,
    events: Mutex<Vec<(Actor, Option<String>, AdapterEvent)>>,
    approvals: Mutex<HashMap<ApprovalId, Approval>>,
    /// Every status move and approval change, in order.
    trail: Mutex<Vec<String>>,
    /// Make `create_approval` fail as a full disk would.
    refuse_creates: AtomicBool,
    /// Global settings.
    settings: Mutex<BTreeMap<String, JsonValue>>,
    /// Every project's settings; `None` makes the read fail.
    project_settings: Mutex<Option<BTreeMap<String, JsonValue>>>,
    /// `run_process` rows: (pid, started_at) to (name, was the main insert).
    processes: Mutex<HashMap<(i64, i64), (String, bool)>>,
    /// Make `track_processes` fail as a full disk would.
    refuse_tracking: AtomicBool,
    /// Panic right after a terminal transition lands.
    panic_after_end: AtomicBool,
}

impl MemWrites {
    fn run(&self, id: &RunId) -> Run {
        lock(&self.runs)[id].clone()
    }
    fn statuses(&self) -> Vec<RunStatus> {
        lock(&self.moves).iter().map(|(s, _)| *s).collect()
    }
    fn approval(&self, id: &ApprovalId) -> Approval {
        lock(&self.approvals)[id].clone()
    }
    fn only_approval(&self) -> Approval {
        let all = lock(&self.approvals);
        assert_eq!(all.len(), 1, "{all:?}");
        all.values().next().unwrap().clone()
    }
    fn trail(&self) -> Vec<String> {
        lock(&self.trail).clone()
    }
}

#[async_trait::async_trait]
impl RunWrites for MemWrites {
    async fn create_run(&self, _actor: Actor, new: NewRun) -> Result<Run, StoreError> {
        let run = Run {
            id: new.id.clone(),
            workspace_id: new.workspace_id,
            kind: new.kind,
            input: new.input,
            resumes_run_id: new.resumes_run_id,
            agent_session_ref: None,
            status: RunStatus::Queued,
            exit_code: None,
            end_reason: None,
            pid: None,
            pgid: None,
            pid_started_at: None,
            queued_at: 1,
            started_at: None,
            ended_at: None,
            log_bytes: 0,
            log_state: LogState::Present,
        };
        lock(&self.runs).insert(new.id, run.clone());
        Ok(run)
    }
    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError> {
        lock(&self.runs)
            .get(run)
            .cloned()
            .ok_or(StoreError::NotFound)
    }
    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Run, StoreError> {
        let mut runs = lock(&self.runs);
        let row = runs.get_mut(run).ok_or(StoreError::NotFound)?;
        if let Some(next) = row.transition(change, 2)? {
            *row = next;
            lock(&self.moves).push((row.status, actor));
            lock(&self.trail).push(format!("run {}", row.status));
        }
        let row = row.clone();
        drop(runs);
        if row.status.is_terminal() && self.panic_after_end.load(Ordering::SeqCst) {
            panic!("the store panics after the end, on purpose");
        }
        Ok(row)
    }
    async fn set_run_process(&self, run: &RunId, p: RunProcess) -> Result<Run, StoreError> {
        let mut runs = lock(&self.runs);
        let row = runs.get_mut(run).ok_or(StoreError::NotFound)?;
        (row.pid, row.pgid, row.pid_started_at) = (Some(p.pid), Some(p.pgid), p.pid_started_at);
        Ok(row.clone())
    }
    async fn set_agent_session_ref(&self, run: &RunId, session: &str) -> Result<(), StoreError> {
        let mut runs = lock(&self.runs);
        runs.get_mut(run)
            .ok_or(StoreError::NotFound)?
            .agent_session_ref = Some(session.into());
        Ok(())
    }
    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        _run: &RunId,
        event: AdapterEvent,
    ) -> Result<(), StoreError> {
        lock(&self.events).push((actor, actor_ref, event));
        Ok(())
    }
    async fn create_approval(
        &self,
        _actor: Actor,
        _actor_ref: Option<String>,
        new: NewApproval,
    ) -> Result<Committed<Approval>, StoreError> {
        if self.refuse_creates.load(Ordering::SeqCst) {
            return Err(StoreError::Full);
        }
        let workspace_id = lock(&self.runs)[&new.run_id].workspace_id.clone();
        let approval = Approval {
            id: new.id,
            run_id: new.run_id,
            workspace_id,
            kind: new.kind,
            summary: new.summary,
            detail: new.detail,
            status: ApprovalStatus::Pending,
            requested_at: 1,
            decided_at: None,
            decided_by: None,
            decided_by_ref: None,
            message: None,
        };
        lock(&self.approvals).insert(approval.id.clone(), approval.clone());
        lock(&self.trail).push(format!("requested {}", approval.summary));
        Ok(Committed {
            value: approval,
            events: Vec::new(),
        })
    }
    async fn decide_approval(
        &self,
        _actor: Actor,
        id: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Committed<Approval>, StoreError> {
        let mut all = lock(&self.approvals);
        let row = all.get_mut(id).ok_or(StoreError::NotFound)?;
        *row = row.decide(&decision, 3)?;
        lock(&self.trail).push(format!("{} {}", row.summary, row.status));
        Ok(Committed {
            value: row.clone(),
            events: Vec::new(),
        })
    }
    async fn get_approval(&self, id: &ApprovalId) -> Result<Approval, StoreError> {
        lock(&self.approvals)
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)
    }
    async fn cancel_pending_approvals(
        &self,
        _actor: Actor,
        run: &RunId,
        message: Option<String>,
    ) -> Result<Committed<Vec<Approval>>, StoreError> {
        let mut all = lock(&self.approvals);
        let mut cancelled = Vec::new();
        for row in all.values_mut() {
            if &row.run_id == run && row.status == ApprovalStatus::Pending {
                *row = row.decide(&ApprovalDecision::cancelled(message.clone()), 4)?;
                lock(&self.trail).push(format!("{} {}", row.summary, row.status));
                cancelled.push(row.clone());
            }
        }
        Ok(Committed {
            value: cancelled,
            events: Vec::new(),
        })
    }
    async fn project_settings(
        &self,
        _: &ProjectId,
    ) -> Result<BTreeMap<String, JsonValue>, StoreError> {
        lock(&self.project_settings)
            .clone()
            .ok_or(StoreError::NotFound)
    }
    async fn get_setting(&self, key: &str) -> Result<Option<JsonValue>, StoreError> {
        Ok(lock(&self.settings).get(key).cloned())
    }
    async fn track_processes(
        &self,
        _: &RunId,
        rows: &[TrackedProcess],
        main: bool,
    ) -> Result<(), StoreError> {
        if self.refuse_tracking.load(Ordering::SeqCst) {
            return Err(StoreError::Full);
        }
        let mut all = lock(&self.processes);
        for row in rows {
            all.entry((row.pid, row.started_at))
                .or_insert((row.name.clone(), main));
        }
        Ok(())
    }
    async fn untrack_processes(&self, _: &RunId, keys: &[(i64, i64)]) -> Result<(), StoreError> {
        let mut all = lock(&self.processes);
        for key in keys {
            all.remove(key);
        }
        Ok(())
    }
}

/// A terminal fed by the test: each chunk sent is one read; dropping the sender is EOF.
struct ChanReader {
    chunks: std_mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

/// A chunk that makes the reader thread panic, as a supervision bug would.
const READER_PANIC: &[u8] = b"<reader panics>";

impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.chunks.recv() {
                Ok(chunk) if chunk == READER_PANIC => panic!("the reader panics on purpose"),
                Ok(chunk) => self.pending = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

struct MemIo(Arc<Mutex<Vec<u8>>>);

impl PtyIo for MemIo {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        lock(&self.0).extend_from_slice(data);
        Ok(())
    }
    fn resize(&mut self, _: PtySize) -> std::io::Result<()> {
        Ok(())
    }
}

/// A process that exits when the test says, or on a signal it doesn't ignore.
struct MemProcess {
    exit: Mutex<std_mpsc::Receiver<ExitInfo>>,
    die: std_mpsc::Sender<ExitInfo>,
    signals: Arc<Mutex<Vec<GroupSignal>>>,
    ignore_hangup: bool,
    /// Set by `reap`; group signals are refused after it.
    reaped: Arc<AtomicBool>,
    /// The table it is in, so a group SIGKILL also kills its group's leftovers.
    table: Option<Arc<MemTable>>,
    pid: i64,
}

impl ChildProcess for MemProcess {
    fn wait(&self) -> std::io::Result<ExitInfo> {
        lock(&self.exit)
            .recv()
            .map_err(|_| std::io::Error::other("gone"))
    }
    fn reap(&self) {
        if let Some(table) = &self.table {
            lock(&table.trail).push(format!("reap {}", self.pid));
        }
        self.reaped.store(true, Ordering::SeqCst);
    }
    fn signal_group(&self, signal: GroupSignal) -> bool {
        if self.reaped.load(Ordering::SeqCst) {
            return false;
        }
        lock(&self.signals).push(signal);
        if let Some(table) = &self.table {
            table.group_signal(self.pid, signal);
        }
        let dies = match signal {
            GroupSignal::Hangup => (!self.ignore_hangup).then_some(1),
            GroupSignal::Kill => Some(9),
            GroupSignal::Cont | GroupSignal::Term => None,
        };
        if let Some(n) = dies {
            let _ = self.die.send(ExitInfo {
                code: None,
                signal: Some(n),
            });
        }
        true
    }
}

/// A process table the test fills. Each process may ignore SIGTERM; a
/// group signal reaches every process whose `pgid` (kept here) matches.
#[derive(Default)]
struct MemTable {
    procs: Mutex<Vec<(ProcInfo, i64)>>,
    ignore_term: Mutex<HashSet<i64>>,
    /// Never die, even on SIGKILL (stuck in the kernel).
    unkillable: Mutex<HashSet<i64>>,
    fail_scans: AtomicBool,
    /// How many scans were asked for.
    scans: std::sync::atomic::AtomicUsize,
    /// Every signal and reap, in order: `term 101`, `kill 101`, `reap 4242`, ...
    trail: Mutex<Vec<String>>,
    /// When the main process (pid 4242) started; `None` reads it as absent.
    main_started: Mutex<Option<i64>>,
}

const MAIN_PID: i64 = 4242;
const MAIN_START: i64 = 5_000_000;

impl MemTable {
    fn with_main() -> Self {
        let t = Self::default();
        *lock(&t.main_started) = Some(MAIN_START);
        t
    }
    /// A child of the main process, in its session and group.
    fn add(&self, pid: i64, sid: i64, pgid: i64, started: i64) {
        lock(&self.procs).push((
            ProcInfo {
                pid,
                ppid: Some(MAIN_PID),
                sid: Some(sid),
                started_at_ms: started,
                uid_is_self: true,
                zombie: false,
                name: format!("child{pid}"),
            },
            pgid,
        ));
    }
    fn alive(&self, pid: i64) -> bool {
        lock(&self.procs).iter().any(|(p, _)| p.pid == pid)
    }
    fn hit(&self, pid: i64, what: &str, term: bool) {
        lock(&self.trail).push(format!("{what} {pid}"));
        if lock(&self.unkillable).contains(&pid) {
            return;
        }
        if !term || !lock(&self.ignore_term).contains(&pid) {
            lock(&self.procs).retain(|(p, _)| p.pid != pid);
        }
    }
    fn group_signal(&self, pgid: i64, signal: GroupSignal) {
        let members: Vec<i64> = lock(&self.procs)
            .iter()
            .filter(|(_, g)| *g == pgid)
            .map(|(p, _)| p.pid)
            .collect();
        for pid in members {
            match signal {
                GroupSignal::Kill | GroupSignal::Hangup => self.hit(pid, "group-kill", false),
                GroupSignal::Term => self.hit(pid, "group-term", true),
                GroupSignal::Cont => {}
            }
        }
    }
}

impl ProcessTable for MemTable {
    fn scan(&self) -> Result<Vec<ProcInfo>, ProcError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        if self.fail_scans.load(Ordering::SeqCst) {
            return Err(ProcError("scripted".into()));
        }
        let mut all: Vec<ProcInfo> = lock(&self.procs).iter().map(|(p, _)| p.clone()).collect();
        if let Some(started) = *lock(&self.main_started) {
            all.push(ProcInfo {
                pid: MAIN_PID,
                ppid: Some(1),
                sid: Some(MAIN_PID),
                started_at_ms: started,
                uid_is_self: true,
                zombie: false,
                name: "agent".into(),
            });
        }
        Ok(all)
    }
    fn started_at(&self, pid: i64) -> Option<ProcState> {
        if pid == MAIN_PID {
            return lock(&self.main_started).map(|started_at_ms| ProcState {
                started_at_ms,
                zombie: false,
            });
        }
        lock(&self.procs)
            .iter()
            .find(|(p, _)| p.pid == pid)
            .map(|(p, _)| ProcState {
                started_at_ms: p.started_at_ms,
                zombie: p.zombie,
            })
    }
    fn signal(&self, pid: i64, started_at_ms: i64, signal: ProcSignal) -> SignalOutcome {
        let matches = pid > 1
            && pid != MAIN_PID
            && lock(&self.procs)
                .iter()
                .any(|(p, _)| p.pid == pid && p.started_at_ms == started_at_ms && p.uid_is_self);
        if !matches {
            return SignalOutcome::Gone;
        }
        match signal {
            ProcSignal::Cont => lock(&self.trail).push(format!("cont {pid}")),
            ProcSignal::Term => self.hit(pid, "term", true),
            ProcSignal::Kill => self.hit(pid, "kill", false),
        }
        SignalOutcome::Sent
    }
    fn signal_group(&self, pgid: i64, _: ProcSignal) -> bool {
        lock(&self.trail).push(format!("killpg {pgid}"));
        true
    }
    fn engine_pid(&self) -> i64 {
        1
    }
}

/// The test's end of one spawned child.
struct ChildHandle {
    output: Option<std_mpsc::Sender<Vec<u8>>>,
    exit: std_mpsc::Sender<ExitInfo>,
    input: Arc<Mutex<Vec<u8>>>,
    signals: Arc<Mutex<Vec<GroupSignal>>>,
}

impl ChildHandle {
    fn print(&self, text: &str) {
        self.output
            .as_ref()
            .unwrap()
            .send(text.as_bytes().to_vec())
            .unwrap();
    }
    fn exit(&mut self, code: i32) {
        self.output = None;
        self.exit
            .send(ExitInfo {
                code: Some(code),
                signal: None,
            })
            .unwrap();
    }
}

#[derive(Default)]
struct FakePty {
    spawned: Mutex<Vec<SpawnRequest>>,
    children: Mutex<Vec<ChildHandle>>,
    ignore_hangup: bool,
    table: Option<Arc<MemTable>>,
}

impl Pty for FakePty {
    fn spawn(&self, req: SpawnRequest) -> Result<PtyChild, PtyError> {
        lock(&self.spawned).push(req);
        let (out_tx, out_rx) = std_mpsc::channel();
        let (exit_tx, exit_rx) = std_mpsc::channel();
        let input = Arc::new(Mutex::new(Vec::new()));
        let signals = Arc::new(Mutex::new(Vec::new()));
        lock(&self.children).push(ChildHandle {
            output: Some(out_tx),
            exit: exit_tx.clone(),
            input: Arc::clone(&input),
            signals: Arc::clone(&signals),
        });
        Ok(PtyChild {
            pid: MAIN_PID,
            pgid: MAIN_PID,
            reader: Box::new(ChanReader {
                chunks: out_rx,
                pending: Vec::new(),
            }),
            io: Box::new(MemIo(input)),
            process: Arc::new(MemProcess {
                exit: Mutex::new(exit_rx),
                die: exit_tx,
                signals,
                ignore_hangup: self.ignore_hangup,
                reaped: Arc::new(AtomicBool::new(false)),
                table: self.table.clone(),
                pid: MAIN_PID,
            }),
        })
    }
}

#[derive(Default)]
struct MemRuntime {
    live: Mutex<HashSet<RunId>>,
    files: Mutex<Vec<(String, Vec<u8>)>>,
    removed: Mutex<Vec<RunId>>,
}

impl RuntimeDir for MemRuntime {
    fn root(&self) -> String {
        "/data/agent-runtime".into()
    }
    fn path(&self, run: &RunId) -> String {
        format!("/data/agent-runtime/{run}")
    }
    fn create(&self, run: &RunId) -> std::io::Result<String> {
        lock(&self.live).insert(run.clone());
        Ok(self.path(run))
    }
    fn write_file(&self, _: &RunId, rel: &str, contents: &[u8]) -> std::io::Result<()> {
        lock(&self.files).push((rel.to_owned(), contents.to_vec()));
        Ok(())
    }
    fn remove(&self, run: &RunId) -> std::io::Result<()> {
        lock(&self.live).remove(run);
        lock(&self.removed).push(run.clone());
        Ok(())
    }
    fn sweep(&self) -> std::io::Result<usize> {
        Ok(0)
    }
}

struct MemLogin;

#[async_trait::async_trait]
impl LoginEnv for MemLogin {
    async fn snapshot(&self) -> Arc<LoginEnvSnapshot> {
        Arc::new(LoginEnvSnapshot {
            vars: vec![("PATH".into(), "/usr/bin".into())],
            fallback: None,
        })
    }
    async fn refresh(&self) -> Arc<LoginEnvSnapshot> {
        self.snapshot().await
    }
    fn current(&self) -> Option<Arc<LoginEnvSnapshot>> {
        None
    }
    fn resolve(&self, _: &LoginEnvSnapshot, program: &str) -> Option<String> {
        (program == "agent").then(|| "/usr/bin/agent".to_owned())
    }
}

struct Counter(Mutex<u32>);

impl TokenSource for Counter {
    fn mint(&self) -> String {
        let mut n = lock(&self.0);
        *n += 1;
        format!("{:032x}", *n)
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_000
    }
}

// ---- a scripted adapter ----

#[derive(Clone)]
struct Scripted {
    program: String,
    files: Vec<LaunchFile>,
    error: Option<AdapterError>,
    grace_ms: u32,
}

impl Default for Scripted {
    fn default() -> Self {
        Self {
            program: "agent".into(),
            files: vec![LaunchFile {
                rel_path: "script.json".into(),
                contents: b"[]".to_vec(),
            }],
            error: None,
            grace_ms: 0,
        }
    }
}

impl AgentAdapter for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    fn display_name(&self) -> &str {
        "Scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            launch: true,
            approvals: false,
            resume: false,
            events: false,
            status_source: StatusSource::Patterns,
        }
    }
    fn programs(&self) -> Vec<String> {
        vec!["agent".into()]
    }
    fn detection_probes(&self) -> Vec<Probe> {
        Vec::new()
    }
    fn interpret_detection(&self, _: &[ProbeResult]) -> Detection {
        Detection {
            installed: true,
            signed_in: SignedIn::Unknown,
            version: None,
            hint: None,
        }
    }
    fn launch(&self, _req: &LaunchRequest) -> Result<Launch, AdapterError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        Ok(Launch {
            plan: LaunchPlan {
                program: self.program.clone(),
                args: vec!["--go".into()],
                files: self.files.clone(),
                initial_input: Some(b"hello\r".to_vec()),
                ..LaunchPlan::default()
            },
            decoder: Box::new(Words {
                grace_ms: self.grace_ms,
            }),
        })
    }
}

/// What [`Words`] answers a hook it knows with.
const HOOK_OK: &[u8] = b"ok";

/// Reports `waiting_input` on "WAIT" and `running` on "GO", in output and in
/// a `status` hook's stdin. Hooks: `event` journals its stdin as a message,
/// `session` saves it, `hold` asks for an approval keyed by its stdin;
/// anything else gets the fallback (exit 2). In output, `ASK:<key>` is a
/// typed request and `YES:`, `NO:`, `HUH:<key>` say it was answered in the
/// terminal. Held replies are `approve` or `deny:<message>`; typed ones are
/// `y\r` or `n\r`.
struct Words {
    grace_ms: u32,
}

impl RunDecoder for Words {
    fn on_output(&mut self, chunk: &[u8], _: i64) -> Vec<Report> {
        let text = String::from_utf8_lossy(chunk);
        let mut reports = Vec::new();
        for word in text.split_whitespace() {
            let settled = |outcome| Report::ApprovalSettled {
                key: word
                    .split_once(':')
                    .map(|(_, k)| k.to_owned())
                    .unwrap_or_default(),
                outcome,
            };
            match word.split_once(':').map(|(tag, _)| tag) {
                Some("ASK") => reports.push(Report::ApprovalRequested(ask(&word[4..]))),
                Some("YES") => reports.push(settled(sy_adapter_api::SettledOutcome::Approved)),
                Some("NO") => reports.push(settled(sy_adapter_api::SettledOutcome::Denied)),
                Some("HUH") => reports.push(settled(sy_adapter_api::SettledOutcome::Unknown)),
                _ => {}
            }
        }
        if text.contains("WAIT") {
            reports.push(Report::Status(LiveStatus::WaitingInput));
        }
        if text.contains("GO") {
            reports.push(Report::Status(LiveStatus::Running));
        }
        reports
    }
    fn on_hook(&mut self, call: &HookCall, now: i64) -> HookOutcome {
        let text = String::from_utf8_lossy(&call.stdin).into_owned();
        let reply = |reports| HookOutcome {
            reports,
            response: HookResponse::Reply(HookReply {
                stdout: HOOK_OK.to_vec(),
                exit_code: 0,
            }),
        };
        match call.hook.as_str() {
            "status" => reply(self.on_output(call.stdin.as_slice(), now)),
            "event" => reply(vec![Report::Event(sy_adapter_api::AgentEvent::Message {
                role: sy_adapter_api::MessageRole::Agent,
                text,
            })]),
            "session" => reply(vec![Report::SessionRef(text)]),
            "panic" => panic!("the decoder panics on purpose"),
            "hold" => HookOutcome {
                reports: Vec::new(),
                response: HookResponse::Hold(ask(&text)),
            },
            _ => HookOutcome {
                reports: Vec::new(),
                response: HookResponse::Reply(HookReply {
                    stdout: Vec::new(),
                    exit_code: 2,
                }),
            },
        }
    }
    fn on_tick(&mut self, _: i64) -> Vec<Report> {
        Vec::new()
    }
    fn hold_reply(&mut self, _: &str, decision: &Decision) -> HookReply {
        let stdout = match (decision.verdict, &decision.message) {
            (sy_adapter_api::Verdict::Approve, _) => "approve".to_owned(),
            (sy_adapter_api::Verdict::Deny, m) => format!("deny:{}", m.as_deref().unwrap_or("")),
        };
        HookReply {
            stdout: stdout.into_bytes(),
            exit_code: 0,
        }
    }
    fn input_reply(&mut self, _: &str, decision: &Decision) -> Vec<u8> {
        match decision.verdict {
            sy_adapter_api::Verdict::Approve => b"y\r".to_vec(),
            sy_adapter_api::Verdict::Deny => b"n\r".to_vec(),
        }
    }
    fn stop(&mut self) -> StopPlan {
        StopPlan {
            input: b"\x03".to_vec(),
            grace_ms: self.grace_ms,
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

/// A request keyed (and summarised) by `key`.
fn ask(key: &str) -> sy_adapter_api::ApprovalRequest {
    sy_adapter_api::ApprovalRequest {
        key: key.into(),
        summary: key.into(),
        detail: sy_adapter_api::ApprovalDetail::Command {
            command: "ls".into(),
            cwd: None,
        },
        raw: serde_json::Value::Null,
    }
}

// ---- harness ----

struct Harness {
    runs: Arc<AgentRuns>,
    table: Arc<MemTable>,
    notices: tokio::sync::Mutex<mpsc::UnboundedReceiver<ApprovalNotice>>,
    store: Arc<MemWrites>,
    pty: Arc<FakePty>,
    runtime: Arc<MemRuntime>,
    log: Arc<MemLog>,
    hub: Arc<RunHub>,
}

fn harness_with(adapter: Scripted, pty: FakePty) -> Harness {
    harness_on(adapter, pty, MemTable::with_main())
}

fn harness_on(adapter: Scripted, mut pty: FakePty, table: MemTable) -> Harness {
    let log = Arc::new(MemLog::default());
    let hub = RunHub::new(log.clone());
    let table = Arc::new(table);
    pty.table = Some(Arc::clone(&table));
    let pty = Arc::new(pty);
    let runtime = Arc::new(MemRuntime::default());
    let mut adapters = AdapterRegistry::default();
    adapters.register(Arc::new(adapter));
    let (approvals, notices) = mpsc::unbounded_channel();
    let runs = AgentRuns::new(AgentRunsDeps {
        approvals,
        adapters,
        hub: Arc::clone(&hub),
        pty: pty.clone(),
        processes: table.clone(),
        runtime: runtime.clone(),
        login: Arc::new(MemLogin),
        tokens: Arc::new(Counter(Mutex::new(0))),
        clock: Arc::new(FixedClock),
        signal_helper: "/bin/sy-signal".into(),
        socket: "/tmp/sy/engine.sock".into(),
        timings: Timings {
            hangup_wait: Duration::from_millis(50),
            kill_wait: Duration::from_millis(500),
            drain: Duration::from_millis(500),
            shutdown_grace: Duration::from_millis(100),
            tick: Duration::from_millis(20),
            scan_every: Duration::from_millis(30),
            sweep_term_wait: Duration::from_millis(300),
            sweep_kill_wait: Duration::from_millis(100),
            sweep_poll: Duration::from_millis(10),
        },
    });
    tokio::spawn(AgentRuns::scan_loop(Arc::downgrade(&runs)));
    Harness {
        runs,
        table,
        notices: tokio::sync::Mutex::new(notices),
        store: Arc::new(MemWrites::default()),
        pty,
        runtime,
        log,
        hub,
    }
}

fn harness() -> Harness {
    harness_with(Scripted::default(), FakePty::default())
}

fn workspace(options: &str) -> Workspace {
    Workspace {
        id: WorkspaceId::new(),
        task_id: TaskId::new(),
        project_id: ProjectId::new(),
        agent_id: "scripted".into(),
        agent_options: options.into(),
        branch: "dev/x".into(),
        worktree_path: "/work/tree".into(),
        base_commit: String::new(),
        state: WorkspaceState::Ready,
        outcome: None,
        merged_commit: None,
        created_at: 1,
        closed_at: None,
    }
}

impl Harness {
    async fn start(&self) -> RunId {
        self.start_with("{}").await
    }

    async fn start_with(&self, options: &str) -> RunId {
        self.start_full(options, LimitOverride::default()).await
    }

    async fn start_limited(&self, limits: LimitOverride) -> RunId {
        self.start_full("{}", limits).await
    }

    async fn start_full(&self, options: &str, limits: LimitOverride) -> RunId {
        self.runs
            .start(
                self.store.clone(),
                StartRun {
                    workspace: workspace(options),
                    prompt: "do it".into(),
                    size: PtySize {
                        cols: 120,
                        rows: 40,
                    },
                    limits,
                },
            )
            .await
            .unwrap()
    }

    fn child<T>(&self, f: impl FnOnce(&mut ChildHandle) -> T) -> T {
        f(lock(&self.pty.children).last_mut().unwrap())
    }

    async fn until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        for _ in 0..1000 {
            if done(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn until_status(&self, run: &RunId, status: RunStatus) {
        self.until(status.as_str(), |h| h.store.run(run).status == status)
            .await;
    }

    async fn until_ended(&self) {
        self.until("the run's end", |h| h.runs.live_count() == 0)
            .await;
    }

    /// Call a hook with the token minted for the `n`th run (from 1).
    async fn hook(&self, n: u32, hook: &str, stdin: &str) -> Result<HookReply, HookError> {
        self.runs
            .hook(
                &format!("{n:032x}"),
                HookCall {
                    hook: hook.into(),
                    args: Vec::new(),
                    stdin: stdin.as_bytes().to_vec(),
                },
            )
            .await
    }

    /// The next approval announcement, as (requested or decided, summary, status).
    async fn notice(&self) -> (&'static str, String, ApprovalStatus) {
        let notice = tokio::time::timeout(Duration::from_secs(5), self.notices.lock().await.recv())
            .await
            .expect("no approval notice")
            .expect("notice channel closed");
        match notice {
            ApprovalNotice::Requested { approval, .. } => {
                ("requested", approval.summary, approval.status)
            }
            ApprovalNotice::Decided { approval, .. } => {
                ("decided", approval.summary, approval.status)
            }
        }
    }

    /// Call `hold` with `key` on the first run, in the background.
    fn hold(&self, key: &str) -> tokio::task::JoinHandle<Result<HookReply, HookError>> {
        let runs = Arc::clone(&self.runs);
        let key = key.to_owned();
        tokio::spawn(async move {
            runs.hook(
                &format!("{:032x}", 1),
                HookCall {
                    hook: "hold".into(),
                    args: Vec::new(),
                    stdin: key.into_bytes(),
                },
            )
            .await
        })
    }

    /// `approval.respond` for the pending approval summarised `key`.
    async fn respond(
        &self,
        key: &str,
        approve: bool,
        message: Option<&str>,
    ) -> Result<Approval, RespondError> {
        let id = lock(&self.store.approvals)
            .values()
            .find(|a| a.summary == key)
            .map(|a| a.id.clone())
            .expect("no such approval");
        self.runs
            .respond(
                self.store.as_ref(),
                &id,
                approve,
                message.map(str::to_owned),
            )
            .await
    }

    /// Every `run_limit_exceeded` journaled, as (limit, threshold, observed).
    fn limit_events(&self) -> Vec<(LimitKind, u64, u64)> {
        lock(&self.store.events)
            .iter()
            .filter_map(|(actor, _, e)| match e {
                AdapterEvent::RunLimitExceeded {
                    limit,
                    threshold,
                    observed,
                    ..
                } => {
                    assert_eq!(*actor, Actor::Engine);
                    Some((*limit, *threshold, *observed))
                }
                _ => None,
            })
            .collect()
    }

    fn logged(&self, run: &RunId) -> String {
        String::from_utf8_lossy(&lock(&self.log.0).get(run).cloned().unwrap_or_default())
            .into_owned()
    }
}

/// The group signals before the sweep, after checking the sweep's own
/// SIGCONT, SIGTERM, SIGKILL closed them (spec 0004, AC-8).
fn before_sweep(signals: &[GroupSignal]) -> Vec<GroupSignal> {
    let sweep = [GroupSignal::Cont, GroupSignal::Term, GroupSignal::Kill];
    assert!(
        signals.ends_with(&sweep),
        "no sweep at the end of {signals:?}"
    );
    signals[..signals.len() - sweep.len()].to_vec()
}

// ---- tests ----

#[tokio::test]
async fn a_run_should_go_starting_running_finished_and_clean_up_after_itself() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| c.print("hi"));
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;

    let row = h.store.run(&run);
    assert_eq!(
        h.store.statuses(),
        [RunStatus::Starting, RunStatus::Running, RunStatus::Finished]
    );
    assert_eq!(
        (
            row.exit_code,
            row.end_reason,
            row.log_bytes,
            row.pid,
            row.pgid,
            row.pid_started_at
        ),
        (
            Some(0),
            Some(EndReason::Exited),
            2,
            Some(4242),
            Some(4242),
            Some(MAIN_START)
        )
    );
    assert_eq!(h.logged(&run), "hi");
    assert!(lock(&h.runtime.live).is_empty() && lock(&h.runtime.removed).contains(&run));
    assert!(!h.runs.token_is_live(&format!("{:032x}", 1)));
    assert!(h.hub.is_empty());
}

#[tokio::test]
async fn the_spawn_should_get_the_resolved_program_worktree_and_agent_env() {
    let h = harness();
    h.start().await;
    let spawned = lock(&h.pty.spawned)[0].clone();
    let env: HashMap<_, _> = spawned.env.into_iter().collect();
    assert_eq!(
        (
            spawned.program.as_str(),
            spawned.args,
            spawned.cwd.as_str(),
            spawned.size,
            env.get("PATH").map(String::as_str),
            env.get("TERM").map(String::as_str),
            env.get("SWITCHYARD_SIGNAL_TOKEN").map(String::len),
        ),
        (
            "/usr/bin/agent",
            vec!["--go".to_owned()],
            "/work/tree",
            PtySize {
                cols: 120,
                rows: 40
            },
            Some("/usr/bin"),
            Some("xterm-256color"),
            Some(32),
        )
    );
    assert_eq!(lock(&h.runtime.files)[0].0, "script.json");
    h.child(|c| c.exit(0));
    h.until_ended().await;
}

#[tokio::test]
async fn initial_input_should_be_typed_once_the_run_is_running() {
    let h = harness();
    let run = h.start().await;
    let before = h.child(|c| lock(&c.input).clone());
    h.child(|c| c.print("ready"));
    h.until_status(&run, RunStatus::Running).await;
    h.until("initial input", |h| {
        h.child(|c| lock(&c.input).clone()) == b"hello\r"
    })
    .await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert!(before.is_empty());
}

#[tokio::test]
async fn decoder_reports_should_move_the_run_between_running_and_waiting_input() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| c.print("WAIT"));
    h.until_status(&run, RunStatus::WaitingInput).await;
    h.child(|c| c.print("GO"));
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(3));
    h.until_ended().await;
    let row = h.store.run(&run);
    assert_eq!(
        (h.store.statuses(), row.exit_code),
        (
            // The first byte means running (AC-6), before the decoder's report.
            vec![
                RunStatus::Starting,
                RunStatus::Running,
                RunStatus::WaitingInput,
                RunStatus::Running,
                RunStatus::Failed
            ],
            Some(3)
        )
    );
}

#[tokio::test]
async fn output_written_just_before_the_exit_should_still_count_as_running() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| {
        c.print("last words");
        c.exit(0);
    });
    h.until_ended().await;
    assert_eq!(
        (h.store.statuses(), h.logged(&run)),
        (
            vec![RunStatus::Starting, RunStatus::Running, RunStatus::Finished],
            "last words".to_owned()
        )
    );
}

async fn failed_launch(adapter: Scripted, options: &str) -> (Harness, RunId) {
    let h = harness_with(adapter, FakePty::default());
    let run = h.start_with(options).await;
    h.until_ended().await;
    (h, run)
}

fn assert_spawn_failed(h: &Harness, run: &RunId, reason: &str) {
    let row = h.store.run(run);
    let log = h.logged(run);
    assert_eq!(
        (
            h.store.statuses(),
            row.end_reason,
            row.exit_code,
            row.log_bytes,
            lock(&h.pty.spawned).len()
        ),
        (
            vec![RunStatus::Starting, RunStatus::Failed],
            Some(EndReason::SpawnFailed),
            None,
            log.len() as u64,
            0
        )
    );
    assert!(
        log.starts_with("switchyard: ") && log.contains(reason) && log.ends_with("\r\n"),
        "{log:?}"
    );
    assert!(lock(&h.runtime.live).is_empty() && h.runs.live_count() == 0);
}

#[tokio::test]
async fn an_adapter_error_should_fail_the_run_with_spawn_failed() {
    let adapter = Scripted {
        error: Some(AdapterError::InvalidOptions("bad script".into())),
        ..Scripted::default()
    };
    let (h, run) = failed_launch(adapter, "{}").await;
    assert_spawn_failed(&h, &run, "bad script");
}

#[tokio::test]
async fn a_program_the_adapter_did_not_list_should_fail_the_run() {
    let adapter = Scripted {
        program: "/bin/sh".into(),
        ..Scripted::default()
    };
    let (h, run) = failed_launch(adapter, "{}").await;
    assert_spawn_failed(&h, &run, "not one of this adapter's programs");
}

#[tokio::test]
async fn a_launch_file_outside_the_launch_dir_should_fail_the_run() {
    let adapter = Scripted {
        files: vec![LaunchFile {
            rel_path: "../escape".into(),
            contents: Vec::new(),
        }],
        ..Scripted::default()
    };
    let (h, run) = failed_launch(adapter, "{}").await;
    assert_spawn_failed(&h, &run, "outside the launch dir");
    assert!(lock(&h.runtime.files).is_empty());
}

#[tokio::test]
async fn options_that_are_not_json_should_fail_the_run() {
    let (h, run) = failed_launch(Scripted::default(), "{nope").await;
    assert_spawn_failed(&h, &run, "not valid JSON");
}

#[tokio::test]
async fn stop_should_type_the_stop_input_then_hang_up_and_end_stopped() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| c.print("working"));
    h.until_status(&run, RunStatus::Running).await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    h.until_ended().await;
    let row = h.store.run(&run);
    let (input, signals) = h.child(|c| (lock(&c.input).clone(), before_sweep(&lock(&c.signals))));
    assert_eq!(
        (row.status, row.end_reason, row.exit_code, signals),
        (
            RunStatus::Stopped,
            Some(EndReason::UserStop),
            None,
            vec![GroupSignal::Hangup]
        )
    );
    assert!(input.ends_with(b"\x03"), "{input:?}");
    assert_eq!(lock(&h.store.moves).last().unwrap().1, Actor::User);
}

#[tokio::test]
async fn an_agent_that_ignores_sighup_should_get_sigkill_after_the_wait() {
    let pty = FakePty {
        ignore_hangup: true,
        ..FakePty::default()
    };
    let h = harness_with(Scripted::default(), pty);
    let run = h.start().await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    h.until_ended().await;
    let signals = h.child(|c| before_sweep(&lock(&c.signals)));
    assert_eq!(
        (h.store.run(&run).status, signals),
        (
            RunStatus::Stopped,
            vec![GroupSignal::Hangup, GroupSignal::Kill]
        )
    );
}

#[tokio::test]
async fn an_exit_during_the_grace_should_still_end_stopped_with_its_code() {
    let adapter = Scripted {
        grace_ms: 5_000,
        ..Scripted::default()
    };
    let h = harness_with(adapter, FakePty::default());
    let run = h.start().await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    h.child(|c| c.exit(130));
    h.until_ended().await;
    let row = h.store.run(&run);
    let signals = h.child(|c| before_sweep(&lock(&c.signals)));
    assert_eq!(
        (row.status, row.end_reason, row.exit_code, signals),
        (
            RunStatus::Stopped,
            Some(EndReason::UserStop),
            Some(130),
            vec![]
        )
    );
}

#[tokio::test]
async fn stop_should_be_a_no_op_twice_and_refuse_ended_or_unknown_runs() {
    let adapter = Scripted {
        grace_ms: 5_000,
        ..Scripted::default()
    };
    let h = harness_with(adapter, FakePty::default());
    let run = h.start().await;
    let first = h.runs.stop(h.store.as_ref(), &run).await;
    let second = h.runs.stop(h.store.as_ref(), &run).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    let ended = h.runs.stop(h.store.as_ref(), &run).await;
    let unknown = h.runs.stop(h.store.as_ref(), &RunId::new()).await;
    let input = h.child(|c| lock(&c.input).clone());
    assert_eq!(
        (first, second, ended, unknown),
        (
            Ok(()),
            Ok(()),
            Err(StopError::NotLive),
            Err(StopError::NotFound)
        )
    );
    assert_eq!(input.iter().filter(|b| **b == 3).count(), 1);
}

#[tokio::test]
async fn a_queued_run_with_no_driver_should_go_straight_to_stopped() {
    let h = harness();
    let id = RunId::new();
    h.store
        .create_run(
            Actor::User,
            NewRun {
                id: id.clone(),
                workspace_id: WorkspaceId::new(),
                kind: RunKind::Agent,
                input: None,
                resumes_run_id: None,
            },
        )
        .await
        .unwrap();
    h.runs.stop(h.store.as_ref(), &id).await.unwrap();
    assert_eq!(
        (h.store.run(&id).status, h.store.run(&id).end_reason),
        (RunStatus::Stopped, Some(EndReason::UserStop))
    );
}

#[tokio::test]
async fn stop_all_should_cap_the_grace_and_wait_for_every_run_to_end() {
    let adapter = Scripted {
        grace_ms: 10_000,
        ..Scripted::default()
    };
    let h = harness_with(adapter, FakePty::default());
    let a = h.start().await;
    let b = h.start().await;
    let started = std::time::Instant::now();
    h.runs.stop_all().await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        (
            h.store.run(&a).status,
            h.store.run(&b).status,
            h.runs.live_count()
        ),
        (RunStatus::Stopped, RunStatus::Stopped, 0)
    );
}

#[tokio::test]
async fn an_unknown_agent_should_create_no_run() {
    let h = harness();
    let mut ws = workspace("{}");
    ws.agent_id = "nobody".into();
    let err = h
        .runs
        .start(
            h.store.clone(),
            StartRun {
                workspace: ws,
                prompt: String::new(),
                size: PtySize { cols: 1, rows: 1 },
                limits: supervise::LimitOverride::default(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err, lock(&h.store.runs).len()),
        (StartError::UnknownAgent("nobody".into()), 0)
    );
}

// ---- hook calls (milestone 2) ----

fn ok_reply() -> Result<HookReply, HookError> {
    Ok(HookReply {
        stdout: HOOK_OK.to_vec(),
        exit_code: 0,
    })
}

#[tokio::test]
async fn hook_calls_should_set_status_journal_events_and_save_the_last_session() {
    let h = harness();
    let run = h.start().await;
    let answers = [
        h.hook(1, "status", "WAIT").await,
        h.hook(1, "event", "thinking").await,
        h.hook(1, "session", "s-1").await,
        h.hook(1, "session", "s-2").await,
    ];
    h.until_status(&run, RunStatus::WaitingInput).await;
    let row = h.store.run(&run);
    let events = lock(&h.store.events).clone();
    assert_eq!(
        (answers.to_vec(), row.agent_session_ref, events),
        (
            vec![ok_reply(), ok_reply(), ok_reply(), ok_reply()],
            Some("s-2".to_owned()),
            vec![(
                Actor::Agent,
                Some(row.workspace_id.to_string()),
                AdapterEvent::Message {
                    role: MessageRole::Agent,
                    text: "thinking".into()
                }
            )]
        )
    );
    h.child(|c| c.exit(0));
    h.until_ended().await;
    // Events and session refs never change the status (AC-8).
    assert_eq!(
        h.store.statuses(),
        [
            RunStatus::Starting,
            RunStatus::WaitingInput,
            RunStatus::Finished
        ]
    );
}

#[tokio::test]
async fn an_unknown_hook_should_get_the_decoders_fallback_with_no_reports() {
    let h = harness();
    h.start().await;
    let answer = h.hook(1, "nope", "WAIT").await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(
        (answer, h.store.statuses()),
        (
            Ok(HookReply {
                stdout: Vec::new(),
                exit_code: 2
            }),
            vec![RunStatus::Starting, RunStatus::Finished]
        )
    );
}

#[tokio::test]
async fn a_token_should_be_unknown_before_its_run_and_after_its_end() {
    let h = harness();
    let before = h.hook(1, "status", "GO").await;
    h.start().await;
    let live = h.hook(1, "status", "GO").await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    let after = h.hook(1, "status", "GO").await;
    assert_eq!(
        (before, live, after),
        (
            Err(HookError::TokenUnknown),
            ok_reply(),
            Err(HookError::TokenUnknown)
        )
    );
}

#[tokio::test]
async fn hook_calls_past_the_burst_should_be_rate_limited_and_drop_their_reports() {
    // The test clock never moves, so nothing refills after the burst.
    let h = harness();
    h.start().await;
    let mut answers = HashMap::new();
    for _ in 0..SIGNAL_BURST + 20 {
        let answer = h.hook(1, "event", "e").await;
        *answers.entry(format!("{answer:?}")).or_insert(0_u64) += 1;
    }
    // A held hook is never charged, even with the bucket empty.
    let held = h.hold("k1");
    let requested = h.notice().await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    let held = held.await.unwrap();
    assert_eq!(
        (
            answers.get(&format!("{:?}", ok_reply())).copied(),
            answers
                .get(&format!(
                    "{:?}",
                    Err::<HookReply, _>(HookError::RateLimited)
                ))
                .copied(),
            lock(&h.store.events).len() as u64,
            requested,
            held,
        ),
        (
            Some(SIGNAL_BURST),
            Some(20),
            SIGNAL_BURST,
            ("requested", "k1".to_owned(), ApprovalStatus::Pending),
            Ok(HookReply {
                stdout: b"deny:".to_vec(),
                exit_code: 0
            })
        )
    );
}

// ---- approvals (milestone 3) ----

fn held_reply(stdout: &str) -> Result<HookReply, HookError> {
    Ok(HookReply {
        stdout: stdout.as_bytes().to_vec(),
        exit_code: 0,
    })
}

#[tokio::test]
async fn a_held_hook_should_wait_for_your_answer_and_get_the_decoders_reply() {
    let h = harness();
    let run = h.start().await;
    let held = h.hold("k1");
    let requested = h.notice().await;
    h.until_status(&run, RunStatus::WaitingInput).await;
    let decided = h.respond("k1", false, Some("not that")).await.unwrap();
    let reply = held.await.unwrap();
    let notice = h.notice().await;
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(
        (
            requested,
            (
                decided.status,
                decided.decided_by_ref.as_deref(),
                decided.message.as_deref()
            ),
            reply,
            notice,
            h.store.statuses(),
        ),
        (
            ("requested", "k1".to_owned(), ApprovalStatus::Pending),
            (ApprovalStatus::Denied, Some("inbox"), Some("not that")),
            held_reply("deny:not that"),
            ("decided", "k1".to_owned(), ApprovalStatus::Denied),
            vec![
                RunStatus::Starting,
                RunStatus::WaitingInput,
                RunStatus::Running,
                RunStatus::Finished
            ],
        )
    );
}

#[tokio::test]
async fn two_pending_approvals_should_keep_the_run_waiting_until_both_are_answered() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| c.print("hi"));
    h.until_status(&run, RunStatus::Running).await;
    let (one, two) = (h.hold("k1"), h.hold("k2"));
    h.notice().await;
    h.notice().await;
    h.respond("k1", true, None).await.unwrap();
    let still = h.store.run(&run).status;
    // A `running` report while one is pending changes nothing.
    h.child(|c| c.print("GO"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after_go = h.store.run(&run).status;
    h.respond("k2", true, None).await.unwrap();
    h.until_status(&run, RunStatus::Running).await;
    assert_eq!(
        (still, after_go, one.await.unwrap(), two.await.unwrap()),
        (
            RunStatus::WaitingInput,
            RunStatus::WaitingInput,
            held_reply("approve"),
            held_reply("approve")
        )
    );
    h.child(|c| c.exit(0));
    h.until_ended().await;
}

#[tokio::test]
async fn a_typed_request_should_get_its_answer_typed_into_the_terminal() {
    let h = harness();
    let run = h.start().await;
    h.child(|c| c.print("ASK:t1"));
    h.notice().await;
    h.until_status(&run, RunStatus::WaitingInput).await;
    h.respond("t1", false, None).await.unwrap();
    h.until("the typed answer", |h| {
        h.child(|c| lock(&c.input).ends_with(b"n\r"))
    })
    .await;
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
}

#[tokio::test]
async fn an_answer_seen_in_the_terminal_should_settle_it_and_reply_to_the_held_hook() {
    let h = harness();
    let run = h.start().await;
    let (yes, huh) = (h.hold("k1"), h.hold("k2"));
    h.notice().await;
    h.notice().await;
    h.child(|c| c.print("YES:k1 HUH:k2 NO:nobody"));
    let replies = (yes.await.unwrap(), huh.await.unwrap());
    h.until_status(&run, RunStatus::Running).await;
    let late = h.respond("k1", false, None).await;
    let (k1, k2) = {
        let all = lock(&h.store.approvals);
        let find = |key: &str| all.values().find(|a| a.summary == key).unwrap().clone();
        (find("k1"), find("k2"))
    };
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(
        (
            replies,
            (k1.status, k1.decided_by_ref.as_deref()),
            (k2.status, k2.decided_by, k2.decided_by_ref.as_deref()),
            late,
        ),
        (
            (held_reply("approve"), held_reply("deny:")),
            (ApprovalStatus::Approved, Some("terminal")),
            (ApprovalStatus::Cancelled, None, None),
            Err(RespondError::NotPending),
        )
    );
}

#[tokio::test]
async fn a_held_hook_whose_caller_goes_away_should_expire() {
    let h = harness();
    let run = h.start().await;
    let held = h.hold("k1");
    h.notice().await;
    held.abort();
    let expired = h.notice().await;
    h.until_status(&run, RunStatus::Running).await;
    let late = h.respond("k1", true, None).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(
        (expired, late),
        (
            ("decided", "k1".to_owned(), ApprovalStatus::Expired),
            Err(RespondError::NotPending)
        )
    );
}

#[tokio::test]
async fn a_duplicate_key_or_a_failed_create_should_make_the_helper_fall_back() {
    let h = harness();
    let run = h.start().await;
    let first = h.hold("k1");
    h.notice().await;
    let duplicate = h.hook(1, "hold", "k1").await;
    h.store.refuse_creates.store(true, Ordering::SeqCst);
    let refused = h.hook(1, "hold", "k2").await;
    h.child(|c| c.print("ASK:t1"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let count = lock(&h.store.approvals).len();
    h.respond("k1", true, None).await.unwrap();
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert!(
        matches!(duplicate, Err(HookError::Fallback(_))),
        "{duplicate:?}"
    );
    assert!(
        matches!(refused, Err(HookError::Fallback(_))),
        "{refused:?}"
    );
    assert_eq!((count, first.await.unwrap()), (1, held_reply("approve")));
}

#[tokio::test]
async fn a_stop_should_cancel_held_approvals_with_a_deny_then_type_its_input() {
    let h = harness_with(
        Scripted {
            grace_ms: 10_000,
            ..Scripted::default()
        },
        FakePty::default(),
    );
    let run = h.start().await;
    h.child(|c| c.print("hi ASK:t1"));
    h.notice().await;
    let held = h.hold("k1");
    h.notice().await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    let reply = held.await.unwrap();
    let stopped = h.notice().await;
    // The stop input goes through the writer thread after `stop` returns.
    h.until("the stop input", |h| {
        h.child(|c| lock(&c.input).ends_with(b"\x03"))
    })
    .await;
    let input = h.child(|c| lock(&c.input).clone());
    // The typed request waits for the run's end, which cancels it.
    let typed = lock(&h.store.approvals)
        .values()
        .find(|a| a.summary == "t1")
        .unwrap()
        .clone();
    h.child(|c| c.exit(130));
    h.until_ended().await;
    let ended = h.notice().await;
    let k1 = lock(&h.store.approvals)
        .values()
        .find(|a| a.summary == "k1")
        .unwrap()
        .clone();
    assert_eq!(
        (
            reply,
            stopped,
            (k1.status, k1.message.as_deref()),
            input.ends_with(b"\x03"),
            typed.status,
            ended,
            h.store.run(&run).status,
        ),
        (
            held_reply("deny:run stopped"),
            ("decided", "k1".to_owned(), ApprovalStatus::Cancelled),
            (ApprovalStatus::Cancelled, Some("run stopped")),
            true,
            ApprovalStatus::Pending,
            ("decided", "t1".to_owned(), ApprovalStatus::Cancelled),
            RunStatus::Stopped,
        )
    );
}

#[tokio::test]
async fn an_exit_with_pending_approvals_should_deny_held_hooks_and_cancel_before_the_end() {
    let h = harness();
    let run = h.start().await;
    let held = h.hold("k1");
    h.notice().await;
    h.child(|c| c.exit(1));
    h.until_ended().await;
    let reply = held.await.unwrap();
    let after = h.hook(1, "status", "GO").await;
    let late = h
        .runs
        .respond(h.store.as_ref(), &h.store.only_approval().id, true, None)
        .await;
    assert_eq!(
        (reply, h.notice().await, after, late, h.store.trail()),
        (
            held_reply("deny:"),
            ("decided", "k1".to_owned(), ApprovalStatus::Cancelled),
            Err(HookError::TokenUnknown),
            Err(RespondError::NotPending),
            vec![
                "run starting".to_owned(),
                "requested k1".to_owned(),
                "run waiting_input".to_owned(),
                "k1 cancelled".to_owned(),
                "run failed".to_owned(),
            ],
        )
    );
    assert_eq!(h.store.run(&run).status, RunStatus::Failed);
}

#[tokio::test]
async fn your_answer_racing_a_stop_should_land_once_and_match_the_held_reply() {
    for _ in 0..20 {
        let h = harness();
        let run = h.start().await;
        let held = h.hold("k1");
        h.notice().await;
        let id = h.store.only_approval().id;
        let (answer, stop) = tokio::join!(
            h.runs.respond(h.store.as_ref(), &id, true, None),
            h.runs.stop(h.store.as_ref(), &run)
        );
        stop.unwrap();
        let reply = held.await.unwrap();
        let row = h.store.approval(&id);
        match answer {
            Ok(approval) => assert_eq!(
                (approval.status, row.status, reply),
                (
                    ApprovalStatus::Approved,
                    ApprovalStatus::Approved,
                    held_reply("approve")
                )
            ),
            Err(err) => assert_eq!(
                (err, row.status, reply),
                (
                    RespondError::NotPending,
                    ApprovalStatus::Cancelled,
                    held_reply("deny:run stopped")
                )
            ),
        }
        h.child(|c| c.exit(130));
        h.until_ended().await;
    }
}

#[tokio::test]
async fn unknown_approvals_should_be_not_found() {
    let h = harness();
    let answer = h
        .runs
        .respond(h.store.as_ref(), &ApprovalId::new(), true, None)
        .await;
    assert_eq!(answer.unwrap_err(), RespondError::NotFound);
}

// ---- detection (milestone 4) ----

use super::detect::{Detector, DetectorDeps, PROBE_TIMEOUT};

/// Records each probe; every run exits 0 printing `1.2.3 (agent)`, after a
/// short wait so two callers overlap.
#[derive(Default)]
struct Probes(Mutex<Vec<ProbeRequest>>);

impl ProcessRunner for Probes {
    fn run(&self, req: ProbeRequest) -> ProbeRun {
        lock(&self.0).push(req);
        std::thread::sleep(Duration::from_millis(50));
        ProbeRun {
            outcome: sy_adapter_api::ProbeOutcome::Exited(ExitInfo {
                code: Some(0),
                signal: None,
            }),
            stdout: b"1.2.3 (agent)".to_vec(),
            stderr: Vec::new(),
        }
    }
}

/// Probes a listed program, a listed one that isn't installed, and one it didn't list.
struct Prober;

impl AgentAdapter for Prober {
    fn id(&self) -> &str {
        "prober"
    }
    fn display_name(&self) -> &str {
        "Prober"
    }
    fn capabilities(&self) -> Capabilities {
        Scripted::default().capabilities()
    }
    fn programs(&self) -> Vec<String> {
        vec!["agent".into(), "missing-tool".into()]
    }
    fn detection_probes(&self) -> Vec<Probe> {
        let probe = |program: &str, timeout_ms| Probe {
            program: program.into(),
            args: vec!["--version".into()],
            timeout_ms,
        };
        vec![
            probe("agent", 9_000),
            probe("missing-tool", 100),
            probe("/usr/bin/sneaky", 100),
        ]
    }
    fn interpret_detection(&self, results: &[ProbeResult]) -> Detection {
        Detection {
            installed: true,
            signed_in: SignedIn::Unknown,
            version: Some(String::from_utf8_lossy(&results[0].stdout).into_owned()),
            hint: Some(format!(
                "{:?}",
                results.iter().map(|r| &r.outcome).collect::<Vec<_>>()
            )),
        }
    }
    fn launch(&self, _: &LaunchRequest) -> Result<Launch, AdapterError> {
        Err(AdapterError::Unsupported("detection only".into()))
    }
}

fn detector(runner: Arc<Probes>) -> Arc<Detector> {
    let mut adapters = AdapterRegistry::default();
    adapters.register(Arc::new(Prober));
    adapters.register(Arc::new(Scripted::default()));
    Detector::new(DetectorDeps {
        adapters,
        runner,
        login: Arc::new(MemLogin),
        runtime: Arc::new(MemRuntime::default()),
        clock: Arc::new(FixedClock),
    })
}

#[tokio::test]
async fn a_pass_should_probe_only_listed_programs_found_on_the_login_path() {
    let runner = Arc::new(Probes::default());
    let d = detector(Arc::clone(&runner));
    let before = d.list();
    d.detect().await;
    let after = d.list();
    let spawned = lock(&runner.0).clone();
    let env: HashMap<_, _> = spawned[0].env.iter().cloned().collect();
    assert_eq!(
        (
            before
                .iter()
                .map(|a| (a.id.as_str(), a.detection.is_none()))
                .collect::<Vec<_>>(),
            spawned.len(),
            (
                spawned[0].program.as_str(),
                spawned[0].cwd.as_str(),
                spawned[0].timeout
            ),
            (
                env.get("NO_COLOR").map(String::as_str),
                env.get("TERM").map(String::as_str),
                env.get("PATH").map(String::as_str)
            ),
        ),
        (
            vec![("prober", true), ("scripted", true)],
            1,
            ("/usr/bin/agent", "/data/agent-runtime", PROBE_TIMEOUT),
            (Some("1"), Some("dumb"), Some("/usr/bin")),
        )
    );
    let prober = after[0].detection.clone().unwrap();
    assert_eq!(
        (
            prober.path.as_deref(),
            prober.checked_at,
            prober.detection.version.as_deref(),
            prober.detection.hint.as_deref(),
            after[1].detection.as_ref().map(|d| d.path.clone()),
        ),
        (
            Some("/usr/bin/agent"),
            1_000,
            Some("1.2.3 (agent)"),
            Some(r#"[Exited(ExitInfo { code: Some(0), signal: None }), NotFound, Refused]"#),
            // No probes: detected, with no path.
            Some(None),
        )
    );
}

#[tokio::test]
async fn concurrent_detect_calls_should_share_one_pass() {
    let runner = Arc::new(Probes::default());
    let d = detector(Arc::clone(&runner));
    d.spawn();
    tokio::join!(d.detect(), d.detect(), d.detect());
    assert_eq!(lock(&runner.0).len(), 1);
    // Once that pass ended, a new call starts a new one.
    d.detect().await;
    assert_eq!(lock(&runner.0).len(), 2);
}

/// A login env with its own `TERM` and `NO_COLOR`, which probes must not see.
struct ColorLogin;

#[async_trait::async_trait]
impl LoginEnv for ColorLogin {
    async fn snapshot(&self) -> Arc<LoginEnvSnapshot> {
        Arc::new(LoginEnvSnapshot {
            vars: vec![
                ("PATH".into(), "/usr/bin".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("NO_COLOR".into(), "0".into()),
                ("HOME".into(), "/Users/me".into()),
            ],
            fallback: None,
        })
    }
    async fn refresh(&self) -> Arc<LoginEnvSnapshot> {
        self.snapshot().await
    }
    fn current(&self) -> Option<Arc<LoginEnvSnapshot>> {
        None
    }
    fn resolve(&self, env: &LoginEnvSnapshot, program: &str) -> Option<String> {
        MemLogin.resolve(env, program)
    }
}

/// Detection only, with exactly the probes it is given.
struct Probing(Vec<Probe>);

impl AgentAdapter for Probing {
    fn id(&self) -> &str {
        "probing"
    }
    fn display_name(&self) -> &str {
        "Probing"
    }
    fn capabilities(&self) -> Capabilities {
        Scripted::default().capabilities()
    }
    fn programs(&self) -> Vec<String> {
        vec!["agent".into(), "missing-tool".into()]
    }
    fn detection_probes(&self) -> Vec<Probe> {
        self.0.clone()
    }
    fn interpret_detection(&self, _: &[ProbeResult]) -> Detection {
        Detection {
            installed: true,
            signed_in: SignedIn::Unknown,
            version: None,
            hint: None,
        }
    }
    fn launch(&self, _: &LaunchRequest) -> Result<Launch, AdapterError> {
        Err(AdapterError::Unsupported("detection only".into()))
    }
}

fn probe(program: &str, timeout_ms: u32) -> Probe {
    Probe {
        program: program.into(),
        args: vec!["--version".into()],
        timeout_ms,
    }
}

fn probing_detector(
    probes: Vec<Probe>,
    login: Arc<dyn LoginEnv>,
    runner: Arc<Probes>,
) -> Arc<Detector> {
    let mut adapters = AdapterRegistry::default();
    adapters.register(Arc::new(Probing(probes)));
    Detector::new(DetectorDeps {
        adapters,
        runner,
        login,
        runtime: Arc::new(MemRuntime::default()),
        clock: Arc::new(FixedClock),
    })
}

// covers: AC-3 (probes are capped at 5 s; an adapter may ask for less)
#[tokio::test]
async fn a_probe_asking_for_less_than_5_s_should_keep_its_timeout_and_zero_should_mean_5_s() {
    let runner = Arc::new(Probes::default());
    let d = probing_detector(
        vec![
            probe("agent", 1_500),
            probe("agent", 0),
            probe("agent", 5_001),
        ],
        Arc::new(MemLogin),
        Arc::clone(&runner),
    );
    d.detect().await;
    let timeouts: Vec<_> = lock(&runner.0).iter().map(|r| r.timeout).collect();
    assert_eq!(
        timeouts,
        [Duration::from_millis(1_500), PROBE_TIMEOUT, PROBE_TIMEOUT]
    );
}

// covers: AC-3 (probes run with NO_COLOR=1 and TERM=dumb, whatever the login shell set)
#[tokio::test]
async fn the_login_envs_own_term_and_no_color_should_be_replaced_not_duplicated() {
    let runner = Arc::new(Probes::default());
    let d = probing_detector(
        vec![probe("agent", 100)],
        Arc::new(ColorLogin),
        Arc::clone(&runner),
    );
    d.detect().await;
    let mut env = lock(&runner.0)[0].env.clone();
    env.sort();
    assert_eq!(
        env,
        [
            ("HOME".to_owned(), "/Users/me".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("TERM".to_owned(), "dumb".to_owned()),
        ]
    );
}

// covers: AC-3 (detection `path` is the first probe's resolution, never a later one's)
#[tokio::test]
async fn path_should_be_empty_when_the_first_probe_does_not_resolve() {
    let runner = Arc::new(Probes::default());
    let d = probing_detector(
        vec![probe("missing-tool", 100), probe("agent", 100)],
        Arc::new(MemLogin),
        Arc::clone(&runner),
    );
    d.detect().await;
    let info = d.list()[0].detection.clone().unwrap();
    assert_eq!(
        (info.path, lock(&runner.0).len()),
        (None, 1),
        "the second probe still ran, but its path is not the agent's"
    );
}

// ---- run limits (spec 0004) ----

fn only(limits: LimitOverride) -> LimitOverride {
    // Everything not named is off, so only the limit under test can fire.
    LimitOverride {
        idle_ms: limits.idle_ms.or(Some(0)),
        active_ms: limits.active_ms.or(Some(0)),
        output_bytes: limits.output_bytes.or(Some(0)),
    }
}

fn assert_limit_end(h: &Harness, run: &RunId, kind: LimitKind) {
    let row = h.store.run(run);
    assert_eq!(
        (row.status, row.end_reason),
        (RunStatus::Failed, Some(EndReason::LimitExceeded))
    );
    let events = h.limit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, kind);
    assert!(events[0].2 >= events[0].1, "{events:?}");
    assert_eq!(lock(&h.store.moves).last().unwrap().1, Actor::Engine);
}

#[tokio::test]
async fn output_over_its_limit_should_stop_the_run_and_end_it_limit_exceeded() {
    let h = harness();
    let run = h
        .start_limited(only(LimitOverride {
            output_bytes: Some(64),
            ..LimitOverride::default()
        }))
        .await;
    h.child(|c| c.print(&"x".repeat(100)));
    h.until_ended().await;
    assert_limit_end(&h, &run, LimitKind::Output);
    assert_eq!(h.limit_events()[0].1, 64);
    let (input, signals) = h.child(|c| (lock(&c.input).clone(), before_sweep(&lock(&c.signals))));
    assert!(input.ends_with(b"\x03"), "{input:?}");
    assert_eq!(signals, vec![GroupSignal::Hangup]);
    // The whole output still went through the hub and the log.
    assert_eq!(h.logged(&run).len(), 100);
}

#[tokio::test]
async fn a_silent_run_should_be_stopped_as_idle() {
    let h = harness();
    let started = std::time::Instant::now();
    let run = h
        .start_limited(only(LimitOverride {
            idle_ms: Some(150),
            ..LimitOverride::default()
        }))
        .await;
    h.child(|c| c.print("GO"));
    h.until_ended().await;
    assert_limit_end(&h, &run, LimitKind::Idle);
    assert!(started.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn output_and_hook_calls_should_keep_a_run_from_going_idle() {
    let h = harness();
    let run = h
        .start_limited(only(LimitOverride {
            idle_ms: Some(150),
            ..LimitOverride::default()
        }))
        .await;
    for i in 0..8 {
        if i % 2 == 0 {
            h.child(|c| c.print("GO"));
        } else {
            h.hook(1, "event", "still here").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(h.runs.live_count(), 1);
    assert!(h.store.run(&run).status.is_live());
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.store.run(&run).status, RunStatus::Finished);
    assert!(h.limit_events().is_empty());
}

#[tokio::test]
async fn waiting_input_should_pause_the_idle_and_active_clocks() {
    let h = harness();
    let run = h
        .start_limited(only(LimitOverride {
            idle_ms: Some(150),
            active_ms: Some(400),
            ..LimitOverride::default()
        }))
        .await;
    h.child(|c| c.print("WAIT"));
    h.until_status(&run, RunStatus::WaitingInput).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.store.run(&run).status.is_live(), "stopped while waiting");
    // Leaving `waiting_input` restarts the idle clock, which then fires.
    let left = std::time::Instant::now();
    h.child(|c| c.print("GO"));
    h.until_ended().await;
    assert!(left.elapsed() >= Duration::from_millis(150));
    assert_limit_end(&h, &run, LimitKind::Idle);
}

#[tokio::test]
async fn a_busy_run_should_be_stopped_after_its_active_time() {
    let h = harness();
    let run = h
        .start_limited(only(LimitOverride {
            active_ms: Some(300),
            ..LimitOverride::default()
        }))
        .await;
    for _ in 0..200 {
        if h.runs.live_count() == 0 {
            break;
        }
        if let Some(out) = lock(&h.pty.children).last().and_then(|c| c.output.clone()) {
            let _ = out.send(b"GO ".to_vec());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.until_ended().await;
    assert_limit_end(&h, &run, LimitKind::Time);
}

#[tokio::test]
async fn a_limit_stop_should_deny_a_held_hook_with_the_limit_and_cancel_it() {
    let h = harness();
    let run = h
        .start_limited(only(LimitOverride {
            output_bytes: Some(64),
            ..LimitOverride::default()
        }))
        .await;
    let held = h.hold("k");
    h.until_status(&run, RunStatus::WaitingInput).await;
    h.child(|c| c.print(&"x".repeat(100)));
    assert_eq!(
        held.await.unwrap(),
        held_reply("deny:run stopped: output limit")
    );
    h.until_ended().await;
    assert_eq!(h.store.only_approval().status, ApprovalStatus::Cancelled);
    assert_limit_end(&h, &run, LimitKind::Output);
}

#[tokio::test]
async fn a_stop_during_a_limit_stop_should_change_nothing() {
    let adapter = Scripted {
        grace_ms: 5_000,
        ..Scripted::default()
    };
    let h = harness_with(adapter, FakePty::default());
    let run = h
        .start_limited(only(LimitOverride {
            output_bytes: Some(8),
            ..LimitOverride::default()
        }))
        .await;
    h.child(|c| c.print("0123456789"));
    h.until("the limit event", |h| !h.limit_events().is_empty())
        .await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    h.child(|c| c.exit(3));
    h.until_ended().await;
    assert_limit_end(&h, &run, LimitKind::Output);
    assert_eq!(h.store.run(&run).exit_code, Some(3));
}

#[tokio::test]
async fn a_run_stopping_for_run_stop_should_never_be_limit_checked() {
    let adapter = Scripted {
        grace_ms: 5_000,
        ..Scripted::default()
    };
    let h = harness_with(adapter, FakePty::default());
    let run = h
        .start_limited(only(LimitOverride {
            output_bytes: Some(8),
            ..LimitOverride::default()
        }))
        .await;
    h.child(|c| c.print("GO"));
    h.until_status(&run, RunStatus::Running).await;
    h.runs.stop(h.store.as_ref(), &run).await.unwrap();
    h.child(|c| c.print(&"x".repeat(100)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    let row = h.store.run(&run);
    assert_eq!(
        (row.status, row.end_reason),
        (RunStatus::Stopped, Some(EndReason::UserStop))
    );
    assert!(h.limit_events().is_empty());
}

#[tokio::test]
async fn limits_should_come_from_settings_and_warn_once_for_a_bad_value() {
    let h = harness();
    lock(&h.store.settings).insert(supervise::KEY_OUTPUT_BYTES.into(), JsonValue::Int(32));
    *lock(&h.store.project_settings) = Some(BTreeMap::from([(
        supervise::KEY_IDLE_MS.to_owned(),
        JsonValue::String("x".into()),
    )]));
    for _ in 0..10 {
        let run = h.start().await;
        h.child(|c| c.exit(0));
        h.until_ended().await;
        assert_eq!(h.store.run(&run).status, RunStatus::Finished);
    }
    // One entry for the bad project value, however many launches read it.
    assert_eq!(lock(&h.runs.bad_settings).len(), 1);
    // The global output limit applies (the bad project idle value was skipped).
    let run = h.start().await;
    h.child(|c| c.print(&"x".repeat(40)));
    h.until_ended().await;
    assert_limit_end(&h, &run, LimitKind::Output);
    assert_eq!(h.limit_events()[0].1, 32);
}

// ---- process facts, the sweep and tracking (spec 0004) ----

impl Harness {
    /// The `run_process` rows, sorted.
    fn rows(&self) -> Vec<(i64, i64, String, bool)> {
        let mut rows: Vec<_> = lock(&self.store.processes)
            .iter()
            .map(|((pid, at), (name, main))| (*pid, *at, name.clone(), *main))
            .collect();
        rows.sort();
        rows
    }
    fn trail(&self) -> Vec<String> {
        lock(&self.table.trail).clone()
    }
    fn at(&self, entry: &str) -> usize {
        let trail = self.trail();
        trail
            .iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("no {entry:?} in {trail:?}"))
    }
}

const CHILD_START: i64 = MAIN_START + 1000;

#[tokio::test]
async fn the_spawn_should_record_the_start_time_and_the_main_row() {
    let h = harness();
    let run = h.start().await;
    assert_eq!(h.store.run(&run).pid_started_at, Some(MAIN_START));
    assert_eq!(h.rows(), vec![(MAIN_PID, MAIN_START, "agent".into(), true)]);
    h.child(|c| c.exit(0));
    h.until_ended().await;
    // Reaped, so confirmed gone: its row goes too.
    assert!(h.rows().is_empty());
    assert!(h.trail().contains(&format!("reap {MAIN_PID}")));
}

#[tokio::test]
async fn no_start_time_should_leave_it_null_with_no_row_and_the_run_going() {
    let h = harness_on(Scripted::default(), FakePty::default(), MemTable::default());
    let run = h.start().await;
    assert_eq!(h.store.run(&run).pid_started_at, None);
    assert!(h.rows().is_empty());
    h.child(|c| c.print("hi"));
    h.until_status(&run, RunStatus::Running).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.store.run(&run).status, RunStatus::Finished);
}

#[tokio::test]
async fn the_scanner_should_track_children_seen_twice_and_drop_confirmed_gone_ones() {
    let h = harness();
    let _run = h.start().await;
    h.table.add(101, MAIN_PID, MAIN_PID, CHILD_START);
    h.until("child 101 tracked", |h| h.rows().iter().any(|r| r.0 == 101))
        .await;
    assert_eq!(
        h.rows()[0],
        (101, CHILD_START, "child101".into(), false),
        "{:?}",
        h.rows()
    );
    lock(&h.table.procs).retain(|(p, _)| p.pid != 101);
    h.until("child 101 untracked", |h| {
        !h.rows().iter().any(|r| r.0 == 101)
    })
    .await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
}

#[tokio::test]
async fn a_failed_scan_should_never_delete_a_row() {
    let h = harness();
    let _run = h.start().await;
    h.table.add(101, MAIN_PID, MAIN_PID, CHILD_START);
    h.until("child 101 tracked", |h| h.rows().iter().any(|r| r.0 == 101))
        .await;
    h.table.fail_scans.store(true, Ordering::SeqCst);
    // Missing from the (failing) table, but no scan says so.
    let gone = lock(&h.table.procs).remove(0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.rows().iter().any(|r| r.0 == 101));
    lock(&h.table.procs).push(gone);
    h.child(|c| c.exit(0));
    h.until_ended().await;
    // The sweep still stops it from the tracked set alone.
    assert!(!h.table.alive(101));
    assert!(h.rows().is_empty());
}

#[tokio::test]
async fn leftovers_should_get_sigterm_then_sigkill_and_the_main_reaped_last() {
    let h = harness();
    let run = h.start().await;
    // In the group; detached into its own group; detached and ignoring SIGTERM.
    h.table.add(101, MAIN_PID, MAIN_PID, CHILD_START);
    h.table.add(102, MAIN_PID, 102, CHILD_START);
    h.table.add(103, MAIN_PID, 103, CHILD_START);
    lock(&h.table.ignore_term).insert(103);
    h.until("children tracked", |h| h.rows().len() == 4).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.store.run(&run).status, RunStatus::Finished);
    assert!(!h.table.alive(101) && !h.table.alive(102) && !h.table.alive(103));
    assert!(h.at("group-term 101") < h.at("term 102"));
    assert!(h.at("term 103") < h.at("kill 103"));
    assert!(h.at("kill 103") < h.at(&format!("reap {MAIN_PID}")));
    assert!(!h.trail().contains(&"kill 102".to_owned()));
    assert!(h.rows().is_empty(), "{:?}", h.rows());
}

#[tokio::test]
async fn an_orphan_should_be_found_by_its_session_at_the_sweep() {
    let h = harness();
    let _run = h.start().await;
    // Started, then reparented to launchd, as the main exits at once:
    // no two scans in a row saw it, so only its session id finds it.
    h.table.add(101, MAIN_PID, 101, CHILD_START);
    lock(&h.table.procs)[0].0.ppid = Some(1);
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert!(!h.table.alive(101));
    assert!(h.at("term 101") < h.at(&format!("reap {MAIN_PID}")));
}

#[tokio::test]
async fn a_process_that_survives_sigkill_should_keep_its_row() {
    let h = harness();
    let _run = h.start().await;
    h.table.add(101, MAIN_PID, 101, CHILD_START);
    lock(&h.table.unkillable).insert(101);
    h.until("child tracked", |h| h.rows().iter().any(|r| r.0 == 101))
        .await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.rows(), vec![(101, CHILD_START, "child101".into(), false)]);
}

#[tokio::test]
async fn a_run_with_no_children_should_sweep_without_waiting() {
    let h = harness();
    let _run = h.start().await;
    h.child(|c| c.exit(0));
    let started = std::time::Instant::now();
    h.until_ended().await;
    // The term wait is 300 ms here; it is skipped.
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_closed_run_should_get_no_scanner_writes() {
    let h = harness();
    let _run = h.start().await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    h.table.add(101, MAIN_PID, MAIN_PID, CHILD_START);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(h.rows().is_empty());
}

// covers: AC-2
#[tokio::test]
async fn a_setting_changed_while_a_run_is_live_should_not_change_its_limits() {
    let h = harness();
    lock(&h.store.settings).insert(supervise::KEY_OUTPUT_BYTES.into(), JsonValue::Int(1000));
    let run = h.start().await;
    h.child(|c| c.print("GO"));
    h.until_status(&run, RunStatus::Running).await;
    // Lowered under what the run is about to print.
    lock(&h.store.settings).insert(supervise::KEY_OUTPUT_BYTES.into(), JsonValue::Int(32));
    h.child(|c| c.print(&"x".repeat(100)));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(h.store.run(&run).status.is_live());
    assert!(h.limit_events().is_empty());
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.store.run(&run).status, RunStatus::Finished);
    // The next run picks the new value up.
    let next = h.start().await;
    h.child(|c| c.print(&"x".repeat(40)));
    h.until_ended().await;
    assert_limit_end(&h, &next, LimitKind::Output);
    assert_eq!(h.limit_events()[0].1, 32);
}

// covers: AC-7
#[tokio::test]
async fn the_scanner_should_scan_only_while_a_run_is_live_or_sweeping() {
    let h = harness();
    let scans = |h: &Harness| h.table.scans.load(Ordering::SeqCst);
    // scan_every is 30 ms here: several ticks pass with nothing live.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(scans(&h), 0);
    let _run = h.start().await;
    h.until("a scan", |h| scans(h) > 0).await;
    h.child(|c| c.exit(0));
    h.until_ended().await;
    // Let a scan already under way finish counting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after = scans(&h);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(scans(&h), after);
}

// covers: AC-8
#[tokio::test]
async fn the_end_should_be_recorded_before_the_sweep_while_the_run_still_counts_as_live() {
    let h = harness();
    let run = h.start().await;
    // Its own group, ignoring SIGTERM: the sweep has to wait out its term wait.
    h.table.add(101, MAIN_PID, 101, CHILD_START);
    lock(&h.table.ignore_term).insert(101);
    h.until("child tracked", |h| h.rows().iter().any(|r| r.0 == 101))
        .await;
    h.child(|c| c.exit(0));
    h.until_status(&run, RunStatus::Finished).await;
    // The sweep is still running: the child is alive and the run still live.
    assert!(h.table.alive(101));
    assert_eq!(h.runs.live_count(), 1);
    h.until_ended().await;
    assert!(!h.table.alive(101));
    assert!(h.at("term 101") < h.at("kill 101"));
}

// ---- fault isolation (spec 0004, AC-11) ----

fn assert_supervisor_error(h: &Harness, run: &RunId) {
    let row = h.store.run(run);
    assert_eq!(
        (row.status, row.end_reason, row.exit_code),
        (RunStatus::Failed, Some(EndReason::SupervisorError), None)
    );
    assert!(!h.runs.token_is_live(&format!("{:032x}", 1)));
    assert!(lock(&h.runtime.removed).contains(run));
}

#[tokio::test]
async fn a_reader_thread_panic_should_end_only_its_run_as_supervisor_error() {
    let h = harness();
    let run = h.start().await;
    let held = h.hold("k");
    h.until_status(&run, RunStatus::WaitingInput).await;
    h.child(|c| {
        c.output
            .as_ref()
            .unwrap()
            .send(READER_PANIC.to_vec())
            .unwrap()
    });
    h.until_ended().await;
    assert_supervisor_error(&h, &run);
    // No reply: the helper falls back on its own.
    assert_eq!(held.await.unwrap(), Err(HookError::TokenUnknown));
    assert_eq!(h.store.only_approval().status, ApprovalStatus::Cancelled);
    let (input, signals) = h.child(|c| (lock(&c.input).clone(), lock(&c.signals).clone()));
    // No adapter stop input: SIGHUP straight away, then the sweep.
    assert!(!input.ends_with(b"\x03"), "{input:?}");
    assert_eq!(before_sweep(&signals), vec![GroupSignal::Hangup]);
    assert!(h.trail().contains(&format!("reap {MAIN_PID}")));
}

#[tokio::test]
async fn a_decoder_panic_should_be_ended_by_the_monitor_and_spare_other_runs() {
    let h = harness();
    let broken = h.start().await;
    let other = h.start().await;
    h.child(|c| c.print("GO"));
    h.until_status(&other, RunStatus::Running).await;
    let held = h.hold("k");
    h.until_status(&broken, RunStatus::WaitingInput).await;
    // The first run's decoder panics inside its driver task.
    assert_eq!(h.hook(1, "panic", "").await, Err(HookError::TokenUnknown));
    assert_eq!(held.await.unwrap(), Err(HookError::TokenUnknown));
    h.until("the broken run's end", |h| {
        h.store.run(&broken).status.is_terminal()
    })
    .await;
    assert_supervisor_error(&h, &broken);
    assert_eq!(h.store.only_approval().status, ApprovalStatus::Cancelled);
    // The other run still works and ends on its own.
    assert_eq!(h.hook(2, "event", "still here").await, ok_reply());
    assert!(h.store.run(&other).status.is_live());
    h.child(|c| c.exit(0));
    h.until_ended().await;
    assert_eq!(h.store.run(&other).status, RunStatus::Finished);
}

#[tokio::test]
async fn a_panic_after_the_end_should_skip_the_end_steps_but_still_sweep() {
    let h = harness();
    let run = h.start().await;
    h.table.add(101, MAIN_PID, MAIN_PID, CHILD_START);
    h.store.panic_after_end.store(true, Ordering::SeqCst);
    h.child(|c| c.exit(0));
    h.until_ended().await;
    let row = h.store.run(&run);
    assert_eq!(
        (row.status, row.end_reason),
        (RunStatus::Finished, Some(EndReason::Exited))
    );
    assert!(!h.table.alive(101));
    assert!(lock(&h.runtime.removed).contains(&run));
}

#[test]
fn the_run_id_should_be_readable_from_the_task_and_thread_locals() {
    assert_eq!(current_run_id(), None);
    fault::set_thread_run(Some("run_x".into()));
    assert_eq!(current_run_id().as_deref(), Some("run_x"));
    fault::set_thread_run(None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let id = RunId::new();
    let seen = rt.block_on(fault::with_run_id(&id, async { current_run_id() }));
    assert_eq!(seen, Some(id.to_string()));
}
