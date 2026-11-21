//! Quit asks first when agents are running. Quitting closes the app only:
//! the engine keeps its agents running and exits on its own once idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, ExitRequestApi, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::engine::EngineLink;

static CONFIRMED: AtomicBool = AtomicBool::new(false);

/// Cmd+Q, the app menu, or `app.exit`. Lets a confirmed quit through.
pub fn on_exit_requested(app: &AppHandle, api: &ExitRequestApi) {
    if CONFIRMED.load(Ordering::Acquire) {
        return;
    }
    api.prevent_exit();
    request(app);
}

/// Quit, asking first if agents are running.
pub fn request(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let running = match app.try_state::<Arc<EngineLink>>() {
            Some(link) => link.refresh_running_agents().await,
            None => 0,
        };
        if running == 0 {
            quit_now(&app);
            return;
        }
        let agents = if running == 1 { "1 agent is".to_owned() } else { format!("{running} agents are") };
        let app2 = app.clone();
        app.dialog()
            .message(format!(
                "{agents} still running. They keep running in the background engine after you quit, and you can reopen switchyard to check on them."
            ))
            .title("Quit switchyard?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Quit".into(), "Cancel".into()))
            .show(move |quit| {
                if quit {
                    quit_now(&app2);
                }
            });
    });
}

fn quit_now(app: &AppHandle) {
    CONFIRMED.store(true, Ordering::Release);
    app.exit(0);
}
