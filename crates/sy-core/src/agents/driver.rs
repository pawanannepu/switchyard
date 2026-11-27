//! One run's driver task: the only owner of its decoder, and the only place
//! its status, hook answers, approvals, stop and end are decided (spec 0003,
//! AC-6 to AC-10, AC-12, AC-17).
//!
//! Approvals: a held hook (`HookResponse::Hold`) or a typed request
//! (`Report::ApprovalRequested`) becomes a `pending` row. It leaves `pending`
//! exactly once, here: your answer (`Control::Decide`), the decoder seeing it
//! answered in the terminal, the held hook's connection closing, a stop, or
//! the run's end. Whatever wins, a held hook gets one reply that matches the
//! row, and every change is announced after its commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sy_adapter_api::{
    AgentEvent, ApprovalRequest, Decision, ExitInfo, ExitOutcome, FileChange as ApiFileChange,
    HookCall, HookReply, HookResponse, LiveStatus, MessageRole as ApiMessageRole, Report,
    RunDecoder, SettledOutcome, Verdict,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use super::approvals::{ApprovalNotice, RespondError, new_approval};
use super::fault::RunKit;
use super::limit::RateLimit;
use super::ports::{ChildProcess, GroupSignal};
use super::supervise::{LimitClocks, LimitHit, Limits, OutputMeter};
use super::{AgentRuns, HookError, MAX_STOP_GRACE, RunWrites};
use crate::ids::{ApprovalId, RunId};
use crate::model::{
    Actor, AdapterEvent, Approval, ApprovalDecision, DECIDED_IN_INBOX, DECIDED_IN_TERMINAL,
    EndReason, FileChange, MessageRole, RunEnd, RunStatus, RunTransition,
};
use crate::ports::StoreError;

/// At most one "rate limited" warning per run in this window.
const LIMIT_WARN_EVERY_MS: i64 = 60_000;

/// The message a stop answers held hooks with (AC-12).
pub(super) const STOP_MESSAGE: &str = "run stopped";

type HookAnswer = oneshot::Sender<Result<HookReply, HookError>>;

/// The lossless lane into a driver.
pub(super) enum Control {
    /// The PTY produced its first byte.
    FirstOutput,
    /// The process exited (observed, not reaped: the sweep reaps it).
    Exit(ExitInfo),
    /// A hook call through `sy-signal`. A dropped `reply` reads as a revoked token.
    Hook {
        /// Names the call, for [`Control::HookClosed`].
        id: u64,
        call: HookCall,
        reply: HookAnswer,
    },
    /// The helper stopped waiting for call `call` (its connection closed).
    HookClosed { call: u64 },
    /// `approval.respond`.
    Decide {
        approval: ApprovalId,
        approve: bool,
        message: Option<String>,
        answer: oneshot::Sender<Result<Approval, RespondError>>,
    },
    /// `run.stop`, or engine shutdown (whose graceful step is capped).
    Stop {
        cause: StopCause,
        ack: oneshot::Sender<()>,
    },
    /// A PTY thread panicked (spec 0004, AC-11): end the run as
    /// `supervisor_error`.
    SupervisorFault,
}

/// Why a run is being stopped. The first cause wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopCause {
    /// `run.stop`.
    User,
    /// Engine shutdown.
    Shutdown,
    /// A run limit (spec 0004, AC-6).
    Limit(LimitHit),
}

