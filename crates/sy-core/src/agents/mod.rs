//! Agent runs (spec 0003): the use case that drives the adapter contract.
//!
//! [`AgentRuns::start`] creates a run and launches it in AC-4's order:
//! `create_run` (`queued`), the hub entry, `starting`, the signal token, the
//! launch dir, the adapter's `launch`, the plan's files, the program check,
//! the spawn in a new PTY, then `set_run_process`. Any failure after
//! `starting` ends the run `failed` with `spawn_failed`, its reason written
//! through the hub as the log's first line.
//!
//! Each launched run then has one driver task (see `driver`) that owns its
//! [`RunDecoder`](sy_adapter_api::RunDecoder), so every decoder call for a run
//! happens on one task, in order. It takes two lanes: a droppable copy of the
//! output (bounded, so the hub and the log never wait on the decoder) and a
//! lossless control lane (first output, hook calls, stop, exit). Threads
//! around it do the blocking work: one reads the PTY into the hub, one writes
//! input to it, and one waits for the process.
//!
//! Hook calls arrive through [`AgentRuns::hook`]: the run's signal token
//! names its driver's control lane, the driver asks the decoder, applies its
//! reports (status, journal events, the session ref) and answers. A token is
//! valid only while its run is live; the driver revokes it as the run's end
//! starts (AC-17).
//!
//! Approvals change only on the driver too (AC-10): a held hook, a decision
//! from [`AgentRuns::respond`], a closed hook, a stop and the run's end all
//! reach it on the control lane, so the first change wins and a held hook's
//! one reply always matches its row.

mod approvals;
pub mod detect;
mod driver;
pub mod launch;
mod limit;
pub mod ports;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use sy_adapter_api::{
    AgentAdapter, HookCall, HookReply, LaunchRequest, PtySize, RunDecoder, SignalEndpoint,
    serde_json,
};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument;

use crate::adapters::AdapterRegistry;
use crate::ids::{ApprovalId, RunId};
use crate::model::{
    Actor, AdapterEvent, Approval, ApprovalDecision, ApprovalStatus, EndReason, NewApproval,
    NewRun, Run, RunEnd, RunKind, RunStatus, RunTransition, Workspace,
};
use crate::ports::{Clock, Committed, RunProcess, Store, StoreError};
use crate::runs::{RunControl, RunError, RunHub, RunWriter};
pub use approvals::{ApprovalNotice, RespondError, new_approval};
use driver::{Control, Driver};
use launch::{AllowedProgram, agent_env, allowed_program, valid_rel_path};
pub use limit::{SIGNAL_BURST, SIGNAL_RATE_PER_SEC};
use ports::{
    ChildProcess, GroupSignal, LoginEnv, Pty, PtyChild, PtyIo, RuntimeDir, SpawnRequest,
    TokenSource,
};

/// The PTY reader's largest read.
pub const READ_CHUNK: usize = 16 * 1024;
/// Chunks the decoder's output lane holds before chunks are dropped for it.
pub const DECODER_LANE: usize = 256;
/// The longest grace an adapter's stop plan may ask for.
pub const MAX_STOP_GRACE: Duration = Duration::from_millis(10_000);
/// Input writes queued for the PTY writer thread.
const WRITER_QUEUE: usize = 64;
/// At most one "dropped decoder chunks" warning per run in this window.
const DROP_WARN_EVERY: Duration = Duration::from_secs(60);

