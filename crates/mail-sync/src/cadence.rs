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
        match window {
            WindowState::Focused if is_active_account => self.focused_active,
            WindowState::Focused => self.focused_other,
            WindowState::Unfocused => self.unfocused,
            WindowState::Minimized => self.minimized,
        }
    }

    /// `base` scaled by `1 + jitter * (2 * unit - 1)`; `unit` is a random number in `[0, 1]` (injected for tests).
    pub fn jittered(&self, base: Duration, unit: f64) -> Duration {
        let unit = if unit.is_finite() { unit.clamp(0.0, 1.0) } else { 0.5 };
        let factor = (1.0 + self.jitter * (2.0 * unit - 1.0)).max(0.0);
        base.mul_f64(factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_selection() {
        let c = Cadence::default();
        assert_eq!(c.interval(WindowState::Focused, true), Duration::from_secs(5));
        assert_eq!(c.interval(WindowState::Focused, false), Duration::from_secs(15));
        assert_eq!(c.interval(WindowState::Unfocused, true), Duration::from_secs(30));
        assert_eq!(c.interval(WindowState::Unfocused, false), Duration::from_secs(30));
        assert_eq!(c.interval(WindowState::Minimized, true), Duration::from_secs(60));
        assert_eq!(c.interval(WindowState::Minimized, false), Duration::from_secs(60));
    }

    #[test]
    fn jitter_bounds() {
        let c = Cadence::default();
        let base = Duration::from_secs(10);
        assert_eq!(c.jittered(base, 0.0), Duration::from_secs(8));
        assert_eq!(c.jittered(base, 0.5), base);
        assert_eq!(c.jittered(base, 1.0), Duration::from_secs(12));
        assert_eq!(c.jittered(base, 7.0), Duration::from_secs(12));
        assert_eq!(c.jittered(base, f64::NAN), base);
    }
}
