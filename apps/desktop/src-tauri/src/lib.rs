//! The app host: a thin client with no business logic.
//!
//! It owns the window, the menu bar item and (later) OS notifications, starts
//! the engine when it isn't running, holds one engine connection, and relays
//! frames between the webview and the engine. Closing the window only hides
//! it; the engine and its agents keep running.

mod engine;
mod launcher;
mod logging;
mod quit;
mod tray;

use std::sync::Arc;

use sy_client::EnginePaths;
use tauri::{Manager, RunEvent, WindowEvent};

pub const MAIN_WINDOW: &str = "main";

#[derive(Debug)]
pub enum RunError {
    Paths(sy_client::paths::PathError),
    Tauri(tauri::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Paths(e) => write!(f, "{e}"),
            RunError::Tauri(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Show and focus the main window (from the menu bar item or the Dock).
pub fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), RunError> {
    let paths = EnginePaths::resolve().map_err(RunError::Paths)?;
    paths.ensure_dirs().map_err(RunError::Paths)?;
    let _log_guard = logging::init(&paths.logs_dir);
    let link = Arc::new(engine::EngineLink::new(paths));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::clone(&link))
        .invoke_handler(tauri::generate_handler![engine::engine_subscribe, engine::engine_send, engine::engine_restart])
        .setup(move |app| {
            tray::create(app)?;
            link.connect(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Closing the window never stops the engine or its agents.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .map_err(RunError::Tauri)?;

    app.run(|app, event| match event {
        RunEvent::ExitRequested { api, .. } => quit::on_exit_requested(app, &api),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main_window(app),
        _ => {}
    });
    Ok(())
}
