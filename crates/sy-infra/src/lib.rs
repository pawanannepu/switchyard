//! Operating system specific code lives here (and only here, `sy-client` and
//! `src-tauri`). It implements the ports `sy-core` defines: storage (SQLite,
//! run log files and the clock, feature 3), and later the git CLI (feature 8),
//! PTYs (feature 5), the keychain (feature 14) and the login environment.

pub mod clock;
pub mod signals;
pub mod storage;
