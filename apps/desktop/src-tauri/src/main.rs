// Prevents an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = switchyard_lib::run() {
        eprintln!("switchyard: {err}");
        std::process::exit(1);
    }
}
