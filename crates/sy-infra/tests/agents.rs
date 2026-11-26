#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on setup failure"
)]

//! The agent run ports against real processes, PTYs and files (spec 0003).

use std::io::Read;
use std::os::unix::fs::{MetadataExt, symlink};
use std::sync::mpsc;
use std::time::Duration;

use sy_adapter_api::{ExitInfo, PtySize};
use sy_core::agents::ports::{
    GroupSignal, LoginEnv, Pty, PtyChild, RuntimeDir, SpawnRequest, TokenSource,
};
use sy_core::ids::RunId;
use sy_infra::agents::{FsRuntimeDir, InheritedEnv, PortablePty, RandomTokens, resolve_on_path};

fn spawn(program: &str, args: &[&str], env: &[(&str, &str)]) -> PtyChild {
    PortablePty
        .spawn(SpawnRequest {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            cwd: "/tmp".into(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            size: PtySize { cols: 80, rows: 24 },
        })
        .unwrap()
}

/// Read the terminal on a thread until EOF (or 5 s), returning everything.
fn read_all(child: &mut PtyChild) -> mpsc::Receiver<Vec<u8>> {
    let mut reader = std::mem::replace(&mut child.reader, Box::new(std::io::empty()));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0_u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        let _ = tx.send(all);
    });
    rx
}

fn text(rx: &mpsc::Receiver<Vec<u8>>) -> String {
    String::from_utf8_lossy(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).into_owned()
}

#[test]
fn a_child_should_print_to_the_terminal_and_report_its_exit_code() {
    let mut child = spawn("/bin/sh", &["-c", "printf hello; exit 3"], &[]);
    let out = read_all(&mut child);
    let exit = child.process.wait().unwrap();
    assert_eq!(
        (exit, text(&out)),
        (
            ExitInfo {
                code: Some(3),
                signal: None
            },
            "hello".to_owned()
        )
    );
}

#[test]
fn a_death_by_signal_should_report_the_signal_number_and_no_code() {
    let child = spawn("/bin/sh", &["-c", "kill -SEGV $$"], &[]);
    assert_eq!(
        child.process.wait().unwrap(),
        ExitInfo {
            code: None,
            signal: Some(11)
        }
    );
}

#[test]
fn a_child_should_lead_its_own_process_group_and_group_signals_should_stop_after_the_reap() {
    let child = spawn("/bin/sleep", &["30"], &[]);
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.pid).unwrap()).unwrap();
    let pgid = rustix::process::getpgid(Some(pid)).unwrap();
    let sent = child.process.signal_group(GroupSignal::Kill);
    let exit = child.process.wait().unwrap();
    let after = child.process.signal_group(GroupSignal::Hangup);
    assert_eq!(
        (
            child.pgid,
            i64::from(pgid.as_raw_nonzero().get()),
            sent,
            exit.signal,
            after
        ),
        (child.pid, child.pid, true, Some(9), false)
    );
}

#[test]
fn input_should_reach_the_child_through_the_terminal() {
    let mut child = spawn("/bin/sh", &["-c", "read x; echo got:$x"], &[]);
    let out = read_all(&mut child);
    child.io.write_all(b"abc\n").unwrap();
    child.process.wait().unwrap();
    assert!(text(&out).contains("got:abc"));
}

#[test]
fn the_child_should_get_exactly_the_environment_it_was_given() {
    let mut child = spawn(
        "/usr/bin/env",
        &[],
        &[
            ("PATH", "/usr/bin:/bin"),
            ("SY_TEST", "1"),
            ("SHELL", "/bin/sh"),
        ],
    );
    let out = read_all(&mut child);
    child.process.wait().unwrap();
    let mut lines: Vec<String> = text(&out)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_owned())
        .collect();
    lines.sort();
    assert_eq!(lines, ["PATH=/usr/bin:/bin", "SHELL=/bin/sh", "SY_TEST=1"]);
}

#[test]
fn a_missing_working_directory_should_refuse_the_spawn() {
    let err = PortablePty
        .spawn(SpawnRequest {
            program: "/bin/sh".into(),
            args: vec![],
            cwd: "/nonexistent/worktree".into(),
            env: vec![],
            size: PtySize { cols: 80, rows: 24 },
        })
        .err()
        .unwrap();
    assert!(err.0.contains("working directory"));
}

