//! Process signals that should stop the engine cleanly.

/// Resolves on SIGTERM (for example at logout) or SIGINT (Ctrl+C when run by hand).
#[cfg(unix)]
pub async fn terminate() -> std::io::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => Ok("SIGTERM"),
        _ = int.recv() => Ok("SIGINT"),
    }
}

#[cfg(not(unix))]
pub async fn terminate() -> std::io::Result<&'static str> {
    tokio::signal::ctrl_c().await?;
    Ok("Ctrl+C")
}
