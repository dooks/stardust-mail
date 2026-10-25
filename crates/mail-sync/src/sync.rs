//! The sync operations: poll, cold fill, folder fill, resync, folder counts.

use mail_model::{ApiError, GmailApi, LabelCounts, LabelId, MailCache};

/// What one poll changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PollReport {
    /// New rows hydrated and admitted into the cache.
    pub added: usize,
    /// Rows whose labels changed.
    pub changed: usize,
    /// Rows deleted.
    pub removed: usize,
    /// A `HistoryExpired` led to a resync during this poll.
    pub resynced: bool,
}

/// One poll: `history.list` from the stored cursor, following `nextPageToken`; apply the events with
/// `MailCache::apply_events`; hydrate the returned ids with `metadata_batch` and `upsert_meta(.., Admit)`; advance
/// the cursor (`set_cursor(new, now_ms())`) only after everything succeeded. No cursor -> `cold_fill`.
/// `HistoryExpired` -> [`resync`] (report has `resynced = true`).
pub async fn poll<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<PollReport, ApiError> {
    todo!()
}

/// First launch of an account: `profile` (for the cursor) BEFORE `list(INBOX, 50)`, then hydrate (first 20, then
/// the other 30 as two batches), upsert, set the INBOX window and the cursor.
pub async fn cold_fill<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<(), ApiError> {
    todo!()
}

/// Fill one folder's list. `label` may be `"ALL"` (All Mail, no label filter). Lists a page, skips ids already
/// cached, hydrates the rest (20 then 30), upserts (`UpsertMode::Always`), then creates/updates the window
/// (`oldest_date`, `complete = next_page_token.is_none()`, `next_page_token`, `as_of`). With `more = true` it
/// continues from the window's `next_page_token` (no-op if the window is complete).
pub async fn fill_folder<A: GmailApi, C: MailCache>(
    api: &A,
    cache: &C,
    label: &LabelId,
    more: bool,
) -> Result<(), ApiError> {
    todo!()
}

/// History expired: `mark_all_stale_and_drop_windows`, take a new cursor from `profile`, refill INBOX.
pub async fn resync<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<(), ApiError> {
    todo!()
}

/// `labels.get` for each id with concurrency [`crate::COUNTS_CONCURRENCY`]. A label whose lookup fails yields
/// `LabelCounts::default()` (unknown); results are in the order of `labels`.
pub async fn folder_counts<A: GmailApi>(api: &A, labels: &[LabelId]) -> Vec<(LabelId, LabelCounts)> {
    todo!()
}
