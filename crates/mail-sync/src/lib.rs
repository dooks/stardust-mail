//! Freshness and fill logic (PLAN §3.6, WP-D). Talks only to the `GmailApi` and `MailCache` traits, so it is tested
//! against in-crate fakes and depends on neither the real cache nor the real client.
//!
//! Privacy: log ids, counts and timings only.

mod cadence;
mod gate;
mod sync;

pub use cadence::Cadence;
pub use gate::{Backoff, Decision, Kind, PollGate, Plan, Trigger, Ui};
pub use sync::{PollReport, cold_fill, fill_folder, folder_counts, poll, resync};

/// Page size of `messages.list` calls.
pub const PAGE_SIZE: u32 = 50;
/// The first hydration batch of a list (shown fast), then the rest.
pub const FIRST_BATCH: usize = 20;
/// Concurrency of `folder_counts`.
pub const COUNTS_CONCURRENCY: usize = 7;
