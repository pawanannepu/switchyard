//! The Claude Code adapter. Detection now (spec 0003, AC-15); launches arrive
//! in feature 8 against the same contract.

use sy_adapter_api::{
    AdapterError, AgentAdapter, Capabilities, Detection, Launch, LaunchRequest, Probe,
    ProbeOutcome, ProbeResult, SignedIn, StatusSource,
};

const PROGRAM: &str = "claude";
const NO_ANSWER: &str = "claude --version did not answer";

/// Drives Claude Code through its official CLI. Never reads its login tokens:
/// detection asks the CLI and nothing else.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &str {
        "claude"
    }

    fn display_name(&self) -> &str {
        "Claude Code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            launch: false,
            approvals: false,
            resume: false,
            events: false,
            status_source: StatusSource::Hooks,
        }
    }

    fn programs(&self) -> Vec<String> {
        vec![PROGRAM.to_owned()]
    }

    fn detection_probes(&self) -> Vec<Probe> {
        vec![Probe {
            program: PROGRAM.to_owned(),
            args: vec!["--version".to_owned()],
            timeout_ms: 5000,
        }]
    }

    fn interpret_detection(&self, results: &[ProbeResult]) -> Detection {
        let missing = Detection {
            installed: false,
            signed_in: SignedIn::Unknown,
            version: None,
            hint: None,
        };
        let Some(first) = results.first() else {
            return missing;
        };
        match &first.outcome {
            ProbeOutcome::NotFound | ProbeOutcome::Refused => missing,
            ProbeOutcome::Exited(exit) if exit.code == Some(0) => Detection {
                installed: true,
                signed_in: SignedIn::Unknown,
                version: String::from_utf8_lossy(&first.stdout)
                    .split_whitespace()
                    .next()
                    .map(str::to_owned),
                hint: None,
            },
            ProbeOutcome::Exited(_) | ProbeOutcome::TimedOut | ProbeOutcome::SpawnError(_) => {
                Detection {
                    installed: true,
                    signed_in: SignedIn::Unknown,
                    version: None,
                    hint: Some(NO_ANSWER.to_owned()),
                }
            }
        }
    }

    fn launch(&self, _req: &LaunchRequest) -> Result<Launch, AdapterError> {
        Err(AdapterError::Unsupported(
            "Claude Code launches arrive in feature 8".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use sy_adapter_api::ExitInfo;

    use super::*;

    fn result(outcome: ProbeOutcome, stdout: &str) -> ProbeResult {
        ProbeResult {
            probe: ClaudeAdapter.detection_probes().remove(0),
            outcome,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn exited(code: i32) -> ProbeOutcome {
        ProbeOutcome::Exited(ExitInfo {
            code: Some(code),
            signal: None,
        })
    }

    #[test]
    fn a_version_answer_should_be_installed_with_its_first_word() {
        let d = ClaudeAdapter.interpret_detection(&[result(exited(0), "2.1.325 (Claude Code)\n")]);
        assert_eq!(
            d,
            Detection {
                installed: true,
                signed_in: SignedIn::Unknown,
                version: Some("2.1.325".to_owned()),
                hint: None
            }
        );
    }

    #[test]
    fn no_answer_should_be_installed_with_a_hint_and_no_version() {
        let all = [exited(1), ProbeOutcome::TimedOut].map(|o| {
            let d = ClaudeAdapter.interpret_detection(&[result(o, "")]);
            (d.installed, d.version, d.hint)
        });
        let want = (true, None, Some(NO_ANSWER.to_owned()));
        assert_eq!(all, [want.clone(), want]);
    }

    #[test]
    fn missing_or_refused_should_not_be_installed() {
        let all = [ProbeOutcome::NotFound, ProbeOutcome::Refused].map(|o| {
            ClaudeAdapter
                .interpret_detection(&[result(o, "")])
                .installed
        });
        assert_eq!(all, [false, false]);
    }

    #[test]
    fn launch_should_be_unsupported_until_feature_8() {
        assert!(!ClaudeAdapter.capabilities().launch);
    }
}
