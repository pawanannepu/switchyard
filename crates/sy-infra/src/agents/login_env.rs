//! The login environment agents and probes run with (spec 0001, *Login
//! environment*; spec 0003, AC-16).
//!
//! [`ShellLoginEnv`] asks your login shell: `$SHELL -l -i -c` with a script
//! that prints a start marker, `env -0`, then an end marker, with no stdin
//! and a 5 second timeout. Only the bytes between the markers count, so
//! whatever your startup files print is ignored. If that fails, it falls
//! back to the engine's own environment with
//! `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`, and says why.
//!
//! [`InheritedEnv`] is the engine's own environment, taken once at start:
//! the devtools `SWITCHYARD_LOGIN_ENV=inherit` seam, so tests don't depend
//! on your shell.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use sy_adapter_api::ProbeOutcome;
use sy_core::agents::ports::{LoginEnv, LoginEnvSnapshot};
use tokio::sync::{Mutex, watch};

use super::process::run_capped;

/// The `PATH` used when the login shell can't be asked.
pub const FALLBACK_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
/// How long the login shell gets.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// The shell used when `SHELL` is unset (the macOS default).
const DEFAULT_SHELL: &str = "/bin/zsh";
/// Most bytes of the shell's output that are read.
const CAPTURE_BYTES: usize = 4 * 1024 * 1024;

const BEGIN: &[u8] = b"__SWITCHYARD_ENV_BEGIN__";
const END: &[u8] = b"__SWITCHYARD_ENV_END__";
/// The script the login shell runs. Fixed text: nothing is interpolated.
const SCRIPT: &str =
    "printf '%s' __SWITCHYARD_ENV_BEGIN__; env -0; printf '%s' __SWITCHYARD_ENV_END__";

