//! The three seams between crates: where tokens come from, the Gmail API, and the per-account cache.

use std::collections::HashSet;

use crate::error::{ApiError, AuthError, CacheError};
use crate::gmail::{HistoryEvent, HistoryPage, LabelCounts, LabelInfo, ListPage, MessageMeta, Profile};
use crate::ids::{HistoryId, LabelId, MessageId};

/// Supplies bearer tokens to `gmail-api`. Implemented by `gauth::Tokens`.
pub trait TokenSource {
    /// A valid access token. `force_refresh = true` discards the cached one first (used after a 401).
    /// Offline refresh failures are `AuthError::Network`; a dead refresh token is `AuthError::NeedsReauth`.
    fn access_token(&self, force_refresh: bool) -> impl Future<Output = Result<String, AuthError>> + Send;
}

/// Read-only Gmail operations for ONE account. Implemented by `gmail_api::GmailClient`; faked in tests.
/// Nothing here can modify a mailbox.
pub trait GmailApi {
    /// `users.getProfile`.
    fn profile(&self) -> impl Future<Output = Result<Profile, ApiError>> + Send;
    /// `labels.list` (ids, names, kinds).
    fn labels(&self) -> impl Future<Output = Result<Vec<LabelInfo>, ApiError>> + Send;
    /// `labels.get`: total / unread counts.
    fn label_counts(&self, id: &LabelId) -> impl Future<Output = Result<LabelCounts, ApiError>> + Send;
    /// `messages.list`. `label = None` means All Mail. Sets `includeSpamTrash=true` for SPAM and TRASH.
    fn list(
        &self,
        label: Option<&LabelId>,
        page_token: Option<&str>,
        max: u32,
    ) -> impl Future<Output = Result<ListPage, ApiError>> + Send;
    /// Metadata for up to 50 ids in one batch request. The inner results are per message (a vanished message is
    /// `Err(NotFound)`); the outer `Err` is a whole-call failure (after the concurrent-GET fallback also failed).
    fn metadata_batch(
        &self,
        ids: &[MessageId],
    ) -> impl Future<Output = Result<Vec<Result<MessageMeta, ApiError>>, ApiError>> + Send;
    /// `messages.get?format=raw`, base64url-decoded RFC 822 bytes.
    fn raw(&self, id: &MessageId) -> impl Future<Output = Result<Vec<u8>, ApiError>> + Send;
    /// `history.list` after `start`. A 404 is `Err(HistoryExpired)`.
    fn history(
        &self,
        start: HistoryId,
        page_token: Option<&str>,
    ) -> impl Future<Output = Result<HistoryPage, ApiError>> + Send;
}

/// What the cache knows about one label's list ("window"): how far back the cached rows reach.
///
/// `label` is a label id, or `"ALL"` (`label::ALL_MAIL`) for All Mail. The cache treats a window on `ALL` as
/// matching every label set (an All Mail window admits any message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub label: LabelId,
    /// `internal_date_ms` of the oldest row known to be contiguous in this list (rows older than this are not
    /// guaranteed to be cached).
    pub oldest_date: i64,
    /// True when the whole list is cached (the end of the mailbox list was reached).
    pub complete: bool,
    /// Where `fill_folder(more = true)` continues; `None` when complete.
    pub next_page_token: Option<String>,
    /// When the window was last refreshed from the server (ms since the epoch).
    pub as_of: i64,
}

/// How [`MailCache::upsert_meta`] decides whether a row may enter the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertMode {
    /// Store every row (used by folder fills, which then widen the window).
    Always,
    /// S4's admit rule: store a row only if it is already cached, or one of its labels has a window that is
    /// complete or reaches back to the row's date. Used when hydrating ids returned by `apply_events`.
    Admit,
}

/// Counts of what [`MailCache::evict`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictReport {
    pub bodies_removed: usize,
    pub rows_removed: usize,
}

