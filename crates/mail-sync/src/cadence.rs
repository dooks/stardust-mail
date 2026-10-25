//! Polling cadence by window state (PLAN §1 Freshness).

use std::time::Duration;

use mail_model::WindowState;

/// Poll intervals. Focused: 5 s for the active account, 15 s for the others; visible but unfocused: 30 s;
/// minimized: 60 s. Each wait gets +-`jitter` (20%) so accounts do not poll in lockstep. The service also polls
/// immediately on focus / restore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cadence {
    pub focused_active: Duration,
    pub focused_other: Duration,
    pub unfocused: Duration,
    pub minimized: Duration,
    /// Fraction, 0.2 = +-20%.
    pub jitter: f64,
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence {
            focused_active: Duration::from_secs(5),
            focused_other: Duration::from_secs(15),
            unfocused: Duration::from_secs(30),
            minimized: Duration::from_secs(60),
            jitter: 0.2,
        }
    }
}

impl Cadence {
    /// The base interval for an account given the window state and whether it is the active account.
    pub fn interval(&self, window: WindowState, is_active_account: bool) -> Duration {
        todo!()
    }

    /// `base` scaled by `1 + jitter * (2 * unit - 1)`; `unit` is a random number in `[0, 1]` (injected for tests).
    pub fn jittered(&self, base: Duration, unit: f64) -> Duration {
        todo!()
    }
}