/// The variables between the markers, or `None` if they aren't both there.
fn parse(output: &[u8]) -> Option<Vec<(String, String)>> {
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let start = find(output, BEGIN)? + BEGIN.len();
    let len = find(&output[start..], END)?;
    let vars = output[start..start + len]
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let text = std::str::from_utf8(entry).ok()?;
            let (key, value) = text.split_once('=')?;
            (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect();
    Some(vars)
}

/// How to ask the login shell.
struct Capture {
    shell: String,
    timeout: Duration,
    /// The engine's own environment: what the shell starts from, and the fallback.
    base: Vec<(String, String)>,
}

/// Asks the login shell, caches the answer, and asks again on `refresh`.
pub struct ShellLoginEnv {
    how: Arc<Capture>,
    capture: watch::Sender<Option<Arc<LoginEnvSnapshot>>>,
    /// One capture at a time.
    capturing: Mutex<()>,
}

impl ShellLoginEnv {
    /// Ask `shell` (or `$SHELL`, or `/bin/zsh`), starting from the engine's
    /// own environment, with a 5 second timeout. Nothing runs until
    /// [`LoginEnv::refresh`].
    pub fn new(shell: Option<String>) -> Arc<Self> {
        Self::with(shell, std::env::vars().collect(), CAPTURE_TIMEOUT)
    }

    /// [`ShellLoginEnv::new`] with an explicit environment and timeout (tests).
    pub fn with(
        shell: Option<String>,
        base: Vec<(String, String)>,
        timeout: Duration,
    ) -> Arc<Self> {
        let shell = shell
            .or_else(|| {
                base.iter()
                    .find(|(k, _)| k == "SHELL")
                    .map(|(_, v)| v.clone())
            })
            .filter(|s| s.starts_with('/'))
            .unwrap_or_else(|| DEFAULT_SHELL.to_owned());
        Arc::new(Self {
            how: Arc::new(Capture {
                shell,
                timeout,
                base,
            }),
            capture: watch::Sender::new(None),
            capturing: Mutex::new(()),
        })
    }
}

impl Capture {
    /// One capture, blocking.
    fn run(&self) -> LoginEnvSnapshot {
        let mut cmd = Command::new(&self.shell);
        cmd.args(["-l", "-i", "-c", SCRIPT])
            .env_clear()
            .envs(self.base.iter().map(|(k, v)| (k, v)));
        let captured = run_capped(cmd, self.timeout, CAPTURE_BYTES);
        let why = match captured.outcome {
            ProbeOutcome::TimedOut => format!(
                "your login shell ({}) did not answer within {} s",
                self.shell,
                self.timeout.as_secs_f32()
            ),
            ProbeOutcome::SpawnError(err) => {
                format!("your login shell ({}) could not start: {err}", self.shell)
            }
            _ => match parse(&captured.stdout) {
                Some(vars) => {
                    return LoginEnvSnapshot {
                        vars,
                        fallback: None,
                    };
                }
                None => format!(
                    "your login shell ({}) did not print its environment",
                    self.shell
                ),
            },
        };
        tracing::warn!(%why, "using the fallback login PATH");
        let mut vars: Vec<(String, String)> = self
            .base
            .iter()
            .filter(|(k, _)| k != "PATH")
            .cloned()
            .collect();
        vars.push(("PATH".into(), FALLBACK_PATH.into()));
        LoginEnvSnapshot {
            vars,
            fallback: Some(format!("{why}; using PATH={FALLBACK_PATH}")),
        }
    }
}

#[async_trait::async_trait]
impl LoginEnv for ShellLoginEnv {
    async fn snapshot(&self) -> Arc<LoginEnvSnapshot> {
        let mut rx = self.capture.subscribe();
        let ready = rx.wait_for(Option::is_some).await;
        match ready.ok().and_then(|v| v.clone()) {
            Some(snapshot) => snapshot,
            None => Arc::new(LoginEnvSnapshot::default()),
        }
    }

    async fn refresh(&self) -> Arc<LoginEnvSnapshot> {
        let _one = self.capturing.lock().await;
        let how = Arc::clone(&self.how);
        let snapshot = tokio::task::spawn_blocking(move || how.run())
            .await
            .unwrap_or_default();
        let snapshot = Arc::new(snapshot);
        self.capture.send_replace(Some(Arc::clone(&snapshot)));
        snapshot
    }

    fn current(&self) -> Option<Arc<LoginEnvSnapshot>> {
        self.capture.borrow().clone()
    }

    fn resolve(&self, env: &LoginEnvSnapshot, program: &str) -> Option<String> {
        resolve_on_path(env.get("PATH")?, program)
    }
}

/// The engine's own environment, as the login environment.
#[derive(Debug, Clone)]
pub struct InheritedEnv {
    snapshot: Arc<LoginEnvSnapshot>,
}

impl InheritedEnv {
    /// Take the engine's environment now.
    pub fn capture() -> Self {
        Self::from_vars(std::env::vars().collect())
    }

    /// A login environment of exactly these variables (for tests).
    pub fn from_vars(vars: Vec<(String, String)>) -> Self {
        Self {
            snapshot: Arc::new(LoginEnvSnapshot {
                vars,
                fallback: None,
            }),
        }
    }
}

#[async_trait::async_trait]
impl LoginEnv for InheritedEnv {
    async fn snapshot(&self) -> Arc<LoginEnvSnapshot> {
        Arc::clone(&self.snapshot)
    }

    async fn refresh(&self) -> Arc<LoginEnvSnapshot> {
        Arc::clone(&self.snapshot)
    }

    fn current(&self) -> Option<Arc<LoginEnvSnapshot>> {
        Some(Arc::clone(&self.snapshot))
    }

    fn resolve(&self, env: &LoginEnvSnapshot, program: &str) -> Option<String> {
        resolve_on_path(env.get("PATH")?, program)
    }
}

/// The first executable regular file named `program` in a `PATH` value.
/// `program` must be a bare name.
pub fn resolve_on_path(path: &str, program: &str) -> Option<String> {
    if program.is_empty() || program.contains('/') {
        return None;
    }
    path.split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(|found| found.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_should_keep_only_the_marked_variables() {
        let out = b"motd noise __SWITCHYARD_ENV_BEGIN__A=1\0PATH=/x:/y\0EQ=a=b\0junk\0__SWITCHYARD_ENV_END__ trailing";
        assert_eq!(
            parse(out),
            Some(vec![
                ("A".to_owned(), "1".to_owned()),
                ("PATH".to_owned(), "/x:/y".to_owned()),
                ("EQ".to_owned(), "a=b".to_owned()),
            ])
        );
        assert_eq!(parse(b"__SWITCHYARD_ENV_BEGIN__A=1\0"), None);
        assert_eq!(parse(b"nothing here"), None);
    }
}
