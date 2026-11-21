//! The switchyard engine protocol: the contract every client speaks.
//!
//! JSON-RPC 2.0 over a user only local socket, one UTF-8 message per line.
//! Rust types here are the single source of truth: [`schema::generate`] emits
//! `protocol.schema.json`, and the TypeScript types in `packages/protocol-ts`
//! are generated from it.

pub mod envelope;
pub mod error;
pub mod methods;
pub mod schema;
pub mod types;

pub use envelope::{Message, RpcError, notification_line, request_line, response_line};
pub use error::ErrorCode;
pub use types::*;

/// The protocol version this build speaks. Majors must match; minors only add things.
pub const PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// Longest accepted line (one message), in bytes, not counting the newline.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Largest decoded payload accepted by `run.input`, in bytes. Clients split bigger pastes.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Most output a `run.attach` replays, in bytes (the per run ring buffer size).
pub const MAX_REPLAY_BYTES: usize = 2 * 1024 * 1024;

/// Per client outgoing queue cap in the engine, in bytes.
pub const CLIENT_QUEUE_BYTES: usize = 8 * 1024 * 1024;