/// The writes agent runs make. A narrow view of [`Store`] so the run logic
/// is tested without a database; [`StoreWrites`] is the real one.
#[async_trait::async_trait]
pub trait RunWrites: Send + Sync {
    /// `Store::create_run`.
    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Run, StoreError>;
    /// `Store::get_run`.
    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError>;
    /// `Store::transition_run`.
    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Run, StoreError>;
    /// `Store::set_run_process`.
    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError>;
    /// `Store::set_agent_session_ref`.
    async fn set_agent_session_ref(&self, run: &RunId, session_ref: &str)
    -> Result<(), StoreError>;
    /// `Store::append_event`.
    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<(), StoreError>;
    /// `Store::create_approval`.
    async fn create_approval(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        new: NewApproval,
    ) -> Result<Committed<Approval>, StoreError>;
    /// `Store::decide_approval`.
    async fn decide_approval(
        &self,
        actor: Actor,
        approval: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Committed<Approval>, StoreError>;
    /// `Store::get_approval`.
    async fn get_approval(&self, approval: &ApprovalId) -> Result<Approval, StoreError>;
    /// `Store::cancel_pending_approvals`.
    async fn cancel_pending_approvals(
        &self,
        actor: Actor,
        run: &RunId,
        message: Option<String>,
    ) -> Result<Committed<Vec<Approval>>, StoreError>;
}

/// [`RunWrites`] through the real [`Store`] (whose committed events the
/// engine broadcasts as `run.status`).
pub struct StoreWrites(pub Arc<dyn Store>);

#[async_trait::async_trait]
impl RunWrites for StoreWrites {
    async fn create_run(&self, actor: Actor, new: NewRun) -> Result<Run, StoreError> {
        Ok(self.0.create_run(actor, new).await?.value)
    }
    async fn get_run(&self, run: &RunId) -> Result<Run, StoreError> {
        self.0.get_run(run).await
    }
    async fn transition_run(
        &self,
        actor: Actor,
        run: &RunId,
        change: RunTransition,
    ) -> Result<Run, StoreError> {
        Ok(self.0.transition_run(actor, run, change).await?.value)
    }
    async fn set_run_process(&self, run: &RunId, process: RunProcess) -> Result<Run, StoreError> {
        self.0.set_run_process(run, process).await
    }
    async fn set_agent_session_ref(
        &self,
        run: &RunId,
        session_ref: &str,
    ) -> Result<(), StoreError> {
        self.0
            .set_agent_session_ref(run, session_ref)
            .await
            .map(|_| ())
    }
    async fn append_event(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        run: &RunId,
        event: AdapterEvent,
    ) -> Result<(), StoreError> {
        self.0
            .append_event(actor, actor_ref, run, event)
            .await
            .map(|_| ())
    }
    async fn create_approval(
        &self,
        actor: Actor,
        actor_ref: Option<String>,
        new: NewApproval,
    ) -> Result<Committed<Approval>, StoreError> {
        self.0.create_approval(actor, actor_ref, new).await
    }
    async fn decide_approval(
        &self,
        actor: Actor,
        approval: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Committed<Approval>, StoreError> {
        self.0.decide_approval(actor, approval, decision).await
    }
    async fn get_approval(&self, approval: &ApprovalId) -> Result<Approval, StoreError> {
        self.0.get_approval(approval).await
    }
    async fn cancel_pending_approvals(
        &self,
        actor: Actor,
        run: &RunId,
        message: Option<String>,
    ) -> Result<Committed<Vec<Approval>>, StoreError> {
        self.0.cancel_pending_approvals(actor, run, message).await
    }
}

/// How the stop sequence and the run end are timed (spec 0003, AC-6 and AC-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// After SIGHUP, how long before SIGKILL.
    pub hangup_wait: Duration,
    /// After SIGKILL, how long to wait for the exit before ending anyway.
    pub kill_wait: Duration,
    /// After the exit, how long to keep reading the PTY for its last output.
    pub drain: Duration,
    /// The longest graceful step during engine shutdown.
    pub shutdown_grace: Duration,
    /// How often `on_tick` runs.
    pub tick: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            hangup_wait: Duration::from_secs(5),
            kill_wait: Duration::from_secs(1),
            drain: Duration::from_secs(1),
            shutdown_grace: Duration::from_millis(3000),
            tick: Duration::from_millis(250),
        }
    }
}

