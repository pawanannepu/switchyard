//! The `sy-signal` invocation an agent's hook config runs.

use crate::b64;
use crate::contract::{HookReply, SignalEndpoint};

/// One hook's command, as an argument array and as a shell string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    /// For hook configs that take an argument array. The agent's own
    /// arguments go after the trailing `--`.
    pub argv: Vec<String>,
    /// For hook configs that take a command string the agent runs through a
    /// POSIX shell: every argument single quoted.
    pub shell: String,
}

/// The `sy-signal` invocation for `hook`. If the helper can't reach the
/// engine it writes `fallback.stdout` and exits with `fallback.exit_code`,
/// so pick a fallback that leaves the agent on its own prompt, never an allow.
///
/// The argv is `[helper, "--socket", socket, "--token", token, "--hook", hook,
/// "--fallback-exit", n, "--fallback-stdout-b64", b64, "--"]`.
pub fn hook_command(endpoint: &SignalEndpoint, hook: &str, fallback: &HookReply) -> HookCommand {
    let argv = vec![
        endpoint.helper.clone(),
        "--socket".to_owned(),
        endpoint.socket.clone(),
        "--token".to_owned(),
        endpoint.token.clone(),
        "--hook".to_owned(),
        hook.to_owned(),
        "--fallback-exit".to_owned(),
        fallback.exit_code.to_string(),
        "--fallback-stdout-b64".to_owned(),
        b64::encode(&fallback.stdout),
        "--".to_owned(),
    ];
    let shell = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    HookCommand { argv, shell }
}

/// `arg` as one POSIX shell word: single quoted, with each `'` written as
/// `'\''`. Safe for spaces, quotes, `$`, backslashes and newlines.
pub fn shell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(helper: &str) -> SignalEndpoint {
        SignalEndpoint {
            helper: helper.to_owned(),
            socket: "/tmp/sy-a/engine.sock".to_owned(),
            token: "00ff".to_owned(),
        }
    }

    #[test]
    fn hook_command_should_build_the_documented_argv() {
        let cmd = hook_command(
            &endpoint("/opt/sy-signal"),
            "status",
            &HookReply {
                stdout: b"foo".to_vec(),
                exit_code: 2,
            },
        );
        assert_eq!(
            cmd.argv,
            [
                "/opt/sy-signal",
                "--socket",
                "/tmp/sy-a/engine.sock",
                "--token",
                "00ff",
                "--hook",
                "status",
                "--fallback-exit",
                "2",
                "--fallback-stdout-b64",
                "Zm9v",
                "--",
            ]
        );
    }

    #[test]
    fn shell_string_should_quote_every_argument() {
        let cmd = hook_command(&endpoint("/a b/sy-signal"), "x", &HookReply::default());
        assert!(cmd.shell.starts_with("'/a b/sy-signal' '--socket' "));
        assert!(cmd.shell.ends_with(" '--fallback-stdout-b64' '' '--'"));
    }

    #[test]
    fn shell_quote_should_survive_quotes_dollars_and_newlines() {
        assert_eq!(
            [
                shell_quote("it's"),
                shell_quote("$HOME `x`"),
                shell_quote("a\nb"),
                shell_quote(r"back\slash"),
            ],
            [
                r"'it'\''s'".to_owned(),
                "'$HOME `x`'".to_owned(),
                "'a\nb'".to_owned(),
                r"'back\slash'".to_owned(),
            ]
        );
    }
}
