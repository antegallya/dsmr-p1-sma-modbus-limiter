//! Shared health/observability state, updated by the control loop and read
//! by the HTTP health server.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct AppState {
    inner: Mutex<Inner>,
    staleness: Duration,
}

struct Inner {
    last_telegram_at: Option<Instant>,
    last_action_at: Option<Instant>,
}

impl AppState {
    pub fn new(staleness: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner { last_telegram_at: None, last_action_at: None }),
            staleness,
        }
    }

    pub fn record_telegram(&self) {
        self.inner.lock().unwrap().last_telegram_at = Some(Instant::now());
    }

    /// Records a control cycle action, either a write or a lapse.
    pub fn record_action(&self) {
        self.inner.lock().unwrap().last_action_at = Some(Instant::now());
    }

    pub fn is_healthy(&self) -> bool {
        let g = self.inner.lock().unwrap();
        let now = Instant::now();
        let telegram_fresh =
            g.last_telegram_at.is_some_and(|t| now.duration_since(t) <= self.staleness);
        // Slack, since the action follows telegram receipt.
        let action_recent = g
            .last_action_at
            .is_some_and(|t| now.duration_since(t) <= self.staleness * 3);
        telegram_fresh && action_recent
    }
}