/// What [`AgentRuns`] is built from.
pub struct AgentRunsDeps {
    /// Every adapter.
    pub adapters: AdapterRegistry,
    /// Where output goes.
    pub hub: Arc<RunHub>,
    /// Spawns agents.
    pub pty: Arc<dyn Pty>,
    /// The launch dirs.
    pub runtime: Arc<dyn RuntimeDir>,
    /// The login environment.
    pub login: Arc<dyn LoginEnv>,
    /// Mints signal tokens.
    pub tokens: Arc<dyn TokenSource>,
    /// The time source.
    pub clock: Arc<dyn Clock>,
    /// The `sy-signal` helper's absolute path.
    pub signal_helper: String,
    /// The engine socket's path.
    pub socket: String,
    /// Stop and end timings.
    pub timings: Timings,
    /// Where approval changes go, for the engine to send to its clients.
    pub approvals: mpsc::UnboundedSender<ApprovalNotice>,
}

/// One run to launch.
#[derive(Debug, Clone)]
pub struct StartRun {
    /// Its workspace: agent id, options and worktree come from here.
    pub workspace: Workspace,
    /// The prompt.
    pub prompt: String,
    /// The terminal size.
    pub size: PtySize,
}

/// Why [`AgentRuns::start`] made no run (a run that fails to launch is a
/// `failed` run, not an error).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// No adapter has the workspace's agent id.
    #[error("no agent adapter {0}")]
    UnknownAgent(String),
    /// The store refused to create or start the run.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The run hub refused the run.
    #[error(transparent)]
    Hub(#[from] RunError),
}

/// Why [`AgentRuns::stop`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopError {
    /// No run has that id.
    #[error("run not found")]
    NotFound,
    /// The run already ended.
    #[error("run is not live")]
    NotLive,
    /// The store failed.
    #[error(transparent)]
    Store(StoreError),
}

/// Why a hook call got no decoder reply. `sy-signal` answers each of these
/// with the fallback in its own arguments (the agent's own prompt).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    /// The token names no live run (unknown, or revoked when the run ended).
    #[error("the signal token names no live run")]
    TokenUnknown,
    /// The run's hook calls are over their rate; this call's reports were dropped.
    #[error("too many hook calls; this one was dropped")]
    RateLimited,
    /// No reply from the decoder can be given (no approval row could be
    /// created, or its key is already pending).
    #[error("{0}")]
    Fallback(String),
}

/// Ends a hook call. If the caller stops waiting first (the helper's
/// connection closed), dropping it tells the driver (AC-10: `expired`).
struct OpenCall {
    control: mpsc::UnboundedSender<Control>,
    call: u64,
    open: bool,
}

impl Drop for OpenCall {
    fn drop(&mut self) {
        if self.open {
            let _ = self.control.send(Control::HookClosed { call: self.call });
        }
    }
}

struct LiveRun {
    control: mpsc::UnboundedSender<Control>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every agent run this engine launched and still drives.
pub struct AgentRuns {
    adapters: AdapterRegistry,
    hub: Arc<RunHub>,
    pty: Arc<dyn Pty>,
    runtime: Arc<dyn RuntimeDir>,
    login: Arc<dyn LoginEnv>,
    tokens: Arc<dyn TokenSource>,
    clock: Arc<dyn Clock>,
    signal_helper: String,
    socket: String,
    timings: Timings,
    approvals: mpsc::UnboundedSender<ApprovalNotice>,
    /// Numbers each hook call, so a closed one can be named.
    next_call: AtomicU64,
    live: Mutex<HashMap<RunId, LiveRun>>,
    /// Signal token to its driver's control lane, while the run is live.
    signals: Mutex<HashMap<String, mpsc::UnboundedSender<Control>>>,
    /// Runs still between their start and the end of their end sequence.
    drivers: watch::Sender<usize>,
}

/// What a successful launch hands the driver.
struct Launched {
    child: PtyChild,
    decoder: Box<dyn RunDecoder>,
    initial_input: Option<Vec<u8>>,
}

impl AgentRuns {
    /// The agent runs use case.
    pub fn new(deps: AgentRunsDeps) -> Arc<Self> {
        Arc::new(Self {
            adapters: deps.adapters,
            hub: deps.hub,
            pty: deps.pty,
            runtime: deps.runtime,
            login: deps.login,
            tokens: deps.tokens,
            clock: deps.clock,
            signal_helper: deps.signal_helper,
            socket: deps.socket,
            timings: deps.timings,
            approvals: deps.approvals,
            next_call: AtomicU64::new(1),
            live: Mutex::new(HashMap::new()),
            signals: Mutex::new(HashMap::new()),
            drivers: watch::Sender::new(0),
        })
    }

