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
            is_working: AtomicBool::new(true), // optimistic default
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

    /// Whether enough time has passed to justify a fresh upstream probe.
    pub fn should_recheck(&self) -> bool {
        now_ms().saturating_sub(self.last_check_ms.load(Ordering::Relaxed))
            >= READINESS_CHECK_INTERVAL_MS
    }

    /// Record that a probe is being performed now (resets the rate limiter).
    pub fn record_check(&self) {
        self.last_check_ms.store(now_ms(), Ordering::Relaxed);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
