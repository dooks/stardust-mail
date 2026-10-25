//! The sync operations: poll, cold fill, folder fill, resync, folder counts.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use mail_model::{
    ApiError, CacheError, GmailApi, HistoryEvent, LabelCounts, LabelId, MailCache, MessageId, MessageMeta,
    UpsertMode, Window, label, now_ms,
};

use crate::{COUNTS_CONCURRENCY, FIRST_BATCH, PAGE_SIZE};

/// `metadata_batch` accepts at most this many ids.
const BATCH_MAX: usize = 50;

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

/// Cache failures surface as a non-5xx `Status`-like error so that `Kind::of` classifies them as `Other`.
fn cache_err(e: CacheError) -> ApiError {
    ApiError::Decode(format!("cache: {e}"))
}

struct Hydrated {
    stored: usize,
    /// Oldest `internal_date_ms` among the rows fetched.
    oldest: Option<i64>,
}

/// Fetch metadata for `ids` (the first [`FIRST_BATCH`] as their own batch, the rest in batches of 50) and store each
/// batch as soon as it arrives. Per-message failures are skipped (counted in the log only).
async fn hydrate<A: GmailApi, C: MailCache>(
    api: &A,
    cache: &C,
    ids: &[MessageId],
    mode: UpsertMode,
) -> Result<Hydrated, ApiError> {
    let mut out = Hydrated { stored: 0, oldest: None };
    if ids.is_empty() {
        return Ok(out);
    }
    let (first, rest) = ids.split_at(FIRST_BATCH.min(ids.len()));
    let mut batches: Vec<&[MessageId]> = vec![first];
    batches.extend(rest.chunks(BATCH_MAX));
    let mut skipped = 0usize;
    for batch in batches {
        let results = api.metadata_batch(batch).await?;
        let mut metas: Vec<MessageMeta> = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Ok(m) => metas.push(m),
                Err(_) => skipped += 1,
            }
        }
        if let Some(d) = metas.iter().map(|m| m.internal_date_ms).min() {
            out.oldest = Some(out.oldest.map_or(d, |o| o.min(d)));
        }
        if !metas.is_empty() {
            out.stored += cache.upsert_meta(&metas, now_ms(), mode).map_err(cache_err)?;
        }
    }
    tracing::debug!(requested = ids.len(), stored = out.stored, skipped, "hydrated");
    Ok(out)
}

/// One poll: `history.list` from the stored cursor, following `nextPageToken`; apply the events with
/// `MailCache::apply_events`; hydrate the returned ids with `metadata_batch` and `upsert_meta(.., Admit)`; advance
/// the cursor (`set_cursor(new, now_ms())`) only after everything succeeded. No cursor -> `cold_fill`.
/// `HistoryExpired` -> [`resync`] (report has `resynced = true`).
pub async fn poll<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<PollReport, ApiError> {
    let Some(cursor) = cache.cursor().map_err(cache_err)? else {
        cold_fill(api, cache).await?;
        return Ok(PollReport::default());
    };
    let mut report = PollReport::default();
    let mut to_fetch: Vec<MessageId> = Vec::new();
    let mut seen: HashSet<MessageId> = HashSet::new();
    let mut token: Option<String> = None;
    let new_cursor = loop {
        let page = match api.history(cursor, token.as_deref()).await {
            Ok(p) => p,
            Err(ApiError::HistoryExpired) => {
                tracing::info!("history expired; resyncing");
                resync(api, cache).await?;
                return Ok(PollReport { resynced: true, ..PollReport::default() });
            }
            Err(e) => return Err(e),
        };
        for ev in &page.events {
            match ev {
                HistoryEvent::Deleted { .. } => report.removed += 1,
                HistoryEvent::LabelsAdded { .. } | HistoryEvent::LabelsRemoved { .. } => report.changed += 1,
                HistoryEvent::Added { .. } => {}
            }
        }
        for id in cache.apply_events(&page.events).map_err(cache_err)? {
            if seen.insert(id.clone()) {
                to_fetch.push(id);
            }
        }
        match page.next_page_token {
            Some(t) => token = Some(t),
            None => break page.history_id,
        }
    };
    report.added = hydrate(api, cache, &to_fetch, UpsertMode::Admit).await?.stored;
    cache.set_cursor(new_cursor, now_ms()).map_err(cache_err)?;
    tracing::debug!(
        added = report.added,
        changed = report.changed,
        removed = report.removed,
        "poll done"
    );
    Ok(report)
}