    /// Every registered adapter.
    pub fn adapters(&self) -> &AdapterRegistry {
        &self.adapters
    }

    /// How many runs are between their start and the end of their end sequence.
    pub fn live_count(&self) -> usize {
        *self.drivers.borrow()
    }

    /// Watch [`AgentRuns::live_count`]: drain and idle exit wait on it, so the
    /// engine never exits before a run's end is recorded and announced.
    pub fn watch_live(&self) -> watch::Receiver<usize> {
        self.drivers.subscribe()
    }

    /// True when the run has a live signal token.
    pub fn token_is_live(&self, token: &str) -> bool {
        lock(&self.signals).contains_key(token)
    }

    /// One hook call from `sy-signal` (AC-7). The run's driver asks its
    /// decoder, applies the reports, charges the rate limit for a reply given
    /// at once, and answers. The payload size was checked by the caller.
    /// Dropping the returned future (the helper's connection closed) tells
    /// the driver nobody waits for the answer any more.
    pub async fn hook(&self, token: &str, call: HookCall) -> Result<HookReply, HookError> {
        let started = Instant::now();
        let (hook, size) = (call.hook.clone(), call.stdin.len());
        let control = lock(&self.signals).get(token).cloned();
        let answer = match control {
            None => Err(HookError::TokenUnknown),
            Some(control) => {
                let (reply, answer) = oneshot::channel();
                let id = self.next_call.fetch_add(1, Ordering::Relaxed);
                if control.send(Control::Hook { id, call, reply }).is_err() {
                    Err(HookError::TokenUnknown)
                } else {
                    let mut open = OpenCall {
                        control,
                        call: id,
                        open: true,
                    };
                    // A dropped reply means the run's end started first.
                    let answer = answer.await.unwrap_or(Err(HookError::TokenUnknown));
                    open.open = false;
                    answer
                }
            }
        };
        tracing::debug!(
            hook,
            size,
            outcome = match &answer {
                Ok(_) => "reply",
                Err(HookError::TokenUnknown) => "token_unknown",
                Err(HookError::RateLimited) => "rate_limited",
                Err(HookError::Fallback(_)) => "fallback",
            },
            latency_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            "hook call"
        );
        answer
    }

    /// `approval.respond` (AC-10): hand the decision to the run's driver,
    /// the only place approvals change, and wait for its answer. Returns the
    /// decided approval even if the agent can no longer take the reply.
    pub async fn respond(
        &self,
        store: &dyn RunWrites,
        approval: &ApprovalId,
        approve: bool,
        message: Option<String>,
    ) -> Result<Approval, RespondError> {
        let row = store
            .get_approval(approval)
            .await
            .map_err(|err| match err {
                StoreError::NotFound => RespondError::NotFound,
                other => RespondError::Store(other),
            })?;
        if row.status != ApprovalStatus::Pending {
            return Err(RespondError::NotPending);
        }
        // No driver: the run's end is under way, and it cancels the row.
        let control = lock(&self.live)
            .get(&row.run_id)
            .map(|r| r.control.clone())
            .ok_or(RespondError::NotPending)?;
        let (answer, answered) = oneshot::channel();
        control
            .send(Control::Decide {
                approval: approval.clone(),
                approve,
                message,
                answer,
            })
            .map_err(|_| RespondError::NotPending)?;
        answered.await.unwrap_or(Err(RespondError::NotPending))
    }

