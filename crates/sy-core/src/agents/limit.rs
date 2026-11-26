//! The per run hook rate limit (spec 0003, AC-7): a token bucket on the
//! engine's clock, charged only for hook calls answered at once.

/// Hook calls a run may have answered at once per second, once its burst is used.
pub const SIGNAL_RATE_PER_SEC: u64 = 100;
/// Hook calls a run may have answered at once in a burst.
pub const SIGNAL_BURST: u64 = 500;

/// One call, in thousandths, so a refill of 100 per second is a whole number per ms.
const MILLI: u64 = 1000;

/// A token bucket that starts full.
#[derive(Debug, Clone)]
pub(super) struct RateLimit {
    /// Thousandths of a call available.
    available: u64,
    /// The clock reading of the last refill.
    refilled_at: Option<i64>,
}

impl RateLimit {
    pub(super) const fn new() -> Self {
        Self {
            available: SIGNAL_BURST * MILLI,
            refilled_at: None,
        }
    }

    /// Charge one call at `now_ms`. False when the bucket is empty (the call
    /// is refused and costs nothing).
    pub(super) fn take(&mut self, now_ms: i64) -> bool {
        if let Some(at) = self.refilled_at {
            let elapsed = u64::try_from(now_ms.saturating_sub(at)).unwrap_or(0);
            // 100 calls per second is 100 thousandths per millisecond.
            let refill = elapsed.saturating_mul(SIGNAL_RATE_PER_SEC);
            self.available = self
                .available
                .saturating_add(refill)
                .min(SIGNAL_BURST * MILLI);
        }
        // A clock that steps back never refills, and never moves the mark back.
        self.refilled_at = Some(self.refilled_at.map_or(now_ms, |at| at.max(now_ms)));
        if self.available < MILLI {
            return false;
        }
        self.available -= MILLI;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_500_should_pass_then_refuse() {
        let mut limit = RateLimit::new();
        let passed = (0..600).filter(|_| limit.take(1_000)).count();
        assert_eq!(passed, 500);
    }

    #[test]
    fn it_should_refill_100_per_second_up_to_the_burst() {
        let mut limit = RateLimit::new();
        while limit.take(0) {}
        // 10 ms is one call; a second is 100; a minute is capped at 500.
        let after = |limit: &mut RateLimit, at: i64| (0..1000).filter(|_| limit.take(at)).count();
        assert_eq!(
            (
                after(&mut limit, 10),
                after(&mut limit, 1_010),
                after(&mut limit, 61_010)
            ),
            (1, 100, 500)
        );
    }

    #[test]
    fn a_clock_stepping_back_should_not_refill() {
        let mut limit = RateLimit::new();
        while limit.take(5_000) {}
        assert!(!limit.take(1_000));
        assert!(!limit.take(5_000));
        assert!(limit.take(5_010));
    }
}
