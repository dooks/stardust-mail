//! Shared plain types, traits, errors and command DTOs for gmail-ro (PLAN.md §3.1).
//!
//! This crate performs no I/O. Everything the other crates agree on lives here; Wave 1 packages must not
//! change these signatures (report a contract problem instead).
//!
//! Privacy: nothing in this crate logs. `Display` impls of errors carry ids, counts and status codes only,
//! never subjects, addresses, snippets or bodies.

mod error;
mod gmail;
mod ids;
pub mod label;
mod traits;
mod tree;
mod view;

pub use error::{ApiError, AuthError, CacheError, CmdError, CmdErrorCode, RenderError};
pub use gmail::{
    HistoryEvent, HistoryPage, LabelCounts, LabelInfo, LabelKind, ListPage, MessageMeta, Profile,
};
pub use ids::{AccountId, HistoryId, LabelId, MessageId, PartId, ThreadId};
pub use traits::{EvictReport, GmailApi, MailCache, TokenSource, UpsertMode, Window};
pub use tree::build_tree;
pub use view::{
    AccountState, AccountView, Addr, AttachmentInfo, AuthLevel, AuthSummary, FolderKind, FolderView, ListResult,
    MailChanged, MessageRow, MessageView, SaveResult, SignInProgress, SignInStage, SignOutResult, SyncState,
    SyncStatus, WindowState,
};

/// Application identifier; also the directory name under `%LOCALAPPDATA%`.
pub const APP_ID: &str = "com.ziggy.gmail-ro";
/// Origin of the `gmail-asset` protocol on Windows (WebView2 maps custom schemes to `http://<scheme>.localhost`).
pub const ASSET_HOST: &str = "http://gmail-asset.localhost";
/// Raw bodies larger than this are shown but never stored in the cache (5 MiB).
pub const MAX_CACHED_BODY: usize = 5 * 1024 * 1024;

/// Milliseconds since the Unix epoch (wall clock). Cache and sync code take timestamps from here so tests
/// can also pass explicit values.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
