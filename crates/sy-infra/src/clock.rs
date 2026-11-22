//! The wall clock behind sy-core's `Clock` port.

use std::time::{SystemTime, UNIX_EPOCH};

use sy_core::ports::Clock;

/// The system clock, in Unix milliseconds UTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        // A clock before 1970 reads as 0 rather than failing a write.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}