    /// Send an approval change to the engine's clients.
    fn announce(&self, notice: ApprovalNotice) {
        let _ = self.approvals.send(notice);
    }

    /// Create one run in `req.workspace` and launch it (AC-4). Returns the
    /// run id once it is running or has failed to launch.
    pub async fn start(
        self: &Arc<Self>,
        store: Arc<dyn RunWrites>,
        req: StartRun,
    ) -> Result<RunId, StartError> {
        let adapter = self
            .adapters
            .get(&req.workspace.agent_id)
            .ok_or_else(|| StartError::UnknownAgent(req.workspace.agent_id.clone()))?;
        let id = RunId::new();
        store
            .create_run(
                Actor::User,
                NewRun {
                    id: id.clone(),
                    workspace_id: req.workspace.id.clone(),
                    kind: RunKind::Agent,
                    input: None,
                    resumes_run_id: None,
                },
            )
            .await?;
        let (writer, hub_control) = self.hub.create_run(id.clone())?;
        if let Err(err) = store
            .transition_run(Actor::Engine, &id, RunTransition::to(RunStatus::Starting))
            .await
        {
            self.remove_from_hub(&id).await;
            return Err(err.into());
        }
        let token = self.tokens.mint();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        lock(&self.live).insert(
            id.clone(),
            LiveRun {
                control: control_tx.clone(),
            },
        );
        lock(&self.signals).insert(token.clone(), control_tx.clone());
        self.drivers.send_modify(|n| *n += 1);

        let span = tracing::info_span!("run", run_id = %id, agent_id = %adapter.id());
        match self
            .launch(adapter.as_ref(), &req, &id, &token, store.as_ref())
            .instrument(span.clone())
            .await
        {
            Ok(launched) => {
                if let Err(reason) = self.drive(
                    store.clone(),
                    &id,
                    req.workspace.id.to_string(),
                    token.clone(),
                    launched,
                    writer,
                    hub_control,
                    control_tx,
                    control_rx,
                    span.clone(),
                ) {
                    self.fail_start(store.as_ref(), &id, &token, None, &reason)
                        .instrument(span)
                        .await;
                }
            }
            Err(reason) => {
                self.fail_start(store.as_ref(), &id, &token, Some(&writer), &reason)
                    .instrument(span)
                    .await;
            }
        }
        Ok(id)
    }

