//! The local socket: binding, permissions, the peer check, and line framing.
//!
//! Unix domain socket today. Windows gets a named pipe behind this same module later.

use std::io;
use std::path::Path;

use sy_protocol::MAX_LINE_BYTES;
pub use tokio_util::codec::LinesCodecError;
use tokio_util::codec::{FramedRead, LinesCodec};

#[cfg(unix)]
pub use tokio::net::{UnixListener as Listener, UnixStream as Stream};

/// Reads newline delimited messages, closing on any line over 1 MiB.
pub type LineReader<R> = FramedRead<R, LinesCodec>;

pub fn line_reader<R: tokio::io::AsyncRead>(read: R) -> LineReader<R> {
    FramedRead::new(read, LinesCodec::new_with_max_length(MAX_LINE_BYTES))
}

/// The effective user id of this process.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A connection from another user was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("peer uid {peer} is not the engine's uid {own}")]
pub struct PeerRejected {
    pub peer: u32,
    pub own: u32,
}

/// Only the engine's own user may connect.
pub fn check_peer(peer_uid: u32, own_uid: u32) -> Result<(), PeerRejected> {
    if peer_uid == own_uid { Ok(()) } else { Err(PeerRejected { peer: peer_uid, own: own_uid }) }
}

/// The connecting process's user id (from the kernel, `getpeereid` on macOS).
#[cfg(unix)]
pub fn peer_uid(stream: &Stream) -> io::Result<u32> {
    Ok(stream.peer_cred()?.uid())
}

/// Bind the engine socket and make it `0600`. The caller must hold the engine lock
/// and have removed any stale socket file first.
#[cfg(unix)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let listener = Listener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(unix)]
pub async fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_peer_should_accept_same_uid() {
        assert_eq!(check_peer(501, 501), Ok(()));
    }

    #[test]
    fn check_peer_should_reject_a_different_uid() {
        assert_eq!(check_peer(502, 501), Err(PeerRejected { peer: 502, own: 501 }));
    }

    #[test]
    fn check_peer_should_reject_root_connecting_to_a_user_engine() {
        assert!(check_peer(0, 501).is_err());
    }
}
