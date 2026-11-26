//! Operating system specific code lives here (and only here, `sy-client` and
//! `src-tauri`). It implements the ports `sy-core` defines: storage (SQLite,
//! run log files and the clock, feature 3), agent runs (PTYs, launch dirs,
//! signal tokens and the login environment, feature 4), and later the git CLI
//! (feature 8) and the keychain (feature 14).

#[cfg(unix)]
pub mod agents;
pub mod clock;
pub mod signals;
pub mod storage;