#[test]
fn launch_dirs_should_be_private_and_files_created_new() {
    let data = tempfile::tempdir().unwrap();
    let dirs = FsRuntimeDir::new(data.path());
    let run = RunId::new();
    let path = dirs.create(&run).unwrap();
    dirs.write_file(&run, "hooks/settings.json", b"{}").unwrap();
    let again = dirs.write_file(&run, "hooks/settings.json", b"x");
    symlink("/etc/hosts", format!("{path}/link")).unwrap();
    let through_link = dirs.write_file(&run, "link", b"x");
    let mode = |p: &str| std::fs::metadata(p).unwrap().mode() & 0o777;
    assert_eq!(
        (
            mode(&path),
            mode(&format!("{path}/hooks")),
            mode(&format!("{path}/hooks/settings.json")),
            std::fs::read(format!("{path}/hooks/settings.json")).unwrap(),
            again.is_err(),
            through_link.is_err(),
        ),
        (0o700, 0o700, 0o600, b"{}".to_vec(), true, true)
    );
    dirs.remove(&run).unwrap();
    dirs.remove(&run).unwrap();
    assert!(!std::path::Path::new(&path).exists());
}

// covers: AC-13 (each run gets a fresh private dir, never a stale one)
#[test]
fn a_launch_dir_that_already_exists_should_not_be_reused() {
    let data = tempfile::tempdir().unwrap();
    let dirs = FsRuntimeDir::new(data.path());
    let run = RunId::new();
    dirs.ensure_root().unwrap();
    std::fs::create_dir(data.path().join("agent-runtime").join(run.as_str())).unwrap();
    let mode = std::fs::metadata(dirs.root()).unwrap().mode() & 0o777;
    assert_eq!(
        (
            mode,
            dirs.create(&run).map_err(|e| e.kind()),
            dirs.path(&run)
        ),
        (
            0o700,
            Err(std::io::ErrorKind::AlreadyExists),
            format!("{}/{run}", dirs.root()),
        )
    );
}

#[test]
fn sweep_should_remove_every_launch_dir() {
    let data = tempfile::tempdir().unwrap();
    let dirs = FsRuntimeDir::new(data.path());
    let before = dirs.sweep().unwrap();
    dirs.create(&RunId::new()).unwrap();
    dirs.create(&RunId::new()).unwrap();
    let swept = dirs.sweep().unwrap();
    let left = std::fs::read_dir(data.path().join("agent-runtime"))
        .unwrap()
        .count();
    assert_eq!((before, swept, left), (0, 2, 0));
}

#[test]
fn tokens_should_be_128_bit_lowercase_hex_and_unique() {
    let tokens = RandomTokens::default();
    let (a, b) = (tokens.mint(), tokens.mint());
    assert!(
        a.len() == 32
            && a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            && a != b
    );
}

#[tokio::test]
async fn resolve_should_find_the_first_executable_on_path_only() {
    let env = InheritedEnv::from_vars(vec![("PATH".into(), "/nonexistent:/bin:/usr/bin".into())]);
    let snapshot = env.snapshot().await;
    assert_eq!(
        (
            env.resolve(&snapshot, "sh"),
            env.resolve(&snapshot, "no-such-program-here"),
            env.resolve(&snapshot, "/bin/sh"),
            resolve_on_path("relative:/bin", "sh"),
        ),
        (
            Some("/bin/sh".to_owned()),
            None,
            None,
            Some("/bin/sh".to_owned())
        )
    );
}

// ---- probes and the login shell (spec 0003, milestone 4) ----

mod probes_and_login {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use sy_adapter_api::{ExitInfo, ProbeOutcome};
    use sy_core::agents::ports::{LoginEnv, PROBE_OUTPUT_BYTES, ProbeRequest, ProcessRunner};
    use sy_infra::agents::{FALLBACK_PATH, ShellLoginEnv, SystemProcessRunner};

