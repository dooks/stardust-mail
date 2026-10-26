//! A type-erased `GmailApi`. The trait uses RPITIT and is not object safe, but the service holds one API object per
//! account and tests substitute fakes, so the object is boxed behind [`DynApi`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mail_model::{
    ApiError, GmailApi, HistoryId, HistoryPage, LabelCounts, LabelId, LabelInfo, ListPage, MessageId, MessageMeta, Profile,
};

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

trait Erased: Send + Sync {
    fn profile(&self) -> Fut<'_, Profile>;
    fn labels(&self) -> Fut<'_, Vec<LabelInfo>>;
    fn label_counts<'a>(&'a self, id: &'a LabelId) -> Fut<'a, LabelCounts>;
    fn list<'a>(&'a self, label: Option<&'a LabelId>, page_token: Option<&'a str>, max: u32) -> Fut<'a, ListPage>;
    fn metadata_batch<'a>(&'a self, ids: &'a [MessageId]) -> Fut<'a, Vec<Result<MessageMeta, ApiError>>>;
    fn raw<'a>(&'a self, id: &'a MessageId) -> Fut<'a, Vec<u8>>;
    fn history<'a>(&'a self, start: HistoryId, page_token: Option<&'a str>) -> Fut<'a, HistoryPage>;
}

impl<T: GmailApi + Send + Sync> Erased for T {
    fn profile(&self) -> Fut<'_, Profile> {
        Box::pin(GmailApi::profile(self))
    }
    fn labels(&self) -> Fut<'_, Vec<LabelInfo>> {
        Box::pin(GmailApi::labels(self))
    }
    fn label_counts<'a>(&'a self, id: &'a LabelId) -> Fut<'a, LabelCounts> {
        Box::pin(GmailApi::label_counts(self, id))
    }
    fn list<'a>(&'a self, label: Option<&'a LabelId>, page_token: Option<&'a str>, max: u32) -> Fut<'a, ListPage> {
        Box::pin(GmailApi::list(self, label, page_token, max))
    }
    fn metadata_batch<'a>(&'a self, ids: &'a [MessageId]) -> Fut<'a, Vec<Result<MessageMeta, ApiError>>> {
        Box::pin(GmailApi::metadata_batch(self, ids))
    }
    fn raw<'a>(&'a self, id: &'a MessageId) -> Fut<'a, Vec<u8>> {
        Box::pin(GmailApi::raw(self, id))
    }
    fn history<'a>(&'a self, start: HistoryId, page_token: Option<&'a str>) -> Fut<'a, HistoryPage> {
        Box::pin(GmailApi::history(self, start, page_token))
    }
}

/// A shared, type-erased [`GmailApi`] for one account.
#[derive(Clone)]
pub struct DynApi(Arc<dyn Erased>);

impl DynApi {
    /// Erase any Gmail API implementation (the real `GmailClient`, or a fake in tests).
    pub fn new<T: GmailApi + Send + Sync + 'static>(api: T) -> DynApi {
        DynApi(Arc::new(api))
    }
}

impl GmailApi for DynApi {
    async fn profile(&self) -> Result<Profile, ApiError> {
        self.0.profile().await
    }
    async fn labels(&self) -> Result<Vec<LabelInfo>, ApiError> {
        self.0.labels().await
    }
    async fn label_counts(&self, id: &LabelId) -> Result<LabelCounts, ApiError> {
        self.0.label_counts(id).await
    }
    async fn list(&self, label: Option<&LabelId>, page_token: Option<&str>, max: u32) -> Result<ListPage, ApiError> {
        self.0.list(label, page_token, max).await
    }
    async fn metadata_batch(&self, ids: &[MessageId]) -> Result<Vec<Result<MessageMeta, ApiError>>, ApiError> {
        self.0.metadata_batch(ids).await
    }
    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>, ApiError> {
        self.0.raw(id).await
    }
    async fn history(&self, start: HistoryId, page_token: Option<&str>) -> Result<HistoryPage, ApiError> {
        self.0.history(start, page_token).await
    }
}
