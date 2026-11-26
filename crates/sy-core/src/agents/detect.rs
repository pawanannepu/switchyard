//! Detection (spec 0003, AC-2, AC-3): which agents are installed and signed
//! in, asked of each CLI through its adapter's probes, never by reading its
//! files. Results live in memory only.
//!
//! A pass runs every adapter's probes, in registration order. At most one
//! pass is in flight: [`Detector::detect`] joins a running pass instead of
//! starting another, and returns when it ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sy_adapter_api::{AgentAdapter, Capabilities, Detection, ProbeOutcome, ProbeResult};
use tokio::sync::watch;

use super::launch::{AllowedProgram, allowed_program};
use super::ports::{LoginEnv, LoginEnvSnapshot, ProbeRequest, ProcessRunner, RuntimeDir};
use crate::adapters::AdapterRegistry;
use crate::ports::Clock;

/// The longest a probe may run; an adapter may ask for less.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One adapter's last detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionInfo {
    /// What the adapter made of its probes.
    pub detection: Detection,
    /// The first probe's resolved program path, when it resolved.
    pub path: Option<String>,
    /// When the pass ended, Unix ms UTC.
    pub checked_at: i64,
}

/// An adapter as `agent.list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// Its id.
    pub id: String,
    /// Its name for people.
    pub display_name: String,
    /// What it can do.
    pub capabilities: Capabilities,
    /// Its last detection; `None` until the first pass ends.
    pub detection: Option<DetectionInfo>,
}

/// What [`Detector`] is built from.
pub struct DetectorDeps {
    /// Every adapter.
    pub adapters: AdapterRegistry,
    /// Runs the probes.
    pub runner: Arc<dyn ProcessRunner>,
    /// The environment probes run with.
    pub login: Arc<dyn LoginEnv>,
    /// Gives the probes' working directory.
    pub runtime: Arc<dyn RuntimeDir>,
    /// The time source.
    pub clock: Arc<dyn Clock>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Detection passes and their last results.
pub struct Detector {
    deps: DetectorDeps,
    results: Mutex<HashMap<String, DetectionInfo>>,
    /// Set while a pass runs; it turns true when that pass ends.
    in_flight: Mutex<Option<watch::Receiver<bool>>>,
}

impl Detector {
    /// The detection use case.
    pub fn new(deps: DetectorDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            results: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(None),
        })
    }

    /// Every adapter, in registration order, with its last detection.
    pub fn list(&self) -> Vec<AgentInfo> {
        let results = lock(&self.results);
        self.deps
            .adapters
            .all()
            .iter()
            .map(|a| AgentInfo {
                id: a.id().to_owned(),
                display_name: a.display_name().to_owned(),
                capabilities: a.capabilities(),
                detection: results.get(a.id()).cloned(),
            })
            .collect()
    }

    /// Run a pass, or join the one in flight, and return when it ends.
    pub async fn detect(self: &Arc<Self>) {
        let mut done = self.start();
        let _ = done.wait_for(|ended| *ended).await;
    }

    /// Start a pass in the background (or join the one in flight).
    pub fn spawn(self: &Arc<Self>) {
        drop(self.start());
    }

    fn start(self: &Arc<Self>) -> watch::Receiver<bool> {
        let mut in_flight = lock(&self.in_flight);
        if let Some(done) = in_flight.as_ref() {
            return done.clone();
        }
        let (tx, rx) = watch::channel(false);
        *in_flight = Some(rx.clone());
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.pass().await;
            *lock(&this.in_flight) = None;
            let _ = tx.send(true);
        });
        rx
    }

    async fn pass(&self) {
        let env = self.deps.login.snapshot().await;
        let mut found = HashMap::new();
        for adapter in self.deps.adapters.all() {
            let (results, path) = self.probe(adapter.as_ref(), &env).await;
            found.insert(
                adapter.id().to_owned(),
                DetectionInfo {
                    detection: adapter.interpret_detection(&results),
                    path,
                    checked_at: 0,
                },
            );
        }
        let now = self.deps.clock.now_ms();
        let mut results = lock(&self.results);
        for (id, mut info) in found {
            info.checked_at = now;
            results.insert(id, info);
        }
    }

    /// Run an adapter's probes. Returns their results and the first one's path.
    async fn probe(
        &self,
        adapter: &dyn AgentAdapter,
        env: &Arc<LoginEnvSnapshot>,
    ) -> (Vec<ProbeResult>, Option<String>) {
        let programs = adapter.programs();
        let mut results = Vec::new();
        let mut first_path = None;
        for (i, probe) in adapter.detection_probes().into_iter().enumerate() {
            let resolved = match allowed_program(&programs, &probe.program) {
                None => {
                    tracing::warn!(
                        agent = adapter.id(),
                        "refused a probe of a program the adapter did not list"
                    );
                    Err(ProbeOutcome::Refused)
                }
                Some(AllowedProgram::Absolute(path)) => Ok(path),
                Some(AllowedProgram::Bare(name)) => {
                    let (login, env) = (Arc::clone(&self.deps.login), Arc::clone(env));
                    tokio::task::spawn_blocking(move || login.resolve(&env, &name))
                        .await
                        .ok()
                        .flatten()
                        .ok_or(ProbeOutcome::NotFound)
                }
            };
            if i == 0 {
                first_path = resolved.as_ref().ok().cloned();
            }
            let result = match resolved {
                Err(outcome) => ProbeResult {
                    probe,
                    outcome,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                Ok(program) => self.run(probe, program, env).await,
            };
            if matches!(result.outcome, ProbeOutcome::TimedOut) {
                tracing::warn!(agent = adapter.id(), "a detection probe timed out");
            }
            results.push(result);
        }
        (results, first_path)
    }

    async fn run(
        &self,
        probe: sy_adapter_api::Probe,
        program: String,
        env: &LoginEnvSnapshot,
    ) -> ProbeResult {
        let mut vars: Vec<(String, String)> = env
            .vars
            .iter()
            .filter(|(k, _)| k != "NO_COLOR" && k != "TERM")
            .cloned()
            .collect();
        vars.push(("NO_COLOR".into(), "1".into()));
        vars.push(("TERM".into(), "dumb".into()));
        let timeout = match probe.timeout_ms {
            0 => PROBE_TIMEOUT,
            ms => Duration::from_millis(ms.into()).min(PROBE_TIMEOUT),
        };
        let req = ProbeRequest {
            program,
            args: probe.args.clone(),
            cwd: self.deps.runtime.root(),
            env: vars,
            timeout,
        };
        let runner = Arc::clone(&self.deps.runner);
        let ran = tokio::task::spawn_blocking(move || runner.run(req))
            .await
            .map_err(|e| ProbeOutcome::SpawnError(e.to_string()));
        match ran {
            Ok(run) => ProbeResult {
                probe,
                outcome: run.outcome,
                stdout: run.stdout,
                stderr: run.stderr,
            },
            Err(outcome) => ProbeResult {
                probe,
                outcome,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        }
    }
}
