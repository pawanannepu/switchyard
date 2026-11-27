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
mod faults;
mod lifecycle;
mod lock;
mod logging;
mod outbox;
mod server;
mod storage;
mod wire;

use std::process::ExitCode;
use std::sync::Arc;

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

/// Log every panic's location at error, with the run whose supervision it
/// happened in (spec 0004, AC-11): a `JoinError` carries no location. The
/// payload is left out, since it may hold data the logs never keep.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let run_id = sy_core::agents::current_run_id();
        tracing::error!(
            run_id = run_id.as_deref(),
            location = location.as_deref(),
            thread = std::thread::current().name(),
            "panic"
        );
    }));
}

fn run() -> anyhow::Result<ExitCode> {
    let paths = EnginePaths::resolve()?;
    paths.ensure_dirs()?;
    let _log_guard = logging::init(&paths.logs_dir)?;
    install_panic_hook();
    if cfg!(panic = "abort") {
        tracing::error!(
            "built with panic = \"abort\": a panic in one run's supervision stops the whole engine"
        );
    }
    // Many runs and held hooks need more than the macOS default of 256 (spec 0004, AC-13).
    match sy_infra::limits::raise_nofile_limit(4096) {
        Ok(limit) => tracing::debug!(limit, "open file limit"),
        Err(err) => tracing::warn!(%err, "could not raise the open file limit"),
    }

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

    let config = config::Config::from_env();

    // Each adapter registers here: one line per agent, nothing in sy-core.
    let mut adapters = AdapterRegistry::default();
    adapters.register(Arc::new(sy_adapter_claude::ClaudeAdapter));
    #[cfg(feature = "devtools")]
    adapters.register(Arc::new(sy_adapter_fake::FakeAdapter::new(
        config.helper("sy-fake-agent"),
    )));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::run(paths, config, adapters))?;
    drop(runtime);
    // Released last, after the socket is gone, so the next engine starts clean.
    drop(lock);
    Ok(ExitCode::SUCCESS)
}