impl StopCause {
    const fn actor(self) -> Actor {
        match self {
            Self::User => Actor::User,
            Self::Shutdown | Self::Limit(_) => Actor::Engine,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The stop input was typed; waiting the plan's grace.
    Grace,
    /// SIGHUP was sent; waiting before SIGKILL.
    Hangup,
    /// SIGKILL was sent; waiting for the exit.
    Kill,
}

struct Stopping {
    phase: Phase,
    deadline: Instant,
}

/// A held hook, waiting for its approval to be decided.
struct Held {
    call: u64,
    reply: HookAnswer,
}

/// One of the run's `pending` approvals.
struct Pending {
    id: ApprovalId,
    /// The decoder's own key for it.
    key: String,
    /// The waiting hook, for a held one; `None` for one answered by typing.
    held: Option<Held>,
}

pub(super) struct Driver {
    runs: Arc<AgentRuns>,
    store: Arc<dyn RunWrites>,
    run: RunId,
    /// The run's workspace id: `actor_ref` on the events it journals.
    workspace: String,
    token: String,
    decoder: Box<dyn RunDecoder>,
    process: Arc<dyn ChildProcess>,
    control: mpsc::UnboundedReceiver<Control>,
    output: mpsc::Receiver<Vec<u8>>,
    reader_done: oneshot::Receiver<()>,
    cut: Arc<AtomicBool>,
    input: Option<mpsc::UnboundedSender<Vec<u8>>>,
    initial_input: Option<Vec<u8>>,
    /// The status as stored (the last transition that landed).
    status: RunStatus,
    /// The decoder's last status report.
    reported: Option<LiveStatus>,
    first_output: bool,
    /// A stop seen before the exit, and why: it decides the run's end.
    stop_cause: Option<StopCause>,
    stopping: Option<Stopping>,
    /// The run's limits, fixed at its start (spec 0004, AC-2).
    limits: Limits,
    /// The idle and active clocks and the output count.
    clocks: LimitClocks,
    /// The reader's byte count and last read.
    meter: Arc<OutputMeter>,
    /// What the fault end needs.
    kit: Arc<RunKit>,
    /// Hook calls answered at once (AC-7).
    limit: RateLimit,
    /// Hook calls refused by the rate limit.
    limited: u64,
    limit_warned_at: Option<i64>,
    /// The run's `pending` approvals, oldest first.
    pending: Vec<Pending>,
    /// The end sequence started: the token is revoked, so no new approval.
    ending: bool,
}

impl Driver {
    #[expect(clippy::too_many_arguments, reason = "built once, by the start path")]
    pub(super) fn new(
        runs: Arc<AgentRuns>,
        store: Arc<dyn RunWrites>,
        run: RunId,
        workspace: String,
        token: String,
        decoder: Box<dyn RunDecoder>,
        process: Arc<dyn ChildProcess>,
        control: mpsc::UnboundedReceiver<Control>,
        output: mpsc::Receiver<Vec<u8>>,
        reader_done: oneshot::Receiver<()>,
        cut: Arc<AtomicBool>,
        input: mpsc::UnboundedSender<Vec<u8>>,
        initial_input: Option<Vec<u8>>,
        limits: Limits,
        meter: Arc<OutputMeter>,
        spawned: std::time::Instant,
        kit: Arc<RunKit>,
    ) -> Self {
        Self {
            runs,
            store,
            run,
            workspace,
            token,
            decoder,
            process,
            control,
            output,
            reader_done,
            cut,
            input: Some(input),
            initial_input,
            status: RunStatus::Starting,
            reported: None,
            first_output: false,
            stop_cause: None,
            stopping: None,
            limits,
            clocks: LimitClocks::new(spawned),
            meter,
            kit,
            limit: RateLimit::new(),
            limited: 0,
            limit_warned_at: None,
            pending: Vec::new(),
            ending: false,
        }
    }

    fn now_ms(&self) -> i64 {
        self.runs.clock.now_ms()
    }

    pub(super) async fn run(mut self) {
        let mut tick = tokio::time::interval(self.runs.timings.tick);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let exit = 'run: loop {
            let deadline = self.stopping.as_ref().map(|s| s.deadline);
            let stop_timer = async move {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                message = self.control.recv() => match message {
                    Some(Control::Exit(info)) => break 'run Some(info),
                    Some(Control::SupervisorFault) => {
                        // No reply to any held hook: dropping them makes `sy-signal` fall back.
                        self.pending.clear();
                        let (runs, store, run, kit) = (
                            Arc::clone(&self.runs),
                            Arc::clone(&self.store),
                            self.run.clone(),
                            Arc::clone(&self.kit),
                        );
                        drop(self);
                        runs.fault_end(store.as_ref(), &run, &kit).await;
                        return;
                    }
                    Some(Control::FirstOutput) => {
                        self.first_output = true;
                        self.update_status().await;
                    }
                    Some(Control::Stop { cause, ack }) => {
                        self.begin_stop(cause).await;
                        let _ = ack.send(());
                    }
                    Some(Control::Hook { id, call, reply }) => {
                        // Any hook call that reaches the driver is activity (AC-3).
                        self.clocks.activity(std::time::Instant::now());
                        self.on_hook(id, &call, reply).await;
                    }
                    Some(Control::HookClosed { call }) => self.hook_closed(call).await,
                    Some(Control::Decide { approval, approve, message, answer }) => {
                        let result = self.respond(&approval, approve, message).await;
                        let _ = answer.send(result);
                    }
                    None => break None,
                },
                Some(chunk) = self.output.recv() => {
                    let reports = self.decoder.on_output(&chunk, self.now_ms());
                    self.apply(reports).await;
                }
                () = stop_timer => {
                    if self.advance_stop() {
                        break None;
                    }
                }
                _ = tick.tick() => {
                    let reports = self.decoder.on_tick(self.now_ms());
                    self.apply(reports).await;
                    self.check_limits().await;
                }
            }
        };
        self.end(exit).await;
    }

    /// One hook call (AC-7): the decoder answers, then the rate limit is
    /// charged for a reply given at once. A refused call's reports are
    /// dropped, and the helper falls back. A held call is never charged.
    async fn on_hook(&mut self, id: u64, call: &HookCall, reply: HookAnswer) {
        let now = self.now_ms();
        let outcome = self.decoder.on_hook(call, now);
        match outcome.response {
            HookResponse::Reply(answer) => {
                let answer = if self.limit.take(now) {
                    self.apply(outcome.reports).await;
                    Ok(answer)
                } else {
                    self.rate_limited(now);
                    Err(HookError::RateLimited)
                };
                let _ = reply.send(answer);
            }
            HookResponse::Hold(request) => {
                self.apply(outcome.reports).await;
                self.request(request, Some(Held { call: id, reply })).await;
            }
        }
    }

    fn rate_limited(&mut self, now: i64) {
        self.limited += 1;
        if self
            .limit_warned_at
            .is_none_or(|at| now.saturating_sub(at) >= LIMIT_WARN_EVERY_MS)
        {
            self.limit_warned_at = Some(now);
            tracing::warn!(
                limited = self.limited,
                "the agent's hook calls are over their rate limit; dropped their reports"
            );
        }
    }

    async fn apply(&mut self, reports: Vec<Report>) {
        let mut status_changed = false;
        for report in reports {
            match report {
                Report::Status(status) => {
                    self.reported = Some(status);
                    status_changed = true;
                }
                // Journaled as the agent's; never a status change (AC-8).
                Report::Event(event) => {
                    if let Err(err) = self
                        .store
                        .append_event(
                            Actor::Agent,
                            Some(self.workspace.clone()),
                            &self.run,
                            adapter_event(event),
                        )
                        .await
                    {
                        tracing::warn!(%err, "could not journal the agent's event");
                    }
                }
                // The last one wins.
                Report::SessionRef(session) => {
                    if let Err(err) = self.store.set_agent_session_ref(&self.run, &session).await {
                        tracing::warn!(%err, "could not record the agent's session");
                    }
                }
                Report::ApprovalRequested(request) => self.request(request, None).await,
                Report::ApprovalSettled { key, outcome } => self.settle(&key, outcome).await,
            }
        }
        if status_changed {
            self.update_status().await;
        }
    }

    // ---- approvals ----

    /// A new request (AC-9): a `pending` row and its event, announced, and
    /// the run `waiting_input`. With no row (a duplicate key, the run ending,
    /// storage refusing it) there is no approval, and a held hook gets an
    /// error, so the helper falls back to the agent's own prompt.
    async fn request(&mut self, request: ApprovalRequest, held: Option<Held>) {
        let refuse = |held: Option<Held>, why: &str| {
            if let Some(held) = held {
                let _ = held.reply.send(Err(HookError::Fallback(why.to_owned())));
            }
        };
        if self.ending {
            refuse(held, "the run is ending");
            return;
        }
        if self.pending.iter().any(|p| p.key == request.key) {
            tracing::warn!("dropped an approval request whose key is already pending");
            refuse(held, "an approval with this key is already pending");
            return;
        }
        let key = request.key.clone();
        let new = new_approval(&self.run, request);
        match self
            .store
            .create_approval(Actor::Agent, Some(self.workspace.clone()), new)
            .await
        {
            Ok(committed) => {
                let id = committed.value.id.clone();
                self.runs.announce(ApprovalNotice::Requested {
                    event_id: committed.events.first().map(|e| e.id),
                    approval: committed.value,
                });
                self.pending.push(Pending { id, key, held });
                self.update_status().await;
            }
            Err(err) => {
                tracing::warn!(%err, "could not create the approval; the agent asks on its own");
                refuse(held, "the approval could not be saved");
            }
        }
    }

    /// Take `pending[index]` out of `pending` as `decision`, and announce it.
    /// The entry always leaves the list: on an error the row's state is not
    /// this run's to keep waiting on (it ended elsewhere, or is gone).
    async fn decide(
        &mut self,
        index: usize,
        actor: Actor,
        decision: ApprovalDecision,
    ) -> (Pending, Result<Approval, StoreError>) {
        let entry = self.pending.remove(index);
        let result = self
            .store
            .decide_approval(actor, &entry.id, decision)
            .await
            .map(|committed| {
                self.runs.announce(ApprovalNotice::Decided {
                    event_id: committed.events.first().map(|e| e.id),
                    approval: committed.value.clone(),
                });
                committed.value
            });
        if let Err(err) = &result {
            tracing::warn!(%err, "an approval change did not land");
        }
        (entry, result)
    }

    /// Answer a held hook (an error makes the helper fall back), or type the
    /// decoder's input for a typed request.
    fn deliver(&mut self, entry: Pending, decision: Result<&Decision, &str>) {
        match (entry.held, decision) {
            (Some(held), Ok(decision)) => {
                let reply = self.decoder.hold_reply(&entry.key, decision);
                let _ = held.reply.send(Ok(reply));
            }
            (Some(held), Err(why)) => {
                let _ = held.reply.send(Err(HookError::Fallback(why.to_owned())));
            }
            (None, Ok(decision)) => {
                let bytes = self.decoder.input_reply(&entry.key, decision);
                self.send_input(bytes);
            }
            (None, Err(_)) => {}
        }
    }

    /// `approval.respond` (AC-10).
    async fn respond(
        &mut self,
        approval: &ApprovalId,
        approve: bool,
        message: Option<String>,
    ) -> Result<Approval, RespondError> {
        let Some(index) = self.pending.iter().position(|p| &p.id == approval) else {
            return Err(RespondError::NotPending);
        };
        let decision = ApprovalDecision::by_user(approve, DECIDED_IN_INBOX, message.clone());
        let (entry, result) = self.decide(index, Actor::User, decision).await;
        let answer = match result {
            Ok(approval) => {
                let decision = Decision {
                    verdict: if approve {
                        Verdict::Approve
                    } else {
                        Verdict::Deny
                    },
                    message,
                };
                self.deliver(entry, Ok(&decision));
                Ok(approval)
            }
            Err(err) => {
                self.deliver(entry, Err("the approval could not be decided"));
                Err(match err {
                    StoreError::ApprovalNotPending(_) => RespondError::NotPending,
                    StoreError::NotFound => RespondError::NotFound,
                    other => RespondError::Store(other),
                })
            }
        };
        self.update_status().await;
        answer
    }

    /// The decoder saw a request answered in the terminal (AC-10). A held
    /// hook gets the reply for what it saw, so the agent never shows a
    /// prompt you already answered.
    async fn settle(&mut self, key: &str, outcome: SettledOutcome) {
        let Some(index) = self.pending.iter().position(|p| p.key == key) else {
            tracing::debug!("a settled approval matched no pending one");
            return;
        };
        let decision = match outcome {
            SettledOutcome::Approved => ApprovalDecision::by_user(true, DECIDED_IN_TERMINAL, None),
            SettledOutcome::Denied => ApprovalDecision::by_user(false, DECIDED_IN_TERMINAL, None),
            SettledOutcome::Unknown => ApprovalDecision::cancelled(None),
        };
        let (mut entry, _) = self.decide(index, Actor::User, decision).await;
        if let Some(held) = entry.held.take() {
            let verdict = match outcome {
                SettledOutcome::Approved => Verdict::Approve,
                SettledOutcome::Denied | SettledOutcome::Unknown => Verdict::Deny,
            };
            let reply = self.decoder.hold_reply(
                &entry.key,
                &Decision {
                    verdict,
                    message: None,
                },
            );
            let _ = held.reply.send(Ok(reply));
        }
        self.update_status().await;
    }

    /// A held hook's connection closed first (the agent's own hook timeout):
    /// its approval is `expired`.
    async fn hook_closed(&mut self, call: u64) {
        let Some(index) = self
            .pending
            .iter()
            .position(|p| p.held.as_ref().is_some_and(|h| h.call == call))
        else {
            return;
        };
        let _ = self
            .decide(index, Actor::Engine, ApprovalDecision::expired())
            .await;
        self.update_status().await;
    }

    /// Every held hook's approval becomes `cancelled`, then the hook gets a
    /// deny with `message` (a stop, AC-12). Typed requests stay pending.
    async fn deny_held(&mut self, actor: Actor, message: Option<&str>) {
        while let Some(index) = self.pending.iter().position(|p| p.held.is_some()) {
            let decision = ApprovalDecision::cancelled(message.map(str::to_owned));
            let (entry, _) = self.decide(index, actor, decision).await;
            let deny = Decision {
                verdict: Verdict::Deny,
                message: message.map(str::to_owned),
            };
            self.deliver(entry, Ok(&deny));
        }
    }

    // ---- status ----

    /// The AC-6 rule: `waiting_input` while any approval is pending or the
    /// decoder last said so; otherwise, once started (first output, any
    /// decoder report, or already past `starting`), `running`.
    fn wanted_status(&self) -> Option<RunStatus> {
        if !self.pending.is_empty() {
            return Some(RunStatus::WaitingInput);
        }
        if !self.first_output && self.reported.is_none() && self.status == RunStatus::Starting {
            return None;
        }
        Some(if self.reported == Some(LiveStatus::WaitingInput) {
            RunStatus::WaitingInput
        } else {
            RunStatus::Running
        })
    }

    async fn update_status(&mut self) {
        let Some(want) = self.wanted_status() else {
            return;
        };
        // Once the end started, only a run still `starting` moves (it wrote
        // output before it exited); otherwise the next status is its end.
        if want == self.status
            || self.status.is_terminal()
            || (self.ending && self.status != RunStatus::Starting)
        {
            return;
        }
        match self
            .store
            .transition_run(Actor::Engine, &self.run, RunTransition::to(want))
            .await
        {
            Ok(run) => {
                // Leaving `waiting_input` is activity (AC-3).
                if self.status == RunStatus::WaitingInput && run.status != RunStatus::WaitingInput {
                    self.clocks.activity(std::time::Instant::now());
                }
                self.status = run.status;
            }
            Err(StoreError::InvalidTransition(err)) => {
                tracing::debug!(%err, "status report refused by the state machine");
            }
            Err(err) => tracing::warn!(%err, "could not record the run's status"),
        }
        if self.status == RunStatus::Running
            && let Some(bytes) = self.initial_input.take()
        {
            self.send_input(bytes);
        }
    }

    fn send_input(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(input) = &self.input {
            let _ = input.send(bytes);
        }
    }

    // ---- stop and end ----

    /// The limit check on a tick (spec 0004, AC-6): only while the run is
    /// live and not already stopping. A hit journals `run_limit_exceeded`,
    /// then starts the stop sequence.
    async fn check_limits(&mut self) {
        if self.stopping.is_some() || self.ending || self.status.is_terminal() {
            return;
        }
        let counting = matches!(self.status, RunStatus::Starting | RunStatus::Running);
        if let Some(at) = self.meter.last_read() {
            self.clocks.activity(at);
        }
        self.clocks
            .tick(std::time::Instant::now(), counting, self.meter.bytes());
        let Some(hit) = self.clocks.check(&self.limits) else {
            return;
        };
        tracing::info!(
            limit = hit.kind.as_str(),
            threshold = hit.threshold,
            observed = hit.observed,
            "run limit reached; stopping the run"
        );
        let event = AdapterEvent::RunLimitExceeded {
            run_id: self.run.clone(),
            limit: hit.kind,
            threshold: hit.threshold,
            observed: hit.observed,
        };
        if let Err(err) = self
            .store
            .append_event(Actor::Engine, None, &self.run, event)
            .await
        {
            tracing::warn!(%err, "could not journal the run limit");
        }
        self.begin_stop(StopCause::Limit(hit)).await;
    }

    /// A stop (AC-12, and spec 0004 AC-6 for a limit): every held hook is
    /// denied with "run stopped" (or "run stopped: <limit> limit") and its
    /// approval cancelled, then the decoder's stop input is typed and the
    /// grace starts. A stop while already stopping changes nothing, except
    /// that shutdown shortens a long grace.
    async fn begin_stop(&mut self, cause: StopCause) {
        let shutdown = cause == StopCause::Shutdown;
        let cap = self.runs.timings.shutdown_grace;
        if let Some(stopping) = &mut self.stopping {
            // Already stopping. Shutdown only shortens a long grace.
            if shutdown && stopping.phase == Phase::Grace {
                stopping.deadline = stopping.deadline.min(Instant::now() + cap);
            }
            return;
        }
        self.stop_cause = Some(cause);
        let message = match cause {
            StopCause::Limit(hit) => format!("{STOP_MESSAGE}: {} limit", hit.kind.as_str()),
            StopCause::User | StopCause::Shutdown => STOP_MESSAGE.to_owned(),
        };
        self.deny_held(cause.actor(), Some(&message)).await;
        self.update_status().await;
        let plan = self.decoder.stop();
        let mut grace = std::time::Duration::from_millis(plan.grace_ms.into()).min(MAX_STOP_GRACE);
        if shutdown {
            grace = grace.min(cap);
        }
        tracing::info!(grace_ms = grace.as_millis(), shutdown, "stopping");
        self.send_input(plan.input);
        self.stopping = Some(Stopping {
            phase: Phase::Grace,
            deadline: Instant::now() + grace,
        });
    }

    /// The stop timer fired. Returns true to give up waiting for the exit.
    fn advance_stop(&mut self) -> bool {
        let timings = self.runs.timings;
        let Some(stopping) = &mut self.stopping else {
            return false;
        };
        match stopping.phase {
            Phase::Grace => {
                self.process.signal_group(GroupSignal::Hangup);
                stopping.phase = Phase::Hangup;
                stopping.deadline = Instant::now() + timings.hangup_wait;
                false
            }
            Phase::Hangup => {
                self.process.signal_group(GroupSignal::Kill);
                stopping.phase = Phase::Kill;
                stopping.deadline = Instant::now() + timings.kill_wait;
                false
            }
            Phase::Kill => {
                tracing::warn!("the agent did not exit after SIGKILL; ending the run anyway");
                true
            }
        }
    }

    /// The run's end, in AC-17's order.
    async fn end(mut self, exit: Option<ExitInfo>) {
        // (2) No new hook calls, and so no new approvals.
        self.ending = true;
        self.runs.revoke(&self.token);
        // (3) Every held hook gets a deny.
        for entry in &mut self.pending {
            if let Some(held) = entry.held.take() {
                let deny = Decision {
                    verdict: Verdict::Deny,
                    message: None,
                };
                let reply = self.decoder.hold_reply(&entry.key, &deny);
                let _ = held.reply.send(Ok(reply));
            }
        }
        // (4) Every pending approval becomes `cancelled`, announced.
        if !self.pending.is_empty() {
            self.pending.clear();
            match self
                .store
                .cancel_pending_approvals(Actor::Engine, &self.run, None)
                .await
            {
                Ok(committed) => {
                    for (i, approval) in committed.value.into_iter().enumerate() {
                        self.runs.announce(ApprovalNotice::Decided {
                            event_id: committed.events.get(i).map(|e| e.id),
                            approval,
                        });
                    }
                }
                Err(err) => tracing::warn!(%err, "could not cancel the run's approvals"),
            }
        }

        // (5) The PTY drain: its last output, then let the reader go.
        let drain = self.runs.timings.drain;
        let _ = tokio::time::timeout(drain, &mut self.reader_done).await;
        self.cut.store(true, Ordering::SeqCst);
        self.control.close();
        self.initial_input = None;
        while let Ok(message) = self.control.try_recv() {
            match message {
                Control::FirstOutput => self.first_output = true,
                // An exit seen first: a stop after it is a no op.
                Control::Stop { ack, .. } => {
                    let _ = ack.send(());
                }
                // The token is revoked: dropping the reply says so.
                Control::Hook { reply, .. } => drop(reply),
                // The run's end won: its approvals are already cancelled.
                Control::Decide { answer, .. } => {
                    let _ = answer.send(Err(RespondError::NotPending));
                }
                Control::HookClosed { .. } | Control::Exit(_) | Control::SupervisorFault => {}
            }
        }
        while let Ok(chunk) = self.output.try_recv() {
            let reports = self.decoder.on_output(&chunk, self.now_ms());
            self.apply(reports).await;
        }
        // A run that wrote output before it exited was running.
        self.update_status().await;
        let exit = exit.unwrap_or(ExitInfo {
            code: None,
            signal: None,
        });
        let outcome = self.decoder.on_exit(&exit);
        self.input = None;
        let log_bytes = self.runs.remove_from_hub(&self.run).await;

        // (6) The terminal transition, whose commit sends `run.status`.
        let (status, reason, actor) = match self.stop_cause {
            Some(cause @ (StopCause::User | StopCause::Shutdown)) => {
                (RunStatus::Stopped, EndReason::UserStop, cause.actor())
            }
            Some(StopCause::Limit(_)) => {
                (RunStatus::Failed, EndReason::LimitExceeded, Actor::Engine)
            }
            None => {
                let status = match outcome {
                    ExitOutcome::Finished => RunStatus::Finished,
                    ExitOutcome::Failed => RunStatus::Failed,
                };
                (status, EndReason::Exited, Actor::Engine)
            }
        };
        tracing::info!(
            code = exit.code,
            signal = exit.signal,
            status = status.as_str(),
            "run ended"
        );
        let end = RunTransition::end(
            status,
            RunEnd {
                reason,
                exit_code: exit.code,
                log_bytes,
            },
        );
        if let Err(err) = self.store.transition_run(actor, &self.run, end).await {
            tracing::warn!(%err, "could not record the run's end");
        }
        // The terminal transition: the scanner stops writing for this run.
        self.runs.ended(&self.run);
        // (7) The launch dir goes, whatever step 6's outcome. The monitor
        // then sweeps the run's leftovers and reaps the main process.
        self.runs.remove_launch_dir(&self.run).await;
    }
}

/// The contract's event as the journal's.
fn adapter_event(event: AgentEvent) -> AdapterEvent {
    match event {
        AgentEvent::Message { role, text } => AdapterEvent::Message {
            role: match role {
                ApiMessageRole::User => MessageRole::User,
                ApiMessageRole::Agent => MessageRole::Agent,
                ApiMessageRole::System => MessageRole::System,
            },
            text,
        },
        AgentEvent::CommandRun { argv, exit_code } => AdapterEvent::CommandRun { argv, exit_code },
        AgentEvent::FileChanged {
            path,
            change,
            from_path,
        } => AdapterEvent::FileChanged {
            path,
            change: match change {
                ApiFileChange::Added => FileChange::Added,
                ApiFileChange::Modified => FileChange::Modified,
                ApiFileChange::Deleted => FileChange::Deleted,
                ApiFileChange::Renamed => FileChange::Renamed,
            },
            from_path,
        },
    }
}