    /// Everything from the launch dir to the spawn. The `Err` text is the
    /// reason line for the run's log, never arguments or environment values.
    async fn launch(
        &self,
        adapter: &dyn AgentAdapter,
        req: &StartRun,
        id: &RunId,
        token: &str,
        store: &dyn RunWrites,
    ) -> Result<Launched, String> {
        let runtime = Arc::clone(&self.runtime);
        let run = id.clone();
        let launch_dir = blocking(move || runtime.create(&run))
            .await
            .map_err(|e| format!("could not create the launch dir: {e}"))?;
        let options: serde_json::Value = serde_json::from_str(&req.workspace.agent_options)
            .map_err(|e| format!("agent options are not valid JSON: {e}"))?;
        let login = self.login.snapshot().await;
        let launch = adapter
            .launch(&LaunchRequest {
                run_id: id.to_string(),
                prompt: req.prompt.clone(),
                cwd: req.workspace.worktree_path.clone(),
                options,
                resume: None,
                launch_dir,
                signal: SignalEndpoint {
                    helper: self.signal_helper.clone(),
                    socket: self.socket.clone(),
                    token: token.to_owned(),
                },
                size: req.size,
            })
            .map_err(|e| e.to_string())?;
        let plan = launch.plan;

        for file in &plan.files {
            if !valid_rel_path(&file.rel_path) {
                tracing::warn!("refused a launch file outside the launch dir");
                return Err(format!(
                    "launch file {:?} is outside the launch dir",
                    file.rel_path
                ));
            }
            let runtime = Arc::clone(&self.runtime);
            let (run, rel, contents) = (id.clone(), file.rel_path.clone(), file.contents.clone());
            blocking(move || runtime.write_file(&run, &rel, &contents))
                .await
                .map_err(|e| format!("could not write launch file {:?}: {e}", file.rel_path))?;
        }

        let program = match allowed_program(&adapter.programs(), &plan.program) {
            None => {
                tracing::warn!("refused a program the adapter did not list");
                return Err(format!(
                    "{:?} is not one of this adapter's programs",
                    plan.program
                ));
            }
            Some(AllowedProgram::Absolute(path)) => path,
            Some(AllowedProgram::Bare(name)) => {
                let resolver = Arc::clone(&self.login);
                let (env, bare) = (Arc::clone(&login), name.clone());
                blocking(move || Ok(resolver.resolve(&env, &bare)))
                    .await
                    .ok()
                    .flatten()
                    .ok_or_else(|| format!("{name:?} was not found on the login PATH"))?
            }
        };
        let env = agent_env(&login.vars, &plan, &self.socket, token);
        let pty = Arc::clone(&self.pty);
        tracing::info!(
            program = program.rsplit('/').next().unwrap_or_default(),
            args = plan.args.len(),
            "launching"
        );
        let spawn = SpawnRequest {
            program,
            args: plan.args.clone(),
            cwd: req.workspace.worktree_path.clone(),
            env,
            size: req.size,
        };
        let child = blocking(move || pty.spawn(spawn).map_err(|e| std::io::Error::other(e.0)))
            .await
            .map_err(|e| format!("could not start the agent: {e}"))?;
        if let Err(err) = store
            .set_run_process(
                id,
                RunProcess {
                    pid: child.pid,
                    pgid: child.pgid,
                    pid_started_at: None,
                },
            )
            .await
        {
            tracing::warn!(%err, "could not record the run's process");
        }
        Ok(Launched {
            child,
            decoder: launch.decoder,
            initial_input: plan.initial_input,
        })
    }

    /// Start the threads and the driver task for a launched run.
    #[expect(clippy::too_many_arguments, reason = "one call site, the start path")]
    fn drive(
        self: &Arc<Self>,
        store: Arc<dyn RunWrites>,
        id: &RunId,
        workspace: String,
        token: String,
        launched: Launched,
        writer: RunWriter,
        hub_control: mpsc::Receiver<RunControl>,
        control_tx: mpsc::UnboundedSender<Control>,
        control_rx: mpsc::UnboundedReceiver<Control>,
        span: tracing::Span,
    ) -> Result<(), String> {
        let Launched {
            child,
            decoder,
            initial_input,
        } = launched;
        let PtyChild {
            reader,
            io,
            process,
            ..
        } = child;
        let (output_tx, output_rx) = mpsc::channel(DECODER_LANE);
        let (done_tx, done_rx) = oneshot::channel();
        let (write_tx, write_rx) = mpsc::channel(WRITER_QUEUE);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let cut = Arc::new(AtomicBool::new(false));

        let threads = (|| {
            spawn_reader(
                reader,
                writer,
                output_tx,
                control_tx.clone(),
                done_tx,
                Arc::clone(&cut),
                span.clone(),
            )?;
            spawn_writer(io, write_rx, span.clone())?;
            spawn_waiter(Arc::clone(&process), control_tx, span.clone())
        })();
        if let Err(err) = threads {
            process.signal_group(GroupSignal::Kill);
            return Err(format!("could not start the run's threads: {err}"));
        }
        tokio::spawn(pump(hub_control, input_rx, write_tx));
        let driver = Driver::new(
            Arc::clone(self),
            store,
            id.clone(),
            workspace,
            token,
            decoder,
            process,
            control_rx,
            output_rx,
            done_rx,
            cut,
            input_tx,
            initial_input,
        );
        tokio::spawn(driver.run().instrument(span));
        Ok(())
    }

