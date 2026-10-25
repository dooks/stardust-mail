//! Pure policy: one poll in flight per account (manual-refresh coalescing) and the failure / backoff table.
//! Ported from spike S4's `gate.rs`.

use std::time::Duration;

use mail_model::ApiError;

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
        todo!()
    }

    /// Call when a poll ends. `true` means the caller must start the queued trailing poll now (the gate stays
    /// "in flight" for it).
    pub fn finish(&mut self) -> bool {
        todo!()
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
        todo!()
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
        todo!()
    }
}
