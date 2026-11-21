//! `switchyardd`, the engine daemon. One instance per user.
//!
//! It is the only owner of agents, PTYs, the database, git operations and
//! secrets. Clients (the desktop app, later a CLI) speak JSON-RPC to it over a
//! user only local socket. See docs/specs/0001-stack-architecture/index.md.

mod config;
#[cfg(feature = "devtools")]
mod dev;
mod dispatch;
mod engine;
mod lifecycle;
mod lock;
mod logging;
mod outbox;
mod server;

use std::process::ExitCode;

use anyhow::Context;
use sy_client::EnginePaths;
use sy_core::adapters::AdapterRegistry;

/// This engine's version, reported in `session.hello` and `engine.info`.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("switchyardd {ENGINE_VERSION}");
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("engine failed: {err:#}");
            eprintln!("switchyardd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let paths = EnginePaths::resolve()?;
    paths.ensure_dirs()?;
    let _log_guard = logging::init(&paths.logs_dir)?;

    let lock = match lock::acquire(&paths.lock)
        .with_context(|| format!("opening {}", paths.lock.display()))?
    {
        lock::Outcome::Acquired(file) => file,
        lock::Outcome::AlreadyRunning => {
            // Another engine holds the lock. Leave its socket alone and exit cleanly.
            tracing::info!(lock = %paths.lock.display(), "already running");
            return Ok(ExitCode::SUCCESS);
        }
    };

    // Each adapter registers here: one line per agent, nothing in sy-core.
    let mut adapters = AdapterRegistry::default();
    adapters.register(sy_adapter_claude::ClaudeAdapter);

    let config = config::Config::from_env();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::run(paths, config, adapters))?;
    drop(runtime);
    // Released last, after the socket is gone, so the next engine starts clean.
    drop(lock);
    Ok(ExitCode::SUCCESS)
}