    fn sh(script: &str, timeout: Duration) -> sy_core::agents::ports::ProbeRun {
        SystemProcessRunner.run(ProbeRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: "/tmp".into(),
            env: vec![("ONLY".into(), "this".into())],
            timeout,
        })
    }

    #[test]
    fn a_probe_should_report_its_exit_and_capped_output_with_exactly_its_env() {
        let run = sh(
            "echo out; echo err >&2; env; exit 3",
            Duration::from_secs(5),
        );
        let big = sh("head -c 200000 /dev/zero", Duration::from_secs(5));
        let stdout = String::from_utf8(run.stdout).unwrap();
        assert_eq!(
            (
                run.outcome,
                stdout.lines().next(),
                stdout.contains("ONLY=this"),
                stdout.contains("HOME="),
                run.stderr,
                big.stdout.len(),
            ),
            (
                ProbeOutcome::Exited(ExitInfo {
                    code: Some(3),
                    signal: None
                }),
                Some("out"),
                true,
                false,
                b"err\n".to_vec(),
                PROBE_OUTPUT_BYTES,
            )
        );
    }

    #[test]
    fn a_probe_past_its_timeout_should_be_killed_with_its_children() {
        let started = Instant::now();
        // The child holding the pipe open must not keep the probe waiting.
        let run = sh("sleep 30 & sleep 30", Duration::from_millis(300));
        assert_eq!(run.outcome, ProbeOutcome::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_missing_program_should_be_a_spawn_error() {
        let run = SystemProcessRunner.run(ProbeRequest {
            program: "/nonexistent/tool".into(),
            args: Vec::new(),
            cwd: "/tmp".into(),
            env: Vec::new(),
            timeout: Duration::from_secs(1),
        });
        assert!(
            matches!(run.outcome, ProbeOutcome::SpawnError(_)),
            "{run:?}"
        );
    }

    /// A stand in login shell: `$4` is the script after `-l -i -c`.
    fn shell(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn base() -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), "/tmp".into()),
        ]
    }

    #[tokio::test]
    async fn the_login_shell_should_give_only_the_marked_environment() {
        let dir = tempfile::tempdir().unwrap();
        let noisy = shell(
            &dir,
            "noisy",
            "echo 'welcome to your shell'\nprintf 'X=noise\\0'\nexport FROM_PROFILE=yes\nexport PATH=/opt/tools/bin:$PATH\neval \"$4\"\necho bye",
        );
        let env = ShellLoginEnv::with(Some(noisy), base(), Duration::from_secs(5));
        let before = env.current();
        let snapshot = env.refresh().await;
        assert_eq!(
            (
                before.is_none(),
                snapshot.fallback.clone(),
                snapshot.get("FROM_PROFILE"),
                snapshot.get("PATH"),
                snapshot.get("X"),
                env.current().is_some(),
            ),
            (
                true,
                None,
                Some("yes"),
                Some("/opt/tools/bin:/usr/bin:/bin"),
                None,
                true
            )
        );
    }

    #[tokio::test]
    async fn a_hanging_or_silent_shell_should_fall_back_and_say_why() {
        let dir = tempfile::tempdir().unwrap();
        let hangs = shell(&dir, "hangs", "sleep 30");
        let silent = shell(&dir, "silent", "echo nothing useful; exit 1");
        let started = Instant::now();
        let hung = ShellLoginEnv::with(Some(hangs), base(), Duration::from_millis(300))
            .refresh()
            .await;
        let took = started.elapsed();
        let quiet = ShellLoginEnv::with(Some(silent), base(), Duration::from_secs(5))
            .refresh()
            .await;
        for snapshot in [&hung, &quiet] {
            assert_eq!(
                (snapshot.get("PATH"), snapshot.get("HOME")),
                (Some(FALLBACK_PATH), Some("/tmp"))
            );
        }
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(
            hung.fallback.as_deref().unwrap().contains("did not answer"),
            "{:?}",
            hung.fallback
        );
        assert!(
            quiet.fallback.as_deref().unwrap().contains("did not print"),
            "{:?}",
            quiet.fallback
        );
    }

    #[tokio::test]
    async fn snapshot_should_wait_for_the_first_capture() {
        let dir = tempfile::tempdir().unwrap();
        let slow = shell(&dir, "slow", "sleep 0.3\neval \"$4\"");
        let env = ShellLoginEnv::with(Some(slow), base(), Duration::from_secs(5));
        let waiting = {
            let env = std::sync::Arc::clone(&env);
            tokio::spawn(async move { env.snapshot().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        env.refresh().await;
        let snapshot = waiting.await.unwrap();
        assert_eq!(snapshot.get("HOME"), Some("/tmp"));
    }
}
