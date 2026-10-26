//! A pass-through `MailCache` that records what a sync call touched, so the service can emit an accurate
//! `mail-changed` (which label lists changed, and whether any row was stored).

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use mail_model::{
    CacheError, EvictReport, HistoryEvent, HistoryId, LabelId, MailCache, MessageId, MessageMeta, UpsertMode, Window,
};

pub(crate) struct Tap<'a, C: MailCache> {
    inner: &'a C,
    touched: Mutex<BTreeSet<String>>,
    stored: AtomicUsize,
}

impl<'a, C: MailCache> Tap<'a, C> {
    pub(crate) fn new(inner: &'a C) -> Self {
        Tap { inner, touched: Mutex::new(BTreeSet::new()), stored: AtomicUsize::new(0) }
    }

    /// Label ids seen in events and stored rows.
    pub(crate) fn touched(&self) -> BTreeSet<String> {
        self.touched.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Rows stored through `upsert_meta`.
    pub(crate) fn stored(&self) -> usize {
        self.stored.load(Ordering::Relaxed)
    }

    fn touch<'l>(&self, labels: impl IntoIterator<Item = &'l LabelId>) {
        let mut g = self.touched.lock().unwrap_or_else(|e| e.into_inner());
        for l in labels {
            g.insert(l.0.clone());
        }
    }
}

impl<C: MailCache> MailCache for Tap<'_, C> {
    fn cursor(&self) -> Result<Option<HistoryId>, CacheError> {
        self.inner.cursor()
    }

    fn set_cursor(&self, cursor: HistoryId, synced_at_ms: i64) -> Result<(), CacheError> {
        self.inner.set_cursor(cursor, synced_at_ms)
    }

    fn upsert_meta(&self, metas: &[MessageMeta], now_ms: i64, mode: UpsertMode) -> Result<usize, CacheError> {
        let n = self.inner.upsert_meta(metas, now_ms, mode)?;
        if n > 0 {
            self.stored.fetch_add(n, Ordering::Relaxed);
            for m in metas {
                self.touch(&m.label_ids);
            }
        }
        Ok(n)
    }

    fn rows(
        &self,
        label: &LabelId,
        before: Option<i64>,
        limit: usize,
        include_stale: bool,
    ) -> Result<Vec<MessageMeta>, CacheError> {
        self.inner.rows(label, before, limit, include_stale)
    }

    fn has(&self, ids: &[MessageId]) -> Result<HashSet<MessageId>, CacheError> {
        self.inner.has(ids)
    }

    fn apply_events(&self, events: &[HistoryEvent]) -> Result<Vec<MessageId>, CacheError> {
        for ev in events {
            match ev {
                HistoryEvent::Added { label_ids, .. }
                | HistoryEvent::LabelsAdded { label_ids, .. }
                | HistoryEvent::LabelsRemoved { label_ids, .. } => self.touch(label_ids),
                HistoryEvent::Deleted { id } => {
                    if let Ok(Some(labels)) = self.inner.label_ids(id) {
                        self.touch(&labels);
                    }
                }
            }
        }
        self.inner.apply_events(events)
    }

    fn window(&self, label: &LabelId) -> Result<Option<Window>, CacheError> {
        self.inner.window(label)
    }

    fn set_window(&self, window: &Window) -> Result<(), CacheError> {
        self.inner.set_window(window)
    }

    fn mark_all_stale_and_drop_windows(&self) -> Result<usize, CacheError> {
        let n = self.inner.mark_all_stale_and_drop_windows()?;
        if n > 0 {
            self.stored.fetch_add(n, Ordering::Relaxed);
        }
        Ok(n)
    }

    fn body(&self, id: &MessageId) -> Result<Option<Vec<u8>>, CacheError> {
        self.inner.body(id)
    }

    fn put_body(&self, id: &MessageId, raw: &[u8], now_ms: i64) -> Result<bool, CacheError> {
        self.inner.put_body(id, raw, now_ms)
    }

    fn touch_opened(&self, id: &MessageId, now_ms: i64) -> Result<(), CacheError> {
        self.inner.touch_opened(id, now_ms)
    }

    fn meta(&self, id: &MessageId) -> Result<Option<(MessageMeta, bool)>, CacheError> {
        self.inner.meta(id)
    }

    fn label_ids(&self, id: &MessageId) -> Result<Option<Vec<LabelId>>, CacheError> {
        self.inner.label_ids(id)
    }

    fn evict(&self, now_ms: i64) -> Result<EvictReport, CacheError> {
        self.inner.evict(now_ms)
    }

    fn synced_at(&self) -> Result<Option<i64>, CacheError> {
        self.inner.synced_at()
    }
}
