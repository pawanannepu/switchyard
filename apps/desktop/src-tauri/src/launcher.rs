//! Starts `switchyardd` detached from the app, so it outlives it.

use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sy_client::EnginePaths;

/// The engine binary sits next to the app's own executable: `Contents/MacOS/`
/// in the bundle (a Tauri `externalBin`), `target/<profile>/` in development.
pub fn engine_binary() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("app executable has no parent directory"))?;
    let bin = dir.join(if cfg!(windows) {
        "switchyardd.exe"
    } else {
        "switchyardd"
    });
    if bin.is_file() {
        Ok(bin)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("engine binary not found at {}", bin.display()),
        ))
    }
}

/// Spawn the engine in a new session, with stdout and stderr going to
/// `engine.stderr.log`. Safe to repeat: if another engine wins the lock, this
/// one exits 0 and the caller simply connects to the winner.
pub fn spawn(paths: &EnginePaths) -> io::Result<()> {
    let bin = engine_binary()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("engine.stderr.log"))?;
    let mut cmd = Command::new(&bin);
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut cmd);
    let mut child = cmd.spawn()?;
    tracing::info!(bin = %bin.display(), pid = child.id(), "spawned engine");
    // Reap it whenever it exits; its exit code doesn't matter here.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    #[expect(
        unsafe_code,
        reason = "pre_exec is unsafe; setsid is async signal safe, as the hook requires"
    )]
    // SAFETY: the hook only calls setsid(2), which is async signal safe and
    // touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(io::Error::from)
        });
    }
}

#[cfg(not(unix))]
fn detach(_cmd: &mut Command) {}