/// One account's local cache. Synchronous (SQLite); callers on async tasks should keep calls short.
/// Implemented by `mail_cache::AccountCache`; faked in `mail-sync` tests. All `now_ms` arguments are
/// milliseconds since the epoch (`mail_model::now_ms()`), passed in so tests are deterministic.
///
/// Rows are immutable except for labels. A row marked stale (after a resync) stays readable offline but is
/// excluded from online lists until revalidated by a fresh `upsert_meta`.
pub trait MailCache: Send + Sync {
    /// The stored history cursor, if any.
    fn cursor(&self) -> Result<Option<HistoryId>, CacheError>;
    /// Store the cursor and record `synced_at_ms` as the time the cache was last known current.
    fn set_cursor(&self, cursor: HistoryId, synced_at_ms: i64) -> Result<(), CacheError>;
    /// Insert or replace rows (headers + snippet encrypted, labels in `msg_labels`), clearing `stale`.
    /// Returns how many rows were stored (`Admit` may skip some).
    fn upsert_meta(&self, metas: &[MessageMeta], now_ms: i64, mode: UpsertMode) -> Result<usize, CacheError>;
    /// Rows of one label (`"ALL"` = every row), newest first by `internal_date_ms`, strictly older than `before`
    /// when given, at most `limit`. Stale rows are excluded unless `include_stale` (used when offline).
    fn rows(
        &self,
        label: &LabelId,
        before: Option<i64>,
        limit: usize,
        include_stale: bool,
    ) -> Result<Vec<MessageMeta>, CacheError>;
    /// Which of `ids` are cached (stale or not).
    fn has(&self, ids: &[MessageId]) -> Result<HashSet<MessageId>, CacheError>;
    /// Apply history events in order (labels added/removed on cached rows, deletions) and return the ids that are
    /// not cached but now belong to a label whose window we hold: the caller fetches their metadata and calls
    /// `upsert_meta(.., UpsertMode::Admit)`.
    fn apply_events(&self, events: &[HistoryEvent]) -> Result<Vec<MessageId>, CacheError>;
    /// The window of one label, if any.
    fn window(&self, label: &LabelId) -> Result<Option<Window>, CacheError>;
    /// Create or replace a window.
    fn set_window(&self, window: &Window) -> Result<(), CacheError>;
    /// Resync step: mark every row stale and delete all windows. Returns the number of rows marked.
    fn mark_all_stale_and_drop_windows(&self) -> Result<usize, CacheError>;
    /// The cached raw RFC 822 body, decrypted.
    fn body(&self, id: &MessageId) -> Result<Option<Vec<u8>>, CacheError>;
    /// Store a raw body (encrypted). Returns `false` and stores nothing when `raw.len() > MAX_CACHED_BODY`.
    fn put_body(&self, id: &MessageId, raw: &[u8], now_ms: i64) -> Result<bool, CacheError>;
    /// Record that the body was just opened (LRU / keep-last-7-days rule).
    fn touch_opened(&self, id: &MessageId, now_ms: i64) -> Result<(), CacheError>;
    /// One cached row's metadata plus its `stale` flag (true after a resync until a fresh `upsert_meta`
    /// revalidates it). `None` when the row is not cached. Stale rows are returned too.
    fn meta(&self, id: &MessageId) -> Result<Option<(MessageMeta, bool)>, CacheError>;
    /// Current label ids of a cached row.
    fn label_ids(&self, id: &MessageId) -> Result<Option<Vec<LabelId>>, CacheError>;
    /// Enforce the caps: bodies <= 200 MB (LRU, but keep bodies opened in the last 7 days and the newest 20 INBOX
    /// ones) and <= 2,000 rows per label.
    fn evict(&self, now_ms: i64) -> Result<EvictReport, CacheError>;
    /// When the cache was last known current (the `now` of the last `set_cursor`), for the "as of" banner.
    fn synced_at(&self) -> Result<Option<i64>, CacheError>;
}
