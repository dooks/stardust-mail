//! Pure policy: one poll in flight per account (manual-refresh coalescing) and the failure / backoff table.
//! Ported from spike S4's `gate.rs`.

use std::time::Duration;

use mail_model::{ApiError, AuthError};

/// What asked for a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Timer,
    Manual,
}

/// What the gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing running: start a poll now (the gate is now "in flight").
    Start,
    /// A timer tick while a poll runs: drop it.
    Skip,
    /// A manual refresh while a poll runs: run exactly one more poll right after it ends.
    Queued,
    /// Another manual refresh while one is already queued: collapsed into the queued one.
    Collapsed,
}

/// One poll in flight per account; a manual click during a poll queues exactly one trailing poll.
#[derive(Debug, Default)]
pub struct PollGate {
    in_flight: bool,
    trailing: bool,
}

impl PollGate {
    pub fn request(&mut self, trigger: Trigger) -> Decision {
        match (self.in_flight, trigger) {
            (false, _) => {
                self.in_flight = true;
                Decision::Start
            }
            (true, Trigger::Timer) => Decision::Skip,
            (true, Trigger::Manual) if self.trailing => Decision::Collapsed,
            (true, Trigger::Manual) => {
                self.trailing = true;
                Decision::Queued
            }
        }
    }

    /// Call when a poll ends. `true` means the caller must start the queued trailing poll now (the gate stays
    /// "in flight" for it).
    pub fn finish(&mut self) -> bool {
        if self.trailing {
            self.trailing = false;
            true
        } else {
            self.in_flight = false;
            false
        }
    }
}

/// Outcome class of a poll, as input to [`Backoff::next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ok,
    /// `Auth(NeedsReauth)` (or any other auth failure): stop polling this account.
    NeedsReauth,
    /// Optional `Retry-After` seconds.
    RateLimited(Option<u64>),
    /// Transport failure.
    Offline,
    /// HTTP 5xx.
    Server,
    /// `HistoryExpired`: resync.
    Gone,
    Other,
}

impl Kind {
    /// Classify an error: `Auth(NotSignedIn|NeedsReauth|MissingScope)` -> `NeedsReauth`; `Auth(Network)` and
    /// `Offline` -> `Offline`; `Status{5xx}` -> `Server`; `RateLimited` -> `RateLimited`; `HistoryExpired` -> `Gone`.
    pub fn of(error: &ApiError) -> Kind {
        match error {
            ApiError::Auth(AuthError::Network(_)) | ApiError::Offline(_) => Kind::Offline,
            ApiError::Auth(_) => Kind::NeedsReauth,
            ApiError::Status { code, .. } if (500..600).contains(code) => Kind::Server,
            ApiError::RateLimited { retry_after_s } => Kind::RateLimited(*retry_after_s),
            ApiError::HistoryExpired => Kind::Gone,
            _ => Kind::Other,
        }
    }
}

/// What the UI banner should say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ui {
    Live,
    Offline,
    Throttled,
    NeedsReauth,
}

/// What to do after a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// `None` = stop polling this account (needs the user).
    pub delay: Option<Duration>,
    pub ui: Ui,
    /// Run `resync` before the next poll.
    pub resync: bool,
}

/// Failure counters. Tables (PLAN §3.6): `RateLimited` 30 s doubling capped at 600 s (honour Retry-After, also
/// capped); `Offline`/`Server` 5 s doubling capped at 300 s; `NeedsReauth` stops; `Ok` resets and waits `base`.
#[derive(Debug, Default)]
pub struct Backoff {
    fails: u32,
}

