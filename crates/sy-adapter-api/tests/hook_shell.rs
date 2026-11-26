#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! `hook_command`'s shell string through a real POSIX shell (spec 0003, AC-1):
//! an agent that runs its hook through `sh -c` must hand `sy-signal` exactly
//! the argv the array form gives. Lives in `tests/` because the contract's
//! own source may not start a process.

use std::process::Command;

use sy_adapter_api::{HookReply, SignalEndpoint, hook_command, shell_quote};

/// The words `sh` splits `script` into, run as `printf '%s\0' <script>`.
fn words(script: &str) -> Vec<String> {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("printf '%s\\0' {script}")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut words: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .split('\0')
        .map(str::to_owned)
        .collect();
    // printf ends every word with NUL, so the split leaves one empty tail.
    assert_eq!(words.pop().as_deref(), Some(""));
    words
}

// covers: AC-1 (`hook_command` quotes spaces, quotes, `$` and newlines)
#[test]
fn the_shell_string_should_split_back_into_exactly_the_argv() {
    let cmd = hook_command(
        &SignalEndpoint {
            helper: "/Users/me/My Apps/it's $HOME/sy-signal".to_owned(),
            socket: "/tmp/sy-a/engine.sock".to_owned(),
            token: "`whoami`".to_owned(),
        },
        "pre tool\nuse",
        &HookReply {
            stdout: br#"{"decision":"ask"}"#.to_vec(),
            exit_code: 2,
        },
    );
    assert_eq!(words(&cmd.shell), cmd.argv);
}

#[test]
fn shell_quote_should_keep_awkward_arguments_whole() {
    let awkward = [
        "",
        "it's",
        "''",
        "$(rm -rf /)",
        "a\\b",
        "tab\there",
        "line\nbreak",
        "* ? [x]",
        "émoji 🚀",
    ];
    let script = awkward
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(words(&script), awkward);
}