/// First launch of an account: `profile` (for the cursor) BEFORE `list(INBOX, 50)`, then hydrate (first 20, then
/// the other 30 as two batches), upsert, set the INBOX window and the cursor.
pub async fn cold_fill<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<(), ApiError> {
    // The cursor is taken before the list: replaying events that the list already shows is harmless, missing
    // events between the two calls is not.
    let cursor = api.profile().await?.history_id;
    let inbox = LabelId::new(label::INBOX);
    let page = api.list(Some(&inbox), None, PAGE_SIZE).await?;
    let ids: Vec<MessageId> = page.ids.iter().map(|(id, _)| id.clone()).collect();
    let hydrated = hydrate(api, cache, &ids, UpsertMode::Always).await?;
    let now = now_ms();
    cache
        .set_window(&Window {
            label: inbox,
            oldest_date: hydrated.oldest.unwrap_or(i64::MAX),
            complete: page.next_page_token.is_none(),
            next_page_token: page.next_page_token,
            as_of: now,
        })
        .map_err(cache_err)?;
    cache.set_cursor(cursor, now).map_err(cache_err)?;
    tracing::debug!(rows = hydrated.stored, "cold fill done");
    Ok(())
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
    let existing = if more { cache.window(label).map_err(cache_err)? } else { None };
    let mut token: Option<String> = None;
    if let Some(w) = &existing {
        if w.complete {
            return Ok(());
        }
        token = w.next_page_token.clone();
    }
    let filter = (label.as_str() != mail_model::label::ALL_MAIL).then_some(label);
    let page = api.list(filter, token.as_deref(), PAGE_SIZE).await?;
    let listed: Vec<MessageId> = page.ids.iter().map(|(id, _)| id.clone()).collect();
    let cached = cache.has(&listed).map_err(cache_err)?;
    let cached_oldest = if cached.is_empty() {
        None
    } else {
        let rows = cache.rows(label, None, 10_000, true).map_err(cache_err)?;
        rows.iter().filter(|m| cached.contains(&m.id)).map(|m| m.internal_date_ms).min()
    };
    let missing: Vec<MessageId> = listed.into_iter().filter(|id| !cached.contains(id)).collect();
    let hydrated = hydrate(api, cache, &missing, UpsertMode::Always).await?;

    // Oldest date over the newly fetched rows and the already-cached listed rows.
    let known = [existing.as_ref().map(|w| w.oldest_date), hydrated.oldest, cached_oldest]
        .into_iter()
        .flatten()
        .min();
    cache
        .set_window(&Window {
            label: label.clone(),
            oldest_date: known.unwrap_or(i64::MAX),
            complete: page.next_page_token.is_none(),
            next_page_token: page.next_page_token,
            as_of: now_ms(),
        })
        .map_err(cache_err)?;
    tracing::debug!(label = %label, listed = page.ids.len(), fetched = hydrated.stored, "folder filled");
    Ok(())
}

/// History expired: `mark_all_stale_and_drop_windows`, take a new cursor from `profile`, refill INBOX.
pub async fn resync<A: GmailApi, C: MailCache>(api: &A, cache: &C) -> Result<(), ApiError> {
    let marked = cache.mark_all_stale_and_drop_windows().map_err(cache_err)?;
    tracing::info!(marked, "resync");
    // `cold_fill` takes the cursor from `profile` before listing, then refills INBOX.
    cold_fill(api, cache).await
}

/// Poll a set of futures to completion together, returning outputs in input order.
async fn join_all<F: Future>(mut futs: Vec<Pin<Box<F>>>) -> Vec<F::Output> {
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, fut) in out.iter_mut().zip(futs.iter_mut()) {
            if slot.is_none() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(v) => *slot = Some(v),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    out.into_iter().flatten().collect()
}

