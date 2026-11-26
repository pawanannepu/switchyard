//! Signal tokens: 16 bytes from the OS random source, lowercase hex.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sy_core::agents::ports::TokenSource;

/// Mints tokens with `getrandom`.
#[derive(Debug, Default)]
pub struct RandomTokens {
    fallback: AtomicU64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl TokenSource for RandomTokens {
    fn mint(&self) -> String {
        let mut bytes = [0_u8; 16];
        if let Err(err) = getrandom::fill(&mut bytes) {
            // Tokens are attribution within one user, not a security wall
            // (spec 0003, *Security model*), so a unique value still works.
            tracing::error!(%err, "no OS randomness for a signal token; using a unique fallback");
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let count = self.fallback.fetch_add(1, Ordering::Relaxed);
            bytes[..8].copy_from_slice(&nanos.to_le_bytes()[..8]);
            bytes[8..].copy_from_slice(&count.to_le_bytes());
        }
        hex(&bytes)
    }
}
