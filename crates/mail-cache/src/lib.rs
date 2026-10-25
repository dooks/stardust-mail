//! Per-account encrypted cache (PLAN §1 Cache, §3.3, WP-C).
//!
//! Layout: `<root>\<sub>\cache.db` (SQLite, WAL) + `<root>\<sub>\key.dpapi` (32-byte key, DPAPI with entropy
//! `gmail-ro-cache-key`). Headers, snippet and raw bodies are ChaCha20-Poly1305 blobs (random 12-byte nonce
//! prepended, the row id as AAD). Ids, label ids, dates and sizes are in the clear.
//!
//! Schema (created by `CacheRoot::account`):
//! ```sql
//! meta(k PRIMARY KEY, v)                                  -- history_id, synced_at, schema_version
//! msgs(id PK, thread_id, internal_date, size_estimate, mime_type, fetched_at, stale INT, meta BLOB)
//! msg_labels(id, label, PRIMARY KEY(id,label)) WITHOUT ROWID;  CREATE INDEX msg_labels_label ON msg_labels(label, id)
//! bodies(id PK, raw BLOB, fetched_at, last_opened)
//! windows(label PK, oldest_date, complete INT, next_page_token, as_of)
//! ```
//! `msgs.meta` = AEAD(JSON of the `MessageMeta` text fields: from, to, subject, date, snippet, history id).
//!
//! Wave 0 stubs: bodies are `todo!()`; signatures are final. Port `spikes\s4-sync\src\{store,delta,signout,dpapi}.rs`
//! and the AEAD variant (d) from `bench.rs`.
//!
//! Privacy: log ids, counts and sizes only.

#![allow(unused_variables, dead_code)] // Wave 0 stubs: remove when the file is implemented.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mail_model::{
    AccountId, CacheError, EvictReport, HistoryEvent, HistoryId, LabelId, MailCache, MessageId, MessageMeta,
    UpsertMode, Window,
};

/// DPAPI entropy of `key.dpapi`.
pub const KEY_ENTROPY: &[u8] = b"gmail-ro-cache-key";
/// Schema version stored in `meta`.
pub const SCHEMA_VERSION: u32 = 1;
/// Body cap per account (bytes of raw RFC 822, before encryption).
pub const MAX_BODY_BYTES_PER_ACCOUNT: u64 = 200 * 1024 * 1024;
/// List rows kept per label.
pub const MAX_ROWS_PER_LABEL: usize = 2000;
/// Bodies opened within this many days are never evicted.
pub const KEEP_OPENED_DAYS: i64 = 7;
/// The newest this many INBOX bodies are never evicted.
pub const KEEP_NEWEST_INBOX: usize = 20;

/// The directory that holds every account's cache directory.
#[derive(Debug, Clone)]
pub struct CacheRoot {
    dir: PathBuf,
    /// Accounts whose `AccountCache` is currently open (deletion is refused for these).
    open: Arc<Mutex<HashSet<String>>>,
}

impl CacheRoot {
    /// Use `dir` (`%LOCALAPPDATA%\com.ziggy.gmail-ro\cache`), creating it if needed.
    pub fn open(dir: &Path) -> Result<CacheRoot, CacheError> {
        todo!()
    }

    /// Open (creating directory, `key.dpapi` and schema on first use) one account's cache.
    pub fn account(&self, id: &AccountId) -> Result<AccountCache, CacheError> {
        todo!()
    }

    /// Delete an account's cache: rename `<id>` to `<id>.deleting-N`, then `remove_dir_all`. Refused with
    /// `CacheError::Locked` while an `AccountCache` for it is open (close it first). Leaves no WAL/SHM strays.
    pub fn delete_account(&self, id: &AccountId) -> Result<(), CacheError> {
        todo!()
    }

    /// Startup sweep: remove leftover `*.deleting-N` directories. Returns how many were removed.
    pub fn sweep(&self) -> usize {
        todo!()
    }
}

/// One account's open cache. Implements [`MailCache`]; `Send + Sync` (the connection sits behind a mutex).
pub struct AccountCache {
    id: AccountId,
    dir: PathBuf,
    conn: Mutex<rusqlite::Connection>,
    key: [u8; 32],
    open: Arc<Mutex<HashSet<String>>>,
}

impl AccountCache {
    /// Checkpoint the WAL and close the connection (and mark the account as not open for deletion purposes).
    pub fn close(self) -> Result<(), CacheError> {
        todo!()
    }
}

impl MailCache for AccountCache {
    fn cursor(&self) -> Result<Option<HistoryId>, CacheError> {
        todo!()
    }

    fn set_cursor(&self, cursor: HistoryId, synced_at_ms: i64) -> Result<(), CacheError> {
        todo!()
    }

    fn upsert_meta(&self, metas: &[MessageMeta], now_ms: i64, mode: UpsertMode) -> Result<usize, CacheError> {
        todo!()
    }

    fn rows(
        &self,
        label: &LabelId,
        before: Option<i64>,
        limit: usize,
        include_stale: bool,
    ) -> Result<Vec<MessageMeta>, CacheError> {
        todo!()
    }

    fn has(&self, ids: &[MessageId]) -> Result<HashSet<MessageId>, CacheError> {
        todo!()
    }

    fn apply_events(&self, events: &[HistoryEvent]) -> Result<Vec<MessageId>, CacheError> {
        todo!()
    }

    fn window(&self, label: &LabelId) -> Result<Option<Window>, CacheError> {
        todo!()
    }

    fn set_window(&self, window: &Window) -> Result<(), CacheError> {
        todo!()
    }

    fn mark_all_stale_and_drop_windows(&self) -> Result<usize, CacheError> {
        todo!()
    }

    fn body(&self, id: &MessageId) -> Result<Option<Vec<u8>>, CacheError> {
        todo!()
    }

    fn put_body(&self, id: &MessageId, raw: &[u8], now_ms: i64) -> Result<bool, CacheError> {
        todo!()
    }

    fn touch_opened(&self, id: &MessageId, now_ms: i64) -> Result<(), CacheError> {
        todo!()
    }

    fn label_ids(&self, id: &MessageId) -> Result<Option<Vec<LabelId>>, CacheError> {
        todo!()
    }

    fn evict(&self, now_ms: i64) -> Result<EvictReport, CacheError> {
        todo!()
    }

    fn synced_at(&self) -> Result<Option<i64>, CacheError> {
        todo!()
    }
}
