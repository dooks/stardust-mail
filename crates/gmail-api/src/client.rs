//! `GmailClient`: the `GmailApi` implementation over `reqwest`.

use mail_model::{
    ApiError, GmailApi, HistoryId, HistoryPage, LabelCounts, LabelId, LabelInfo, ListPage, MessageId, MessageMeta,
    Profile, TokenSource,
};
use reqwest::Url;

/// Gmail client for ONE account, generic over where its bearer tokens come from (`gauth::Tokens` in the app, a
/// fake in tests).
///
/// On a 401 it calls `access_token(force_refresh = true)` once and retries once. Transport errors are
/// `ApiError::Offline`. `metadata_batch` posts one multipart batch of GET sub-requests and falls back to
/// concurrent GETs (concurrency [`crate::FALLBACK_CONCURRENCY`]) if the batch call itself fails.
pub struct GmailClient<T> {
    tokens: T,
    http: reqwest::Client,
    base: Url,
}

impl<T: TokenSource + Send + Sync> GmailClient<T> {
    /// `base` overrides [`crate::DEFAULT_BASE`] (httpmock in tests). HTTP/2, rustls, request timeouts set here.
    pub fn new(tokens: T, base: Option<Url>) -> Self {
        todo!()
    }
}

impl<T: TokenSource + Send + Sync> GmailApi for GmailClient<T> {
    async fn profile(&self) -> Result<Profile, ApiError> {
        todo!()
    }

    async fn labels(&self) -> Result<Vec<LabelInfo>, ApiError> {
        todo!()
    }

    async fn label_counts(&self, id: &LabelId) -> Result<LabelCounts, ApiError> {
        todo!()
    }

    /// Sets `includeSpamTrash=true` for `SPAM` / `TRASH`.
    async fn list(&self, label: Option<&LabelId>, page_token: Option<&str>, max: u32) -> Result<ListPage, ApiError> {
        todo!()
    }

    async fn metadata_batch(&self, ids: &[MessageId]) -> Result<Vec<Result<MessageMeta, ApiError>>, ApiError> {
        todo!()
    }

    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>, ApiError> {
        todo!()
    }

    /// `historyTypes=messageAdded,messageDeleted,labelAdded,labelRemoved&maxResults=500`; 404 -> `HistoryExpired`.
    async fn history(&self, start: HistoryId, page_token: Option<&str>) -> Result<HistoryPage, ApiError> {
        todo!()
    }
}
