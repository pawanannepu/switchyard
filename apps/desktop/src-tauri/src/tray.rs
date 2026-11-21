//! The menu bar item: engine state and running count, open the window, quit.
//! Attention counts join once approvals exist (feature 12).

use sy_protocol::{HostConnection, HostConnectionState};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{App, AppHandle, Manager, Wry};

const TRAY_ID: &str = "main";
const OPEN: &str = "open";
const QUIT: &str = "quit";

struct StatusItem(MenuItem<Wry>);

pub fn create(app: &mut App) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Engine: starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, OPEN, "Open switchyard", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit switchyard", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &PredefinedMenuItem::separator(app)?, &open, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("switchyard")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            OPEN => crate::show_main_window(app),
            QUIT => crate::quit::request(app),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    app.manage(StatusItem(status));
    Ok(())
}

fn describe(status: &HostConnection, running: u32) -> String {
    match status.state {
        HostConnectionState::Connecting => "Engine: starting…".into(),
        HostConnectionState::Connected => match running {
            0 => "Engine running · no agents".into(),
            1 => "Engine running · 1 agent".into(),
            n => format!("Engine running · {n} agents"),
        },
        HostConnectionState::Stopped => "Engine stopped".into(),
        HostConnectionState::Failed => "Engine didn't start".into(),
        HostConnectionState::Incompatible => "Engine needs a restart".into(),
    }
}

pub fn show_status(app: &AppHandle, status: &HostConnection, running: u32) {
    let text = describe(status, running);
    if let Some(item) = app.try_state::<StatusItem>() {
        let _ = item.0.set_text(&text);
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("switchyard · {text}")));
    }
}
