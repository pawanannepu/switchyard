fn main() {
    // Declaring the app's commands turns on ACL checks for them, so the webview
    // can call only what capabilities/default.json grants: the three relay commands.
    let manifest = tauri_build::AppManifest::new().commands(&["engine_subscribe", "engine_send", "engine_restart"]);
    if let Err(err) = tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest)) {
        println!("cargo:warning=tauri build script failed: {err:#}");
        std::process::exit(1);
    }
}
