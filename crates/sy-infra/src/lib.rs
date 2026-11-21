//! Operating system specific code lives here (and only here, `sy-client` and
//! `src-tauri`). It implements the ports `sy-core` defines: SQLite (feature 3),
//! the git CLI (feature 8), PTYs (feature 5), the keychain (feature 14) and the
//! login environment. The scaffold only needs process signals.

pub mod signals;