    /// End a run that never launched, in AC-17's order: revoke the token,
    /// write the reason line and do the final flush, `failed` with
    /// `spawn_failed`, then remove the launch dir.
    async fn fail_start(
        &self,
        store: &dyn RunWrites,
        id: &RunId,
        token: &str,
        writer: Option<&RunWriter>,
        reason: &str,
    ) {
        tracing::warn!("the run could not be launched");
        self.revoke(token);
        if let Some(writer) = writer {
            writer.write(format!("switchyard: {reason}\r\n").as_bytes());
        }
        let log_bytes = self.remove_from_hub(id).await;
        let end = RunTransition::end(
            RunStatus::Failed,
            RunEnd {
                reason: EndReason::SpawnFailed,
                exit_code: None,
                log_bytes,
            },
        );
        if let Err(err) = store.transition_run(Actor::Engine, id, end).await {
            tracing::warn!(%err, "could not record the failed launch");
        }
        self.remove_launch_dir(id).await;
        self.finish(id);
    }

    /// The final flush and delivery; returns the run's `log_bytes`.
    async fn remove_from_hub(&self, id: &RunId) -> u64 {
        let hub = Arc::clone(&self.hub);
        let run = id.clone();
        tokio::task::spawn_blocking(move || hub.remove(&run))
            .await
            .ok()
            .flatten()
            .map_or(0, |removed| removed.log_bytes)
    }

    async fn remove_launch_dir(&self, id: &RunId) {
        let runtime = Arc::clone(&self.runtime);
        let run = id.clone();
        if let Err(err) = blocking(move || runtime.remove(&run)).await {
            tracing::warn!(%err, "could not remove the launch dir");
        }
    }

    /// New hook calls with this token are refused from now on.
    fn revoke(&self, token: &str) {
        lock(&self.signals).remove(token);
    }

    /// The run's end sequence is over.
    fn finish(&self, id: &RunId) {
        lock(&self.live).remove(id);
        self.drivers.send_modify(|n| *n = n.saturating_sub(1));
    }

    /// `run.stop` (AC-12). Returns once the stop has started. A run already
    /// stopping (or ending) is a no op; a `queued` run with no driver goes
    /// straight to `stopped`.
    pub async fn stop(&self, store: &dyn RunWrites, run: &RunId) -> Result<(), StopError> {
        let control = lock(&self.live).get(run).map(|r| r.control.clone());
        if let Some(control) = control {
            let (ack, done) = oneshot::channel();
            // A closed lane means the run's end sequence already started.
            if control
                .send(Control::Stop {
                    shutdown: false,
                    ack,
                })
                .is_ok()
                && done.await.is_ok()
            {
                return Ok(());
            }
        }
        let row = store.get_run(run).await.map_err(|err| match err {
            StoreError::NotFound => StopError::NotFound,
            other => StopError::Store(other),
        })?;
        if row.status.is_live() {
            // Ending right now, its exit seen first: a no op.
            return Ok(());
        }
        if row.status != RunStatus::Queued {
            return Err(StopError::NotLive);
        }
        store
            .transition_run(
                Actor::User,
                run,
                RunTransition::end(
                    RunStatus::Stopped,
                    RunEnd {
                        reason: EndReason::UserStop,
                        exit_code: None,
                        log_bytes: 0,
                    },
                ),
            )
            .await
            .map(|_| ())
            .map_err(StopError::Store)
    }

