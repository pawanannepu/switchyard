//! Everything a client of the engine needs, and the parts of the transport the
//! engine shares with its clients:
//!
//! - [`paths`]: the one data dir and socket path resolver. The engine, the app
//!   host and the dev bridge all call it, so they never disagree.
//! - [`transport`]: the Unix socket, its permissions, the peer check and framing.
//! - [`connection`]: a client connection with request/response correlation.
//! - [`relay`]: forwards UI frames to the engine and back, rewriting ids.

pub mod connection;
pub mod paths;
pub mod relay;
pub mod transport;

pub use connection::{CallError, Connection, HelloError};
pub use paths::EnginePaths;
