//! Lightweight backend-health tracking for the readiness probe.
//!
//! Mirrors the reference implementation's `backendState`: an optimistic
//! "working" flag plus a rate limiter so a failing backend isn't re-probed on
//! every `/ready` call.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// How long to wait between upstream re-checks while the backend looks down.
const READINESS_CHECK_INTERVAL_MS: u64 = 30_000;

#[derive(Debug)]
pub struct BackendHealth {
    is_working: AtomicBool,
    last_check_ms: AtomicU64,
}

impl Default for BackendHealth {
    fn default() -> Self {
        Self {
            is_working: AtomicBool::new(false), // optimistic default
            last_check_ms: AtomicU64::new(0),
        }
    }
}

impl BackendHealth {
    pub fn mark_working(&self) {
        self.is_working.store(true, Ordering::Relaxed);
    }

    pub fn mark_failed(&self) {
        self.is_working.store(false, Ordering::Relaxed);
    }

    pub fn is_working(&self) -> bool {
        self.is_working.load(Ordering::Relaxed)
    }

    /// Claim the next probe atomically so concurrent callers cannot duplicate it.
    pub fn begin_check(&self) -> bool {
        let now = now_ms();
        self.last_check_ms
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                (now.saturating_sub(last) >= READINESS_CHECK_INTERVAL_MS).then_some(now)
            })
            .is_ok()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn starts_unready_and_claims_only_one_probe() {
        let health = BackendHealth::default();
        assert!(!health.is_working());
        assert!(health.begin_check());
        assert!(!health.begin_check());
    }
}