    /// Engine shutdown: stop every live run at once (the graceful step capped
    /// at [`Timings::shutdown_grace`]) and wait until each has ended.
    pub async fn stop_all(&self) {
        let controls: Vec<_> = lock(&self.live)
            .values()
            .map(|r| r.control.clone())
            .collect();
        for control in controls {
            let (ack, _done) = oneshot::channel();
            let _ = control.send(Control::Stop {
                shutdown: true,
                ack,
            });
        }
        let mut drivers = self.drivers.subscribe();
        let _ = drivers.wait_for(|n| *n == 0).await;
    }
}

/// Run blocking work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(std::io::Error::other)?
}

/// What the writer thread does.
enum PtyCommand {
    Write(Vec<u8>),
    Resize(PtySize),
}

/// Merge `run.input`/`run.resize` (the hub's bounded lane, so a full one
/// answers `InputBusy`) and the driver's own input (never busy) into the
/// writer thread's queue, waiting for a free slot. Ends when both close.
async fn pump(
    mut hub: mpsc::Receiver<RunControl>,
    mut driver: mpsc::UnboundedReceiver<Vec<u8>>,
    writer: mpsc::Sender<PtyCommand>,
) {
    let (mut hub_open, mut driver_open) = (true, true);
    while hub_open || driver_open {
        let command = tokio::select! {
            message = hub.recv(), if hub_open => match message {
                Some(RunControl::Input(data)) => PtyCommand::Write(data),
                Some(RunControl::Resize { cols, rows }) => PtyCommand::Resize(PtySize { cols, rows }),
                None => { hub_open = false; continue }
            },
            data = driver.recv(), if driver_open => match data {
                Some(data) => PtyCommand::Write(data),
                None => { driver_open = false; continue }
            },
        };
        if writer.send(command).await.is_err() {
            break;
        }
    }
}

fn spawn_writer(
    mut io: Box<dyn PtyIo>,
    mut queue: mpsc::Receiver<PtyCommand>,
    span: tracing::Span,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            let _span = span.enter();
            while let Some(command) = queue.blocking_recv() {
                let result = match command {
                    PtyCommand::Write(data) => io.write_all(&data),
                    PtyCommand::Resize(size) => io.resize(size),
                };
                if let Err(err) = result {
                    tracing::debug!(%err, "terminal write failed");
                }
            }
            // Dropping `io` closes the PTY master's write side.
        })
        .map(|_| ())
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    writer: RunWriter,
    decoder: mpsc::Sender<Vec<u8>>,
    control: mpsc::UnboundedSender<Control>,
    done: oneshot::Sender<()>,
    cut: Arc<AtomicBool>,
    span: tracing::Span,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let _span = span.enter();
            let mut buf = vec![0_u8; READ_CHUNK];
            let mut first = true;
            let mut dropped: u64 = 0;
            let mut warned: Option<Instant> = None;
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if cut.load(Ordering::SeqCst) {
                    break;
                }
                let chunk = &buf[..n];
                writer.write(chunk);
                if first {
                    first = false;
                    let _ = control.send(Control::FirstOutput);
                }
                if let Err(mpsc::error::TrySendError::Full(_)) = decoder.try_send(chunk.to_vec()) {
                    dropped += 1;
                    if warned.is_none_or(|at| at.elapsed() >= DROP_WARN_EVERY) {
                        warned = Some(Instant::now());
                        tracing::warn!(
                            dropped,
                            "the decoder fell behind; dropped output copies for it"
                        );
                    }
                }
            }
            let _ = done.send(());
        })
        .map(|_| ())
}

fn spawn_waiter(
    process: Arc<dyn ChildProcess>,
    control: mpsc::UnboundedSender<Control>,
    span: tracing::Span,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pty-waiter".into())
        .spawn(move || {
            let _span = span.enter();
            let exit = process.wait().unwrap_or_else(|err| {
                tracing::warn!(%err, "could not wait for the agent");
                sy_adapter_api::ExitInfo {
                    code: None,
                    signal: None,
                }
            });
            let _ = control.send(Control::Exit(exit));
        })
        .map(|_| ())
}