impl Backoff {
    /// Decide what to do after a poll with outcome `kind`. `base` is the normal cadence interval (jitter is
    /// applied by the caller).
    pub fn next(&mut self, kind: Kind, base: Duration) -> Plan {
        let secs = |s: u64| Some(Duration::from_secs(s));
        match kind {
            Kind::Ok => {
                self.fails = 0;
                Plan { delay: Some(base), ui: Ui::Live, resync: false }
            }
            Kind::NeedsReauth => Plan { delay: None, ui: Ui::NeedsReauth, resync: false },
            Kind::RateLimited(retry_after) => {
                self.fails = self.fails.saturating_add(1);
                let exp = (30u64 << (self.fails - 1).min(10)).min(600);
                let s = retry_after.unwrap_or(0).max(exp).min(600);
                Plan { delay: secs(s), ui: if self.fails >= 3 { Ui::Throttled } else { Ui::Live }, resync: false }
            }
            Kind::Offline | Kind::Server => {
                self.fails = self.fails.saturating_add(1);
                let s = (5u64 << (self.fails - 1).min(10)).min(300);
                Plan { delay: secs(s), ui: if self.fails >= 2 { Ui::Offline } else { Ui::Live }, resync: false }
            }
            Kind::Gone => Plan { delay: Some(Duration::ZERO), ui: Ui::Live, resync: true },
            Kind::Other => {
                self.fails = self.fails.saturating_add(1);
                Plan { delay: secs(60), ui: Ui::Live, resync: false }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_secs(5);

    fn secs(p: Plan) -> u64 {
        p.delay.expect("delay").as_secs()
    }

    #[test]
    fn manual_during_inflight_gives_one_trailing_poll() {
        let mut g = PollGate::default();
        assert_eq!(g.request(Trigger::Timer), Decision::Start);
        assert_eq!(g.request(Trigger::Timer), Decision::Skip);
        assert_eq!(g.request(Trigger::Manual), Decision::Queued);
        assert_eq!(g.request(Trigger::Manual), Decision::Collapsed);
        assert_eq!(g.request(Trigger::Manual), Decision::Collapsed);
        assert!(g.finish());
        // The trailing poll is now in flight.
        assert_eq!(g.request(Trigger::Timer), Decision::Skip);
        assert!(!g.finish());
        assert_eq!(g.request(Trigger::Manual), Decision::Start);
    }

    #[test]
    fn idle_manual_starts() {
        let mut g = PollGate::default();
        assert_eq!(g.request(Trigger::Manual), Decision::Start);
        assert!(!g.finish());
        assert_eq!(g.request(Trigger::Timer), Decision::Start);
    }

    #[test]
    fn kind_classification() {
        let k = Kind::of;
        assert_eq!(k(&ApiError::Auth(AuthError::NotSignedIn)), Kind::NeedsReauth);
        assert_eq!(k(&ApiError::Auth(AuthError::NeedsReauth { reason: "x".into() })), Kind::NeedsReauth);
        assert_eq!(k(&ApiError::Auth(AuthError::MissingScope { granted: vec![] })), Kind::NeedsReauth);
        assert_eq!(k(&ApiError::Auth(AuthError::Network("x".into()))), Kind::Offline);
        assert_eq!(k(&ApiError::Offline("x".into())), Kind::Offline);
        assert_eq!(k(&ApiError::Status { code: 503, reason: "x".into() }), Kind::Server);
        assert_eq!(k(&ApiError::Status { code: 400, reason: "x".into() }), Kind::Other);
        assert_eq!(k(&ApiError::RateLimited { retry_after_s: Some(7) }), Kind::RateLimited(Some(7)));
        assert_eq!(k(&ApiError::HistoryExpired), Kind::Gone);
        assert_eq!(k(&ApiError::NotFound), Kind::Other);
    }

    #[test]
    fn rate_limited_table() {
        let mut b = Backoff::default();
        let got: Vec<u64> = (0..7).map(|_| secs(b.next(Kind::RateLimited(None), BASE))).collect();
        assert_eq!(got, [30, 60, 120, 240, 480, 600, 600]);
        let mut b = Backoff::default();
        assert_eq!(secs(b.next(Kind::RateLimited(Some(100)), BASE)), 100);
        assert_eq!(secs(b.next(Kind::RateLimited(Some(10)), BASE)), 60);
        assert_eq!(secs(b.next(Kind::RateLimited(Some(5000)), BASE)), 600);
    }

    #[test]
    fn throttled_banner_from_third_hit() {
        let mut b = Backoff::default();
        assert_eq!(b.next(Kind::RateLimited(None), BASE).ui, Ui::Live);
        assert_eq!(b.next(Kind::RateLimited(None), BASE).ui, Ui::Live);
        assert_eq!(b.next(Kind::RateLimited(None), BASE).ui, Ui::Throttled);
    }

    #[test]
    fn offline_and_server_table() {
        let mut b = Backoff::default();
        let kinds = [Kind::Offline, Kind::Server, Kind::Offline, Kind::Server, Kind::Offline];
        let got: Vec<u64> = kinds.iter().map(|k| secs(b.next(*k, BASE))).collect();
        assert_eq!(got, [5, 10, 20, 40, 80]);
        for _ in 0..10 {
            b.next(Kind::Offline, BASE);
        }
        assert_eq!(secs(b.next(Kind::Offline, BASE)), 300);
    }

    #[test]
    fn offline_banner_from_second_failure_and_ok_resets() {
        let mut b = Backoff::default();
        assert_eq!(b.next(Kind::Offline, BASE).ui, Ui::Live);
        assert_eq!(b.next(Kind::Offline, BASE).ui, Ui::Offline);
        let ok = b.next(Kind::Ok, BASE);
        assert_eq!(ok, Plan { delay: Some(BASE), ui: Ui::Live, resync: false });
        assert_eq!(secs(b.next(Kind::Offline, BASE)), 5);
    }

    #[test]
    fn needs_reauth_stops_and_gone_resyncs() {
        let mut b = Backoff::default();
        let p = b.next(Kind::NeedsReauth, BASE);
        assert_eq!(p, Plan { delay: None, ui: Ui::NeedsReauth, resync: false });
        let p = b.next(Kind::Gone, BASE);
        assert!(p.resync);
        assert_eq!(p.delay, Some(Duration::ZERO));
    }
}
