//! Pure logic: one poll in flight per account (manual-refresh coalescing) and the failure/backoff policy.

// Reference logic for the real app; the CLI exercises only part of it (the rest is covered by unit tests).
#![allow(dead_code)]

use std::time::Duration;

use crate::api::Kind;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Trigger {
    Timer,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    /// Nothing running: start a poll now.
    Start,
    /// A timer tick while a poll runs: drop it (the running poll covers it).
    Skip,
    /// A manual refresh while a poll runs: run exactly one more poll right after it ends.
    Queued,
    /// Another manual refresh while one is already queued: collapsed into the queued one.
    Collapsed,
}

#[derive(Default, Debug)]
pub struct PollGate {
    in_flight: bool,
    trailing: bool,
}

impl PollGate {
    pub fn request(&mut self, t: Trigger) -> Decision {
        match (self.in_flight, t) {
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

    /// Call when a poll ends. Returns true when the caller must start the queued trailing poll now
    /// (the gate stays "in flight" for it).
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

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ui {
    Live,
    /// Show the "cached, as of ..." banner.
    Offline,
    Throttled,
    NeedsSignIn,
}

#[derive(Debug, PartialEq)]
pub struct Plan {
    /// None = stop polling this account (needs user action).
    pub delay: Option<Duration>,
    pub ui: Ui,
    pub resync: bool,
}

#[derive(Default, Debug)]
pub struct Backoff {
    fails: u32,
    auth_fails: u32,
}

const CAP_OFFLINE: u64 = 300;
const CAP_THROTTLE: u64 = 600;

impl Backoff {
    /// Decide what to do after a poll with `kind`. `base` is the normal cadence. Jitter is applied by the caller.
    pub fn next(&mut self, kind: &Kind, base: Duration) -> Plan {
        match kind {
            Kind::Ok => {
                *self = Backoff::default();
                Plan { delay: Some(base), ui: Ui::Live, resync: false }
            }
            Kind::Unauthorized => {
                self.auth_fails += 1;
                if self.auth_fails == 1 {
                    // The poll helper already retried once with another token call; one more immediate try.
                    Plan { delay: Some(Duration::ZERO), ui: Ui::Live, resync: false }
                } else {
                    Plan { delay: None, ui: Ui::NeedsSignIn, resync: false }
                }
            }
            Kind::RateLimited(ra) => {
                self.fails += 1;
                let exp = (30u64 << (self.fails - 1).min(5)).min(CAP_THROTTLE);
                let secs = ra.unwrap_or(0).max(exp).min(CAP_THROTTLE);
                Plan { delay: Some(Duration::from_secs(secs)), ui: if self.fails >= 3 { Ui::Throttled } else { Ui::Live }, resync: false }
            }
            Kind::Offline | Kind::Server => {
                self.fails += 1;
                let secs = (base.as_secs().max(1) << self.fails.min(6)).min(CAP_OFFLINE);
                Plan { delay: Some(Duration::from_secs(secs)), ui: if self.fails >= 2 { Ui::Offline } else { Ui::Live }, resync: false }
            }
            Kind::Gone => Plan { delay: Some(Duration::ZERO), ui: Ui::Live, resync: true },
            Kind::Other => {
                self.fails += 1;
                Plan { delay: Some(Duration::from_secs(60)), ui: Ui::Live, resync: false }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_during_inflight_coalesces() {
        let mut g = PollGate::default();
        assert_eq!(g.request(Trigger::Timer), Decision::Start);
        assert_eq!(g.request(Trigger::Timer), Decision::Skip);
        assert_eq!(g.request(Trigger::Manual), Decision::Queued);
        assert_eq!(g.request(Trigger::Manual), Decision::Collapsed);
        assert_eq!(g.request(Trigger::Manual), Decision::Collapsed);
        // First poll ends: exactly one trailing poll is started.
        assert!(g.finish());
        assert_eq!(g.request(Trigger::Timer), Decision::Skip);
        assert!(!g.finish());
        assert_eq!(g.request(Trigger::Manual), Decision::Start);
    }

    #[test]
    fn manual_when_idle_starts() {
        let mut g = PollGate::default();
        assert_eq!(g.request(Trigger::Manual), Decision::Start);
        assert!(!g.finish());
    }

    #[test]
    fn offline_backoff_grows_and_caps_then_recovers() {
        let mut b = Backoff::default();
        let base = Duration::from_secs(5);
        let p1 = b.next(&Kind::Offline, base);
        assert_eq!(p1.delay, Some(Duration::from_secs(10)));
        assert_eq!(p1.ui, Ui::Live);
        let p2 = b.next(&Kind::Offline, base);
        assert_eq!(p2.delay, Some(Duration::from_secs(20)));
        assert_eq!(p2.ui, Ui::Offline);
        for _ in 0..10 {
            b.next(&Kind::Offline, base);
        }
        assert_eq!(b.next(&Kind::Offline, base).delay, Some(Duration::from_secs(300)));
        let ok = b.next(&Kind::Ok, base);
        assert_eq!(ok, Plan { delay: Some(base), ui: Ui::Live, resync: false });
    }

    #[test]
    fn unauthorized_twice_stops() {
        let mut b = Backoff::default();
        let base = Duration::from_secs(5);
        assert_eq!(b.next(&Kind::Unauthorized, base).delay, Some(Duration::ZERO));
        let p = b.next(&Kind::Unauthorized, base);
        assert_eq!(p.delay, None);
        assert_eq!(p.ui, Ui::NeedsSignIn);
    }

    #[test]
    fn rate_limit_honours_retry_after() {
        let mut b = Backoff::default();
        let base = Duration::from_secs(5);
        let p = b.next(&Kind::RateLimited(Some(120)), base);
        assert_eq!(p.delay, Some(Duration::from_secs(120)));
        let mut b = Backoff::default();
        assert_eq!(b.next(&Kind::RateLimited(None), base).delay, Some(Duration::from_secs(30)));
        assert_eq!(b.next(&Kind::RateLimited(None), base).delay, Some(Duration::from_secs(60)));
        assert_eq!(b.next(&Kind::RateLimited(None), base).ui, Ui::Throttled);
    }

    #[test]
    fn gone_requests_resync() {
        let mut b = Backoff::default();
        assert!(b.next(&Kind::Gone, Duration::from_secs(5)).resync);
    }
}