/// `labels.get` for each id with concurrency [`crate::COUNTS_CONCURRENCY`]. A label whose lookup fails yields
/// `LabelCounts::default()` (unknown); results are in the order of `labels`.
pub async fn folder_counts<A: GmailApi>(api: &A, labels: &[LabelId]) -> Vec<(LabelId, LabelCounts)> {
    let mut out = Vec::with_capacity(labels.len());
    for chunk in labels.chunks(COUNTS_CONCURRENCY) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|id| {
                Box::pin(async move {
                    let counts = api.label_counts(id).await.unwrap_or_default();
                    (id.clone(), counts)
                })
            })
            .collect();
        out.extend(join_all(futs).await);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use mail_model::{AuthError, HistoryId, HistoryPage, LabelInfo, ListPage, Profile, ThreadId};

    use super::*;

    type Log = Arc<Mutex<Vec<String>>>;

    fn mid(s: &str) -> MessageId {
        MessageId::new(s)
    }

    fn meta(id: &str, date: i64, labels: &[&str]) -> MessageMeta {
        MessageMeta {
            id: mid(id),
            thread_id: ThreadId::new(format!("t{id}")),
            label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
            internal_date_ms: date,
            size_estimate: 1,
            history_id: HistoryId(1),
            mime_type: "text/plain".into(),
            from_raw: String::new(),
            to_raw: String::new(),
            subject: String::new(),
            date_raw: String::new(),
            snippet_raw: String::new(),
        }
    }

    fn list_page(ids: &[String], next: Option<&str>) -> ListPage {
        ListPage {
            ids: ids.iter().map(|i| (mid(i), ThreadId::new(format!("t{i}")))).collect(),
            next_page_token: next.map(str::to_string),
        }
    }

    #[derive(Default)]
    struct ApiState {
        profile_hid: u64,
        /// Keyed by page token ("" = first page).
        lists: HashMap<String, ListPage>,
        metas: HashMap<MessageId, MessageMeta>,
        /// Keyed by page token ("" = first page).
        history: HashMap<String, Result<HistoryPage, ApiError>>,
        batch_error: Option<ApiError>,
        batch_sizes: Vec<usize>,
        list_tokens: Vec<Option<String>>,
        list_labels: Vec<Option<String>>,
    }

    struct FakeApi {
        log: Log,
        st: Mutex<ApiState>,
    }

    impl FakeApi {
        fn new(log: &Log) -> Self {
            FakeApi { log: log.clone(), st: Mutex::new(ApiState { profile_hid: 100, ..Default::default() }) }
        }
        fn note(&self, s: &str) {
            self.log.lock().unwrap().push(s.to_string());
        }
        fn with<R>(&self, f: impl FnOnce(&mut ApiState) -> R) -> R {
            f(&mut self.st.lock().unwrap())
        }
    }

    impl GmailApi for FakeApi {
        fn profile(&self) -> impl Future<Output = Result<Profile, ApiError>> + Send {
            self.note("profile");
            let hid = self.with(|s| s.profile_hid);
            async move { Ok(Profile { email: String::new(), messages_total: 0, history_id: HistoryId(hid) }) }
        }
        async fn labels(&self) -> Result<Vec<LabelInfo>, ApiError> {
            Ok(vec![])
        }
        fn label_counts(&self, id: &LabelId) -> impl Future<Output = Result<LabelCounts, ApiError>> + Send {
            let r = if id.as_str() == "BAD" {
                Err(ApiError::NotFound)
            } else {
                Ok(LabelCounts { total: Some(id.as_str().len() as u32), unread: Some(1) })
            };
            async move { r }
        }
        fn list(
            &self,
            label: Option<&LabelId>,
            page_token: Option<&str>,
            _max: u32,
        ) -> impl Future<Output = Result<ListPage, ApiError>> + Send {
            self.note("list");
            let r = self.with(|s| {
                s.list_tokens.push(page_token.map(str::to_string));
                s.list_labels.push(label.map(|l| l.as_str().to_string()));
                s.lists.get(page_token.unwrap_or("")).cloned().ok_or(ApiError::NotFound)
            });
            async move { r }
        }
        fn metadata_batch(
            &self,
            ids: &[MessageId],
        ) -> impl Future<Output = Result<Vec<Result<MessageMeta, ApiError>>, ApiError>> + Send {
            self.note("batch");
            let r = self.with(|s| {
                s.batch_sizes.push(ids.len());
                if let Some(e) = s.batch_error.clone() {
                    return Err(e);
                }
                Ok(ids.iter().map(|i| s.metas.get(i).cloned().ok_or(ApiError::NotFound)).collect())
            });
            async move { r }
        }
        async fn raw(&self, _id: &MessageId) -> Result<Vec<u8>, ApiError> {
            Err(ApiError::NotFound)
        }
        fn history(
            &self,
            _start: HistoryId,
            page_token: Option<&str>,
        ) -> impl Future<Output = Result<HistoryPage, ApiError>> + Send {
            self.note("history");
            let r = self.with(|s| s.history.get(page_token.unwrap_or("")).cloned().unwrap_or(Err(ApiError::NotFound)));
            async move { r }
        }
    }

    #[derive(Default)]
    struct CacheState {
        cursor: Option<HistoryId>,
        synced_at: Option<i64>,
        rows: HashMap<MessageId, MessageMeta>,
        windows: HashMap<LabelId, Window>,
        apply_returns: Vec<MessageId>,
        applied: Vec<HistoryEvent>,
        fail_apply: bool,
        stale_marked: usize,
        modes: Vec<UpsertMode>,
    }

    struct FakeCache {
        log: Log,
        st: Mutex<CacheState>,
    }

    impl FakeCache {
        fn new(log: &Log) -> Self {
            FakeCache { log: log.clone(), st: Mutex::new(CacheState::default()) }
        }
        fn with<R>(&self, f: impl FnOnce(&mut CacheState) -> R) -> R {
            f(&mut self.st.lock().unwrap())
        }
    }

    impl MailCache for FakeCache {
        fn cursor(&self) -> Result<Option<HistoryId>, CacheError> {
            Ok(self.with(|s| s.cursor))
        }
        fn set_cursor(&self, cursor: HistoryId, synced_at_ms: i64) -> Result<(), CacheError> {
            self.log.lock().unwrap().push("set_cursor".into());
            self.with(|s| {
                s.cursor = Some(cursor);
                s.synced_at = Some(synced_at_ms);
            });
            Ok(())
        }
        fn upsert_meta(&self, metas: &[MessageMeta], _now: i64, mode: UpsertMode) -> Result<usize, CacheError> {
            self.with(|s| {
                s.modes.push(mode);
                for m in metas {
                    s.rows.insert(m.id.clone(), m.clone());
                }
            });
            Ok(metas.len())
        }
        fn rows(&self, l: &LabelId, _b: Option<i64>, _n: usize, _s: bool) -> Result<Vec<MessageMeta>, CacheError> {
            Ok(self.with(|s| {
                s.rows
                    .values()
                    .filter(|m| l.as_str() == "ALL" || m.label_ids.contains(l))
                    .cloned()
                    .collect()
            }))
        }
        fn has(&self, ids: &[MessageId]) -> Result<HashSet<MessageId>, CacheError> {
            Ok(self.with(|s| ids.iter().filter(|i| s.rows.contains_key(*i)).cloned().collect()))
        }
        fn apply_events(&self, events: &[HistoryEvent]) -> Result<Vec<MessageId>, CacheError> {
            self.with(|s| {
                if s.fail_apply {
                    return Err(CacheError::Db("boom".into()));
                }
                s.applied.extend(events.iter().cloned());
                Ok(std::mem::take(&mut s.apply_returns))
            })
        }
        fn window(&self, l: &LabelId) -> Result<Option<Window>, CacheError> {
            Ok(self.with(|s| s.windows.get(l).cloned()))
        }
        fn set_window(&self, w: &Window) -> Result<(), CacheError> {
            self.with(|s| s.windows.insert(w.label.clone(), w.clone()));
            Ok(())
        }
        fn mark_all_stale_and_drop_windows(&self) -> Result<usize, CacheError> {
            Ok(self.with(|s| {
                s.windows.clear();
                s.stale_marked = s.rows.len();
                s.rows.len()
            }))
        }
        fn body(&self, _id: &MessageId) -> Result<Option<Vec<u8>>, CacheError> {
            Ok(None)
        }
        fn put_body(&self, _id: &MessageId, _raw: &[u8], _now: i64) -> Result<bool, CacheError> {
            Ok(true)
        }
        fn touch_opened(&self, _id: &MessageId, _now: i64) -> Result<(), CacheError> {
            Ok(())
        }
        fn label_ids(&self, _id: &MessageId) -> Result<Option<Vec<LabelId>>, CacheError> {
            Ok(None)
        }
        fn evict(&self, _now: i64) -> Result<mail_model::EvictReport, CacheError> {
            Ok(Default::default())
        }
        fn synced_at(&self) -> Result<Option<i64>, CacheError> {
            Ok(self.with(|s| s.synced_at))
        }
    }

    fn setup() -> (FakeApi, FakeCache, Log) {
        let log: Log = Arc::default();
        (FakeApi::new(&log), FakeCache::new(&log), log)
    }

    /// `n` ids `m0..m{n-1}` with metadata (dates descending from 1000) registered on the API.
    fn seed_ids(api: &FakeApi, n: usize) -> Vec<String> {
        let ids: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
        api.with(|s| {
            for (i, id) in ids.iter().enumerate() {
                s.metas.insert(mid(id), meta(id, 1000 - i as i64, &["INBOX"]));
            }
        });
        ids
    }

    fn position(log: &Log, name: &str) -> usize {
        log.lock().unwrap().iter().position(|e| e == name).unwrap_or_else(|| panic!("{name} not called"))
    }

    #[tokio::test]
    async fn cold_fill_takes_cursor_before_list_and_splits_batches() {
        let (api, cache, log) = setup();
        let ids = seed_ids(&api, 50);
        api.with(|s| {
            s.profile_hid = 777;
            s.lists.insert(String::new(), list_page(&ids, Some("next")));
        });
        cold_fill(&api, &cache).await.unwrap();
        assert!(position(&log, "profile") < position(&log, "list"));
        assert!(position(&log, "list") < position(&log, "batch"));
        assert!(position(&log, "batch") < position(&log, "set_cursor"));
        assert_eq!(api.with(|s| s.batch_sizes.clone()), vec![20, 30]);
        assert_eq!(api.with(|s| s.list_labels.clone()), vec![Some("INBOX".to_string())]);
        cache.with(|s| {
            assert_eq!(s.cursor, Some(HistoryId(777)));
            assert_eq!(s.rows.len(), 50);
            assert!(s.modes.iter().all(|m| *m == UpsertMode::Always));
            let w = &s.windows[&LabelId::new("INBOX")];
            assert!(!w.complete);
            assert_eq!(w.next_page_token.as_deref(), Some("next"));
            assert_eq!(w.oldest_date, 951);
        });
    }

    #[tokio::test]
    async fn per_message_errors_are_skipped() {
        let (api, cache, _log) = setup();
        let mut ids = seed_ids(&api, 3);
        ids.push("gone".into());
        api.with(|s| s.lists.insert(String::new(), list_page(&ids, None)));
        cold_fill(&api, &cache).await.unwrap();
        cache.with(|s| {
            assert_eq!(s.rows.len(), 3);
            assert!(s.windows[&LabelId::new("INBOX")].complete);
        });
    }

    #[tokio::test]
    async fn poll_without_cursor_cold_fills() {
        let (api, cache, _log) = setup();
        let ids = seed_ids(&api, 2);
        api.with(|s| s.lists.insert(String::new(), list_page(&ids, None)));
        let r = poll(&api, &cache).await.unwrap();
        assert_eq!(r, PollReport::default());
        assert_eq!(cache.with(|s| s.cursor), Some(HistoryId(100)));
    }

    fn hist(events: Vec<HistoryEvent>, hid: u64, next: Option<&str>) -> Result<HistoryPage, ApiError> {
        Ok(HistoryPage { events, history_id: HistoryId(hid), next_page_token: next.map(str::to_string) })
    }

    #[tokio::test]
    async fn poll_follows_pages_hydrates_and_advances_cursor() {
        let (api, cache, log) = setup();
        seed_ids(&api, 2);
        cache.with(|s| {
            s.cursor = Some(HistoryId(50));
            s.apply_returns = vec![mid("m0"), mid("m1")];
        });
        api.with(|s| {
            s.history.insert(
                String::new(),
                hist(
                    vec![
                        HistoryEvent::Added { id: mid("m0"), thread_id: ThreadId::new("t"), label_ids: vec![] },
                        HistoryEvent::Deleted { id: mid("old") },
                    ],
                    60,
                    Some("p2"),
                ),
            );
            s.history.insert(
                "p2".into(),
                hist(
                    vec![HistoryEvent::LabelsAdded { id: mid("x"), label_ids: vec![LabelId::new("STARRED")] }],
                    70,
                    None,
                ),
            );
        });
        let r = poll(&api, &cache).await.unwrap();
        assert_eq!(r, PollReport { added: 2, changed: 1, removed: 1, resynced: false });
        cache.with(|s| {
            assert_eq!(s.cursor, Some(HistoryId(70)));
            assert_eq!(s.applied.len(), 3);
            assert_eq!(s.modes, vec![UpsertMode::Admit]);
        });
        assert!(position(&log, "batch") < position(&log, "set_cursor"));
    }

    #[tokio::test]
    async fn poll_idle_advances_cursor_without_hydrating() {
        let (api, cache, log) = setup();
        cache.with(|s| s.cursor = Some(HistoryId(50)));
        api.with(|s| s.history.insert(String::new(), hist(vec![], 55, None)));
        let r = poll(&api, &cache).await.unwrap();
        assert_eq!(r, PollReport::default());
        assert_eq!(cache.with(|s| s.cursor), Some(HistoryId(55)));
        assert!(!log.lock().unwrap().iter().any(|e| e == "batch"));
    }

    #[tokio::test]
    async fn cursor_not_advanced_when_apply_fails() {
        let (api, cache, _log) = setup();
        cache.with(|s| {
            s.cursor = Some(HistoryId(50));
            s.fail_apply = true;
        });
        api.with(|s| s.history.insert(String::new(), hist(vec![], 60, None)));
        assert!(poll(&api, &cache).await.is_err());
        assert_eq!(cache.with(|s| s.cursor), Some(HistoryId(50)));
    }

    #[tokio::test]
    async fn cursor_not_advanced_when_hydrate_fails() {
        let (api, cache, _log) = setup();
        cache.with(|s| {
            s.cursor = Some(HistoryId(50));
            s.apply_returns = vec![mid("m0")];
        });
        api.with(|s| {
            s.history.insert(String::new(), hist(vec![], 60, None));
            s.batch_error = Some(ApiError::Offline("down".into()));
        });
        let e = poll(&api, &cache).await.unwrap_err();
        assert_eq!(crate::Kind::of(&e), crate::Kind::Offline);
        assert_eq!(cache.with(|s| s.cursor), Some(HistoryId(50)));
    }

    #[tokio::test]
    async fn history_404_resyncs() {
        let (api, cache, log) = setup();
        let ids = seed_ids(&api, 3);
        cache.with(|s| {
            s.cursor = Some(HistoryId(1));
            s.rows.insert(mid("old"), meta("old", 5, &["INBOX"]));
            s.windows.insert(
                LabelId::new("SENT"),
                Window { label: LabelId::new("SENT"), oldest_date: 0, complete: true, next_page_token: None, as_of: 0 },
            );
        });
        api.with(|s| {
            s.profile_hid = 900;
            s.history.insert(String::new(), Err(ApiError::HistoryExpired));
            s.lists.insert(String::new(), list_page(&ids, None));
        });
        let r = poll(&api, &cache).await.unwrap();
        assert!(r.resynced);
        cache.with(|s| {
            assert_eq!(s.stale_marked, 1);
            assert_eq!(s.cursor, Some(HistoryId(900)));
            assert!(!s.windows.contains_key(&LabelId::new("SENT")));
            assert!(s.windows.contains_key(&LabelId::new("INBOX")));
        });
        assert!(position(&log, "history") < position(&log, "profile"));
    }

    #[tokio::test]
    async fn needs_reauth_error_classifies_as_stop() {
        let (api, cache, _log) = setup();
        cache.with(|s| s.cursor = Some(HistoryId(1)));
        api.with(|s| {
            s.history.insert(String::new(), Err(ApiError::Auth(AuthError::NeedsReauth { reason: "x".into() })));
        });
        let e = poll(&api, &cache).await.unwrap_err();
        let kind = crate::Kind::of(&e);
        assert_eq!(kind, crate::Kind::NeedsReauth);
        let mut b = crate::Backoff::default();
        let plan = b.next(kind, std::time::Duration::from_secs(5));
        assert_eq!(plan.delay, None);
        assert_eq!(plan.ui, crate::Ui::NeedsReauth);
        assert_eq!(cache.with(|s| s.cursor), Some(HistoryId(1)));
    }

    #[tokio::test]
    async fn fill_folder_more_continues_and_completes() {
        let (api, cache, _log) = setup();
        let ids = seed_ids(&api, 80);
        let sent = LabelId::new("SENT");
        api.with(|s| {
            s.lists.insert(String::new(), list_page(&ids[..50], Some("t2")));
            s.lists.insert("t2".into(), list_page(&ids[50..], None));
        });
        fill_folder(&api, &cache, &sent, false).await.unwrap();
        cache.with(|s| {
            let w = &s.windows[&sent];
            assert!(!w.complete);
            assert_eq!(w.next_page_token.as_deref(), Some("t2"));
            assert_eq!(w.oldest_date, 951);
        });
        fill_folder(&api, &cache, &sent, true).await.unwrap();
        assert_eq!(api.with(|s| s.list_tokens.clone()), vec![None, Some("t2".to_string())]);
        cache.with(|s| {
            let w = &s.windows[&sent];
            assert!(w.complete);
            assert_eq!(w.next_page_token, None);
            assert_eq!(w.oldest_date, 921);
            assert_eq!(s.rows.len(), 80);
        });
        // Complete: further `more` is a no-op.
        fill_folder(&api, &cache, &sent, true).await.unwrap();
        assert_eq!(api.with(|s| s.list_tokens.len()), 2);
    }

    #[tokio::test]
    async fn fill_folder_skips_cached_ids_and_all_mail_has_no_filter() {
        let (api, cache, _log) = setup();
        let ids = seed_ids(&api, 25);
        cache.with(|s| {
            for id in &ids[..5] {
                s.rows.insert(mid(id), meta(id, 1, &[]));
            }
        });
        api.with(|s| s.lists.insert(String::new(), list_page(&ids, None)));
        fill_folder(&api, &cache, &LabelId::new("ALL"), false).await.unwrap();
        assert_eq!(api.with(|s| s.list_labels.clone()), vec![None]);
        // 20 uncached ids: a single first batch.
        assert_eq!(api.with(|s| s.batch_sizes.clone()), vec![20]);
        assert!(cache.with(|s| s.windows[&LabelId::new("ALL")].complete));
    }

    #[tokio::test]
    async fn fill_folder_all_cached_page_keeps_real_oldest_date() {
        let (api, cache, _log) = setup();
        let ids = seed_ids(&api, 30);
        let inbox = LabelId::new("INBOX");
        api.with(|s| s.lists.insert(String::new(), list_page(&ids, Some("t2"))));
        fill_folder(&api, &cache, &inbox, false).await.unwrap();
        let batches = api.with(|s| s.batch_sizes.len());
        // Second fill: every id is cached, nothing is fetched, the window still reaches back to the oldest row.
        fill_folder(&api, &cache, &inbox, false).await.unwrap();
        assert_eq!(api.with(|s| s.batch_sizes.len()), batches);
        assert_eq!(cache.with(|s| s.windows[&inbox].oldest_date), 971);
        // Same for All Mail.
        let all = LabelId::new("ALL");
        fill_folder(&api, &cache, &all, false).await.unwrap();
        assert_eq!(cache.with(|s| s.windows[&all].oldest_date), 971);
    }

    #[tokio::test]
    async fn fill_folder_failure_leaves_window_untouched() {
        let (api, cache, _log) = setup();
        let ids = seed_ids(&api, 3);
        api.with(|s| {
            s.lists.insert(String::new(), list_page(&ids, None));
            s.batch_error = Some(ApiError::RateLimited { retry_after_s: None });
        });
        assert!(fill_folder(&api, &cache, &LabelId::new("SPAM"), false).await.is_err());
        assert!(cache.with(|s| s.windows.is_empty()));
    }

    #[tokio::test]
    async fn folder_counts_order_and_errors() {
        let (api, _cache, _log) = setup();
        let labels: Vec<LabelId> = (0..20)
            .map(|i| LabelId::new(if i == 3 { "BAD".to_string() } else { "L".repeat(i + 1) }))
            .collect();
        let got = folder_counts(&api, &labels).await;
        assert_eq!(got.len(), 20);
        for (i, (id, c)) in got.iter().enumerate() {
            assert_eq!(id, &labels[i]);
            if i == 3 {
                assert_eq!(*c, LabelCounts::default());
            } else {
                assert_eq!(c.total, Some(i as u32 + 1));
            }
        }
    }
}
