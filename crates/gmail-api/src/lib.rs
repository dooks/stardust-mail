//! Read-only Gmail REST v1 client (PLAN §3.4, WP-B).
//!
//! Only GET requests plus the batch POST whose sub-requests are GETs. WP-B adds a test that greps this crate's
//! source for forbidden write-endpoint path segments (see PLAN §3.4); keep such strings out of code and comments.
//!
//! Privacy: log status codes, ids, counts and timings only.

mod batch;
mod classify;
mod client;

pub use batch::{BatchPart, boundary_of, build_batch_body, parse_batch_response};
pub use classify::classify;
pub use client::GmailClient;
pub use reqwest::Url;

/// `https://gmail.googleapis.com/` (the default base; tests pass an `httpmock` URL).
pub const DEFAULT_BASE: &str = "https://gmail.googleapis.com/";
/// Path of the batch endpoint relative to the base.
pub const BATCH_PATH: &str = "batch/gmail/v1";
/// Concurrency of the fallback when a batch request itself fails.
pub const FALLBACK_CONCURRENCY: usize = 20;
