//! The machinery behind [`crate::Service`]: per-account runtime state, the poll loop, fills, sign-in and sign-out.
//!
//! Privacy: log ids, counts and timings only.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gauth::{Client, LoginHandle, RevokeOutcome, Store, StoredAccount, Tokens};
use mail_cache::{AccountCache, CacheRoot};
use mail_model::{
    AccountId, AccountState, AccountView, ApiError, AttachmentInfo, AuthError, CmdError, CmdErrorCode, FolderKind,
    FolderView, GmailApi, LabelCounts, LabelId, LabelInfo, LabelKind, ListResult, MailCache, MailChanged, MessageId, HistoryEvent, MessageMeta, UpsertMode,
    MessageView, PartId, SaveResult, SignInProgress, SignInStage, SignOutResult, SyncState, SyncStatus, WindowState,
    label, now_ms,
};
use mail_render::{ImagePolicy, Parsed};
use mail_sync::{Backoff, Cadence, Decision, Kind, PollGate, Trigger, Ui};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::api::DynApi;
use crate::tap::Tap;
use crate::{ApiFactory, Asset, EventSink, FileSaver, ServiceEvent};

/// Rows per page of `messages`.
const PAGE: usize = 50;
/// Label list refresh interval.
const LABELS_TTL: Duration = Duration::from_secs(300);
/// Minimum gap between count refreshes per account.
const COUNTS_TTL: Duration = Duration::from_secs(60);
/// A folder window refreshed this recently is not refilled when the folder is opened again.
const FILL_FRESH_MS: i64 = 30_000;
/// Cache eviction at most this often per account.
const EVICT_EVERY: Duration = Duration::from_secs(600);
/// Parsed messages kept for `asset` and `save_attachment`.
const PARSED_LRU: usize = 6;
/// A loop never spins faster than this.
const MIN_GAP: Duration = Duration::from_millis(20);

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Mem {
    state: SyncState,
    labels: Option<(Vec<LabelInfo>, Instant)>,
    counts: HashMap<LabelId, LabelCounts>,
    counts_basic_at: Option<Instant>,
    counts_full_at: Option<Instant>,
    fills: HashSet<String>,
    shown: HashMap<String, usize>,
    view_folder: Option<String>,
    last_end: Instant,
    backoff_delay: Option<Duration>,
    next_at: Instant,
    last_evict: Option<Instant>,
}

/// One signed-in account at runtime.
pub(crate) struct Acct {
    pub(crate) id: AccountId,
    pub(crate) email: String,
    pub(crate) cache: AccountCache,
    api: DynApi,
    tokens: Tokens,
    needs_reauth: AtomicBool,
    stopped: AtomicBool,
    stop: Notify,
    poll_now: Notify,
    reschedule: Notify,
    gate: Mutex<PollGate>,
    backoff: Mutex<Backoff>,
    mem: Mutex<Mem>,
    tasks: Mutex<JoinSet<()>>,
}

impl Acct {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn needs_reauth(&self) -> bool {
        self.needs_reauth.load(Ordering::SeqCst)
    }

    fn state(&self) -> SyncState {
        lock(&self.mem).state
    }

    fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let mut t = lock(&self.tasks);
        while t.try_join_next().is_some() {}
        t.spawn(fut);
    }

    fn halt(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }
}

struct Shared {
    window: WindowState,
    active: Option<AccountId>,
    accounts: Vec<Arc<Acct>>,
    login: Option<LoginHandle>,
}

pub(crate) struct Inner {
    sink: Arc<dyn EventSink>,
    store: Store,
    cache_root: CacheRoot,
    client: Option<Client>,
    factory: ApiFactory,
    cadence: Cadence,
    login_timeout: Duration,
    st: Mutex<Shared>,
    parsed: Mutex<Vec<ParsedEntry>>,
}

type ParsedEntry = ((AccountId, MessageId), Arc<Parsed>);

/// Removes a folder from the in-flight set when dropped.
struct FillGuard {
    acct: Arc<Acct>,
    folder: String,
}

impl FillGuard {
    fn try_new(acct: &Arc<Acct>, folder: &LabelId) -> Option<FillGuard> {
        lock(&acct.mem)
            .fills
            .insert(folder.0.clone())
            .then(|| FillGuard { acct: acct.clone(), folder: folder.0.clone() })
    }
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        lock(&self.acct.mem).fills.remove(&self.folder);
    }
}

fn rand_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(now_ms());
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

fn folder_label(folder_id: &str) -> Result<LabelId, CmdError> {
    if folder_id.is_empty() || folder_id.starts_with(label::GROUP_PREFIX) {
        return Err(CmdError::not_found("not a folder"));
    }
    Ok(LabelId::new(folder_id))
}

fn system_label_name(id: &str) -> Option<&'static str> {
    Some(match id {
        label::INBOX => "Inbox",
        label::STARRED => "Starred",
        label::SENT => "Sent",
        label::DRAFT => "Drafts",
        label::SPAM => "Spam",
        label::TRASH => "Trash",
        label::CATEGORY_PERSONAL => "Personal",
        label::CATEGORY_UPDATES => "Updates",
        label::CATEGORY_PROMOTIONS => "Promotions",
        label::CATEGORY_SOCIAL => "Social",
        label::CATEGORY_FORUMS => "Forums",
        _ => return None,
    })
}

fn window_shape(w: &Option<mail_model::Window>) -> Option<(i64, bool, Option<String>)> {
    w.as_ref().map(|w| (w.oldest_date, w.complete, w.next_page_token.clone()))
}

fn is_auth_dead(e: &ApiError) -> bool {
    matches!(e, ApiError::Auth(AuthError::NeedsReauth { .. } | AuthError::MissingScope { .. } | AuthError::NotSignedIn))
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        sink: Arc<dyn EventSink>,
        store: Store,
        cache_root: CacheRoot,
        client: Option<Client>,
        factory: ApiFactory,
        cadence: Cadence,
        login_timeout: Duration,
    ) -> Arc<Inner> {
        Arc::new(Inner {
            sink,
            store,
            cache_root,
            client,
            factory,
            cadence,
            login_timeout,
            st: Mutex::new(Shared { window: WindowState::Focused, active: None, accounts: Vec::new(), login: None }),
            parsed: Mutex::new(Vec::new()),
        })
    }

    fn emit(&self, e: ServiceEvent) {
        self.sink.emit(e);
    }

    fn progress(&self, stage: SignInStage, message: Option<String>) {
        self.emit(ServiceEvent::SignInProgress(SignInProgress { stage, message }));
    }

    pub(crate) fn get(&self, id: &AccountId) -> Result<Arc<Acct>, CmdError> {
        lock(&self.st)
            .accounts
            .iter()
            .find(|a| &a.id == id)
            .cloned()
            .ok_or_else(|| CmdError::not_found("unknown account"))
    }

    fn all(&self) -> Vec<Arc<Acct>> {
        lock(&self.st).accounts.clone()
    }

    fn view(&self, acct: &Acct) -> AccountView {
        let m = lock(&acct.mem);
        let unread = |l: &str| m.counts.get(&LabelId::new(l)).and_then(|c| c.unread);
        AccountView {
            id: acct.id.clone(),
            email: acct.email.clone(),
            state: if acct.needs_reauth() { AccountState::NeedsReauth } else { AccountState::SignedIn },
            inbox_unread: unread(label::INBOX),
            spam_unread: unread(label::SPAM),
        }
    }

    fn status_of(&self, acct: &Acct) -> SyncStatus {
        SyncStatus {
            account_id: acct.id.clone(),
            state: acct.state(),
            as_of: acct.cache.synced_at().ok().flatten(),
        }
    }

    fn set_sync(&self, acct: &Acct, state: SyncState, force: bool) {
        let changed = {
            let mut m = lock(&acct.mem);
            let c = m.state != state;
            m.state = state;
            c
        };
        if changed || force {
            self.emit(ServiceEvent::SyncStatus(self.status_of(acct)));
        }
    }

    fn interval(&self, id: &AccountId) -> Duration {
        let s = lock(&self.st);
        self.cadence.interval(s.window, s.active.as_ref() == Some(id))
    }

    fn schedule(&self, acct: &Acct) {
        let base = self.interval(&acct.id);
        let mut m = lock(&acct.mem);
        let delay = self.cadence.jittered(m.backoff_delay.unwrap_or(base), rand_unit());
        m.next_at = m.last_end + delay;
    }

    // ---------------------------------------------------------------- accounts

    /// Open the cache, tokens and API of a stored account and (unless it needs a new sign-in) start its poll task.
    fn install(self: &Arc<Self>, stored: &StoredAccount) -> Result<Arc<Acct>, CmdError> {
        let client = self
            .client
            .clone()
            .ok_or_else(|| CmdError::from(AuthError::Config("the OAuth client file is missing".into())))?;
        let id = stored.id();
        let cache = self.cache_root.account(&id)?;
        let tokens = Tokens::new(client, self.store.clone(), stored);
        let api = (self.factory)(&tokens);
        let now = Instant::now();
        let acct = Arc::new(Acct {
            id,
            email: stored.email.clone(),
            cache,
            api,
            tokens,
            needs_reauth: AtomicBool::new(stored.needs_reauth),
            stopped: AtomicBool::new(stored.needs_reauth),
            stop: Notify::new(),
            poll_now: Notify::new(),
            reschedule: Notify::new(),
            gate: Mutex::new(PollGate::default()),
            backoff: Mutex::new(Backoff::default()),
            mem: Mutex::new(Mem {
                state: if stored.needs_reauth { SyncState::NeedsReauth } else { SyncState::Syncing },
                labels: None,
                counts: HashMap::new(),
                counts_basic_at: None,
                counts_full_at: None,
                fills: HashSet::new(),
                shown: HashMap::new(),
                view_folder: None,
                last_end: now,
                backoff_delay: None,
                next_at: now,
                last_evict: None,
            }),
            tasks: Mutex::new(JoinSet::new()),
        });
        lock(&self.st).accounts.push(acct.clone());
        if !stored.needs_reauth {
            let (inner, a) = (self.clone(), acct.clone());
            acct.spawn(async move { poll_loop(inner, a).await });
        }
        Ok(acct)
    }

    /// Load every readable stored account. Called once from `open`.
    pub(crate) fn load_accounts(self: &Arc<Self>) {
        if self.client.is_none() {
            return;
        }
        let (stored, skipped) = self.store.list();
        tracing::info!(accounts = stored.len(), skipped, "accounts loaded");
        for s in &stored {
            if let Err(e) = self.install(s) {
                tracing::warn!(error = %e, "account could not be started");
            }
        }
    }

    /// Stop an account's tasks and close its cache. `acct` must already be removed from `Shared`. Returns `false`
    /// if some task still holds the account after a while (the cache then closes when the last holder is gone).
    async fn teardown(&self, acct: Arc<Acct>) -> bool {
        acct.halt();
        lock(&acct.tasks).abort_all();
        let mut a = acct;
        for _ in 0..200 {
            match Arc::try_unwrap(a) {
                Ok(owned) => {
                    if let Err(e) = owned.cache.close() {
                        tracing::warn!(error = %e, "cache close failed");
                    }
                    return true;
                }
                Err(back) => {
                    a = back;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        tracing::warn!("account tasks still running at teardown");
        false
    }

    pub(crate) async fn shutdown(&self) {
        let accts: Vec<Arc<Acct>> = std::mem::take(&mut lock(&self.st).accounts);
        for a in accts {
            self.teardown(a).await;
        }
    }

    pub(crate) fn set_window_state(&self, state: WindowState) {
        let prev = std::mem::replace(&mut lock(&self.st).window, state);
        for a in self.all() {
            let restored = (prev == WindowState::Minimized && state != WindowState::Minimized)
                || (state == WindowState::Focused && prev != WindowState::Focused);
            if restored {
                a.poll_now.notify_one();
            } else {
                self.schedule(&a);
                a.reschedule.notify_one();
            }
        }
    }

    pub(crate) fn set_view(&self, id: &AccountId, folder_id: String) -> Result<(), CmdError> {
        let acct = self.get(id)?;
        lock(&acct.mem).view_folder = Some(folder_id);
        let prev = lock(&self.st).active.replace(id.clone());
        if prev.as_ref() != Some(id) {
            for a in self.all() {
                if a.id == *id || Some(&a.id) == prev.as_ref() {
                    self.schedule(&a);
                    a.reschedule.notify_one();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn accounts(&self) -> Vec<AccountView> {
        self.all().iter().map(|a| self.view(a)).collect()
    }

    pub(crate) fn status(&self) -> Vec<SyncStatus> {
        self.all().iter().map(|a| self.status_of(a)).collect()
    }

    /// A dead refresh token or a missing scope was seen: persist the flag, stop polling, tell the UI.
    fn mark_needs_reauth(&self, acct: &Acct) {
        if acct.needs_reauth.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.store.set_needs_reauth(&acct.id, true) {
            tracing::warn!(error = %e, "could not persist needs_reauth");
        }
        acct.halt();
        lock(&acct.mem).state = SyncState::NeedsReauth;
        tracing::info!("account needs a new sign-in");
        self.emit(ServiceEvent::AccountChanged(self.view(acct)));
        self.emit(ServiceEvent::SyncStatus(self.status_of(acct)));
    }

    fn api_err(&self, acct: &Acct, e: ApiError) -> CmdError {
        if is_auth_dead(&e) {
            self.mark_needs_reauth(acct);
        }
        e.into()
    }

    // ---------------------------------------------------------------- sign-in / sign-out

    pub(crate) async fn sign_in(
        self: &Arc<Self>,
        hint: Option<String>,
        expect: Option<AccountId>,
    ) -> Result<AccountView, CmdError> {
        let Some(client) = self.client.clone() else {
            let e: CmdError = AuthError::Config(
                "oauth-client.json was not found in the app data folder; copy your OAuth client file there".into(),
            )
            .into();
            self.progress(SignInStage::Error, Some(e.message.clone()));
            return Err(e);
        };
        let pending = match gauth::begin_login(&client, hint.as_deref()).await {
            Ok(p) => p,
            Err(AuthError::AlreadyPending) => return Err(AuthError::AlreadyPending.into()),
            Err(e) => return Err(self.sign_in_failed(e)),
        };
        let handle = pending.handle();
        lock(&self.st).login = Some(handle.clone());
        self.progress(SignInStage::Browser, None);
        if let Err(e) = handle.reopen_browser() {
            lock(&self.st).login = None;
            return Err(self.sign_in_failed(e));
        }
        self.progress(SignInStage::Waiting, None);
        let res = gauth::complete_login(pending, self.login_timeout).await;
        lock(&self.st).login = None;
        let stored = match res {
            Ok(s) => s,
            Err(e) => return Err(self.sign_in_failed(e)),
        };
        if let Some(expected) = &expect
            && stored.id() != *expected
        {
            let e = CmdError::new(CmdErrorCode::Auth, "signed in as a different Google account").with_detail("wrong_account");
            self.progress(SignInStage::Error, Some(e.message.clone()));
            return Err(e);
        }
        match self.finish_sign_in(&stored).await {
            Ok(view) => {
                self.emit(ServiceEvent::AccountChanged(view.clone()));
                self.progress(SignInStage::Done, None);
                Ok(view)
            }
            Err(e) => {
                self.progress(SignInStage::Error, Some(e.message.clone()));
                Err(e)
            }
        }
    }

    fn sign_in_failed(&self, e: AuthError) -> CmdError {
        let c: CmdError = e.into();
        self.progress(SignInStage::Error, Some(c.message.clone()));
        c
    }

    /// Save the token, replace any running state of the same account, and start polling (the first poll is the
    /// cold fill of a new account).
    async fn finish_sign_in(self: &Arc<Self>, stored: &StoredAccount) -> Result<AccountView, CmdError> {
        self.store.save(stored)?;
        let old = {
            let mut s = lock(&self.st);
            let id = stored.id();
            s.accounts.iter().position(|a| a.id == id).map(|i| s.accounts.remove(i))
        };
        if let Some(old) = old {
            self.teardown(old).await;
        }
        let acct = self.install(stored)?;
        Ok(self.view(&acct))
    }

    pub(crate) async fn reopen_sign_in(&self) -> Result<(), CmdError> {
        let h = lock(&self.st).login.clone();
        match h {
            Some(h) => h.reopen_browser().map_err(Into::into),
            None => Err(CmdError::not_found("no sign-in is pending")),
        }
    }

    pub(crate) fn cancel_sign_in(&self) {
        if let Some(h) = lock(&self.st).login.clone() {
            h.cancel();
        }
    }

    pub(crate) async fn reauth(self: &Arc<Self>, id: AccountId) -> Result<AccountView, CmdError> {
        let acct = self.get(&id)?;
        self.sign_in(Some(acct.email.clone()), Some(id)).await
    }

    pub(crate) async fn sign_out(self: &Arc<Self>, id: AccountId) -> Result<SignOutResult, CmdError> {
        let acct = {
            let mut s = lock(&self.st);
            let i = s.accounts.iter().position(|a| a.id == id).ok_or_else(|| CmdError::not_found("unknown account"))?;
            if s.active.as_ref() == Some(&id) {
                s.active = None;
            }
            s.accounts.remove(i)
        };
        let refresh_token = acct.tokens.refresh_token().to_string();
        self.teardown(acct).await;
        lock(&self.parsed).retain(|((a, _), _)| a != &id);

        // Revoke first, then always delete locally.
        let outcome = match &self.client {
            Some(c) => gauth::revoke_at(&c.endpoints.revoke, &refresh_token).await,
            None => RevokeOutcome::Failed("no OAuth client".into()),
        };
        if let Err(e) = self.store.delete(&id) {
            tracing::warn!(error = %e, "token file delete failed");
        }
        let mut deleted = false;
        for _ in 0..40 {
            match self.cache_root.delete_account(&id) {
                Ok(()) => {
                    deleted = true;
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        if !deleted {
            tracing::warn!("cache directory could not be deleted now; it is swept at the next start");
        }
        let revoked = matches!(outcome, RevokeOutcome::Revoked | RevokeOutcome::AlreadyInvalid);
        tracing::info!(revoked, "signed out");
        Ok(SignOutResult { revoked, manual_removal_needed: !revoked })
    }

    // ---------------------------------------------------------------- polling

    /// One request for a poll through the gate; runs polls until no trailing one is queued.
    async fn trigger(self: &Arc<Self>, acct: &Arc<Acct>, trigger: Trigger, announce: bool) {
        let decision = lock(&acct.gate).request(trigger);
        if decision != Decision::Start {
            return;
        }
        let mut announce = announce || trigger == Trigger::Manual;
        loop {
            self.do_poll(acct, announce).await;
            if acct.stopped() || !lock(&acct.gate).finish() {
                break;
            }
            announce = true;
        }
        acct.reschedule.notify_one();
    }

    async fn do_poll(self: &Arc<Self>, acct: &Arc<Acct>, announce: bool) {
        if acct.stopped() {
            return;
        }
        let base = self.interval(&acct.id);
        let prev = acct.state();
        if announce {
            self.set_sync(acct, SyncState::Syncing, true);
        }
        let tap = Tap::new(&acct.cache);
        let res = mail_sync::poll(&acct.api, &tap).await;
        let touched = tap.touched();
        let kind = res.as_ref().map_or_else(Kind::of, |_| Kind::Ok);
        let plan = lock(&acct.backoff).next(kind, base);
        {
            let mut m = lock(&acct.mem);
            m.last_end = Instant::now();
            m.backoff_delay = if kind == Kind::Ok { None } else { plan.delay };
        }
        self.schedule(acct);
        match res {
            Ok(rep) => {
                let changed =
                    !touched.is_empty() || rep.added + rep.changed + rep.removed > 0 || rep.resynced;
                if !acct.needs_reauth() {
                    self.set_sync(acct, SyncState::Ok, changed || announce);
                }
                if changed {
                    self.emit_changed(acct, touched);
                }
                self.housekeeping(acct, changed).await;
            }
            Err(e) => {
                tracing::debug!(?kind, "poll failed");
                match plan.ui {
                    Ui::NeedsReauth => self.mark_needs_reauth(acct),
                    Ui::Offline => self.set_sync(acct, SyncState::Offline, false),
                    Ui::Throttled => self.set_sync(acct, SyncState::Throttled, false),
                    Ui::Live => {
                        if announce {
                            let back = if prev == SyncState::Syncing { SyncState::Ok } else { prev };
                            self.set_sync(acct, back, true);
                        }
                    }
                }
                drop(e);
            }
        }
    }

    fn emit_changed(&self, acct: &Acct, touched: std::collections::BTreeSet<String>) {
        let mut labels = touched;
        labels.insert(label::INBOX.into());
        labels.insert(label::SPAM.into());
        labels.insert(label::ALL_MAIL.into());
        self.emit(ServiceEvent::MailChanged(MailChanged {
            account_id: acct.id.clone(),
            labels: labels.into_iter().collect(),
        }));
    }

    async fn housekeeping(&self, acct: &Arc<Acct>, changed: bool) {
        let due = {
            let m = lock(&acct.mem);
            m.counts_basic_at.is_none_or(|t| t.elapsed() >= COUNTS_TTL) && (changed || m.counts.is_empty())
        };
        if due {
            self.refresh_counts(acct, false).await;
        }
        if changed {
            let due = lock(&acct.mem).last_evict.is_none_or(|t| t.elapsed() >= EVICT_EVERY);
            if due {
                lock(&acct.mem).last_evict = Some(Instant::now());
                if let Err(e) = acct.cache.evict(now_ms()) {
                    tracing::warn!(error = %e, "evict failed");
                }
            }
        }
    }

    /// `labels.get` for the visible system folders (and, when `full`, user labels). Emits `account-changed` when
    /// the Inbox or Spam count moved.
    async fn refresh_counts(&self, acct: &Arc<Acct>, full: bool) {
        let ids: Vec<LabelId> = {
            let mut m = lock(&acct.mem);
            m.counts_basic_at = Some(Instant::now());
            let mut v = vec![LabelId::new(label::INBOX), LabelId::new(label::SPAM)];
            if full {
                m.counts_full_at = Some(Instant::now());
                v.extend([label::STARRED, label::SENT].map(LabelId::new));
                v.extend(label::CATEGORIES.map(LabelId::new));
                if let Some((labels, _)) = &m.labels {
                    v.extend(labels.iter().filter(|l| l.kind == LabelKind::User).map(|l| l.id.clone()));
                }
            }
            v
        };
        let got = mail_sync::folder_counts(&acct.api, &ids).await;
        let moved = {
            let mut m = lock(&acct.mem);
            let mut moved = false;
            for (id, c) in got {
                if c.unread.is_none() && c.total.is_none() {
                    continue;
                }
                let basic = id.as_str() == label::INBOX || id.as_str() == label::SPAM;
                let old = m.counts.insert(id, c);
                moved |= basic && old.map(|o| o.unread) != Some(c.unread);
            }
            moved
        };
        if moved {
            self.emit(ServiceEvent::AccountChanged(self.view(acct)));
        }
    }

    pub(crate) async fn refresh(self: &Arc<Self>, id: &AccountId) -> Result<(), CmdError> {
        let acct = self.get(id)?;
        if acct.needs_reauth() {
            return Err(AuthError::NeedsReauth { reason: "sign-in expired".into() }.into());
        }
        let (inner, a) = (self.clone(), acct.clone());
        acct.spawn(async move { inner.trigger(&a, Trigger::Manual, true).await });
        Ok(())
    }

    // ---------------------------------------------------------------- folders and lists

    async fn labels_of(self: &Arc<Self>, acct: &Arc<Acct>) -> Vec<LabelInfo> {
        let cached = lock(&acct.mem).labels.clone();
        if let Some((l, at)) = &cached
            && at.elapsed() < LABELS_TTL
        {
            return l.clone();
        }
        if acct.needs_reauth() {
            return cached.map(|c| c.0).unwrap_or_default();
        }
        match acct.api.labels().await {
            Ok(l) => {
                lock(&acct.mem).labels = Some((l.clone(), Instant::now()));
                l
            }
            Err(e) => {
                if is_auth_dead(&e) {
                    self.mark_needs_reauth(acct);
                }
                cached.map(|c| c.0).unwrap_or_default()
            }
        }
    }

    pub(crate) async fn folders(self: &Arc<Self>, id: &AccountId) -> Result<Vec<FolderView>, CmdError> {
        let acct = self.get(id)?;
        let labels = self.labels_of(&acct).await;
        let due = lock(&acct.mem).counts_full_at.is_none_or(|t| t.elapsed() >= COUNTS_TTL);
        if due && !acct.needs_reauth() {
            self.refresh_counts(&acct, true).await;
        }
        let mut tree = mail_model::build_tree(&labels);
        let m = lock(&acct.mem);
        for f in &mut tree {
            f.unread = match f.kind {
                FolderKind::Inbox
                | FolderKind::Starred
                | FolderKind::Sent
                | FolderKind::Spam
                | FolderKind::Category
                | FolderKind::User => m.counts.get(&LabelId::new(f.id.as_str())).and_then(|c| c.unread),
                _ => None,
            };
        }
        Ok(tree)
    }

    /// Fill one folder; returns whether anything the UI shows changed.
    async fn run_fill(&self, acct: &Arc<Acct>, label: &LabelId, more: bool) -> Result<bool, ApiError> {
        let before = acct.cache.window(label).ok().flatten();
        let tap = Tap::new(&acct.cache);
        mail_sync::fill_folder(&acct.api, &tap, label, more).await?;
        let after = acct.cache.window(label).ok().flatten();
        Ok(tap.stored() > 0 || window_shape(&before) != window_shape(&after))
    }

    fn spawn_fill(self: &Arc<Self>, acct: &Arc<Acct>, label: LabelId, more: bool) {
        let Some(guard) = FillGuard::try_new(acct, &label) else { return };
        let (inner, a) = (self.clone(), acct.clone());
        acct.spawn(async move {
            let _guard = guard;
            match inner.run_fill(&a, &label, more).await {
                Ok(true) => inner.emit(ServiceEvent::MailChanged(MailChanged {
                    account_id: a.id.clone(),
                    labels: vec![label.0.clone()],
                })),
                Ok(false) => {}
                Err(e) => inner.fill_failed(&a, e),
            }
        });
    }

    fn fill_failed(&self, acct: &Acct, e: ApiError) {
        match Kind::of(&e) {
            Kind::NeedsReauth => self.mark_needs_reauth(acct),
            kind => tracing::debug!(?kind, "folder fill failed"),
        }
    }

    pub(crate) async fn messages(
        self: &Arc<Self>,
        id: &AccountId,
        folder_id: &str,
        more: bool,
    ) -> Result<ListResult, CmdError> {
        let acct = self.get(id)?;
        let label = folder_label(folder_id)?;
        let offline = acct.state() == SyncState::Offline;
        let live = !acct.needs_reauth();
        let mut window = acct.cache.window(&label)?;
        let shown = if more { lock(&acct.mem).shown.get(folder_id).copied().unwrap_or(PAGE) + PAGE } else { PAGE };

        if more && live && !offline && window.as_ref().is_none_or(|w| !w.complete) {
            let cached_rows = acct.cache.rows(&label, None, shown + 1, false)?.len();
            if cached_rows <= shown
                && let Some(guard) = FillGuard::try_new(&acct, &label)
            {
                // A partial window without a next token (row cap): start again from the top.
                let from_token = window.as_ref().is_some_and(|w| w.next_page_token.is_some());
                match self.run_fill(&acct, &label, from_token).await {
                    Ok(_) => {}
                    Err(e) => self.fill_failed(&acct, e),
                }
                drop(guard);
                window = acct.cache.window(&label)?;
            }
        }

        let mut metas = acct.cache.rows(&label, None, shown + 1, offline)?;
        let has_extra = metas.len() > shown;
        metas.truncate(shown);
        let rows: Vec<_> = metas.iter().map(mail_render::message_row).collect();
        let incomplete = window.as_ref().map_or(!rows.is_empty(), |w| !w.complete);
        let end_of_cache = offline && !has_extra && window.as_ref().is_none_or(|w| !w.complete);
        let more_available = has_extra || (!offline && incomplete);
        lock(&acct.mem).shown.insert(folder_id.to_string(), shown);

        if !more && live {
            let fresh = window.as_ref().is_some_and(|w| now_ms() - w.as_of < FILL_FRESH_MS);
            if !fresh {
                self.spawn_fill(&acct, label, false);
            }
        }
        Ok(ListResult { rows, more: more_available, end_of_cache })
    }

    // ---------------------------------------------------------------- one message

    /// Raw bytes from the cache or Gmail. A fetched body is cached (under the cap). Returns `(raw, from_cache)`.
    async fn raw_of(self: &Arc<Self>, acct: &Arc<Acct>, msg: &MessageId, touch: bool) -> Result<(Vec<u8>, bool), CmdError> {
        if let Some(b) = acct.cache.body(msg).ok().flatten() {
            if touch {
                let _ = acct.cache.touch_opened(msg, now_ms());
            }
            return Ok((b, true));
        }
        if acct.needs_reauth() {
            return Err(AuthError::NeedsReauth { reason: "sign-in expired".into() }.into());
        }
        let raw = acct.api.raw(msg).await.map_err(|e| self.api_err(acct, e))?;
        match acct.cache.put_body(msg, &raw, now_ms()) {
            Ok(true) if touch => {
                let _ = acct.cache.touch_opened(msg, now_ms());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "body not cached"),
        }
        Ok((raw, false))
    }

    fn remember(&self, key: (AccountId, MessageId), parsed: Arc<Parsed>) {
        let mut l = lock(&self.parsed);
        l.retain(|(k, _)| k != &key);
        l.insert(0, (key, parsed));
        l.truncate(PARSED_LRU);
    }

    async fn parsed_of(self: &Arc<Self>, acct: &Arc<Acct>, msg: &MessageId) -> Result<Arc<Parsed>, CmdError> {
        let key = (acct.id.clone(), msg.clone());
        let hit = lock(&self.parsed).iter().find(|(k, _)| k == &key).map(|(_, p)| p.clone());
        if let Some(p) = hit {
            return Ok(p);
        }
        let (raw, _) = self.raw_of(acct, msg, false).await?;
        let parsed = Arc::new(mail_render::parse(&raw)?);
        self.remember(key, parsed.clone());
        Ok(parsed)
    }

    /// Before opening: a stale row (or an uncached message without a cached body) is revalidated with one
    /// metadata get. Success refreshes the row (clearing `stale`); a vanished message is removed from the cache and
    /// reported as `not_found` / `gone`. Offline or any other failure falls back to whatever is cached.
    /// Returns the best known metadata (for the `internalDate` fallback).
    async fn revalidate(self: &Arc<Self>, acct: &Arc<Acct>, msg: &MessageId) -> Result<Option<MessageMeta>, CmdError> {
        let row = acct.cache.meta(msg).ok().flatten();
        let needs = match &row {
            Some((_, stale)) => *stale,
            None => acct.cache.body(msg).ok().flatten().is_none(),
        };
        if !needs || acct.needs_reauth() {
            return Ok(row.map(|(m, _)| m));
        }
        let cached = row.is_some();
        let fetched = match acct.api.metadata_batch(std::slice::from_ref(msg)).await {
            Ok(mut v) if v.len() == 1 => v.remove(0),
            Ok(_) => return Ok(row.map(|(m, _)| m)),
            Err(e) => Err(e),
        };
        match fetched {
            Ok(m) => {
                let mode = if cached { UpsertMode::Always } else { UpsertMode::Admit };
                if let Err(e) = acct.cache.upsert_meta(std::slice::from_ref(&m), now_ms(), mode) {
                    tracing::warn!(error = %e, "revalidated row not cached");
                }
                Ok(Some(m))
            }
            Err(ApiError::NotFound) => {
                if let Err(e) = acct.cache.apply_events(&[HistoryEvent::Deleted { id: msg.clone() }]) {
                    tracing::warn!(error = %e, "gone message not removed from the cache");
                }
                lock(&self.parsed).retain(|(k, _)| !(k.0 == acct.id && &k.1 == msg));
                Err(CmdError::not_found("This message is no longer in Gmail.").with_detail("gone"))
            }
            Err(e) if is_auth_dead(&e) => Err(self.api_err(acct, e)),
            Err(_) => Ok(row.map(|(m, _)| m)),
        }
    }

    pub(crate) async fn message(
        self: &Arc<Self>,
        id: &AccountId,
        msg: &MessageId,
        show_images: bool,
    ) -> Result<MessageView, CmdError> {
        let acct = self.get(id)?;
        let known = self.revalidate(&acct, msg).await?;
        let (raw, cached) = self.raw_of(&acct, msg, true).await?;
        let parsed = Arc::new(mail_render::parse(&raw)?);
        self.remember((id.clone(), msg.clone()), parsed.clone());
        let policy = if show_images { ImagePolicy::Allow } else { ImagePolicy::Block };
        let rendered = mail_render::render(&parsed, id, msg, policy);
        let hb = mail_render::header_block(&parsed);
        let attachments: Vec<AttachmentInfo> = mail_render::attachments(&parsed);
        let labels = self.label_names(&acct, msg).await;
        let date = hb.date_ms.or_else(|| known.as_ref().map(|m| m.internal_date_ms)).unwrap_or(0);
        Ok(MessageView {
            id: msg.clone(),
            subject: hb.subject,
            from: hb.from,
            to: hb.to,
            cc: hb.cc,
            reply_to: hb.reply_to,
            date,
            labels,
            auth: mail_render::auth_summary(&parsed),
            attachments,
            page: rendered.page,
            remote_images_blocked: rendered.remote_images_blocked,
            images_shown: show_images,
            cached,
        })
    }

    /// Display names of a cached row's labels (system names; user labels by name; hidden labels omitted).
    async fn label_names(self: &Arc<Self>, acct: &Arc<Acct>, msg: &MessageId) -> Vec<String> {
        let ids = acct.cache.label_ids(msg).ok().flatten().unwrap_or_default();
        let mut user: Option<Vec<LabelInfo>> = None;
        let mut out = Vec::new();
        for l in ids {
            if label::HIDDEN.contains(&l.as_str()) {
                continue;
            }
            if let Some(n) = system_label_name(l.as_str()) {
                out.push(n.to_string());
                continue;
            }
            if user.is_none() {
                let known = lock(&acct.mem).labels.as_ref().map(|c| c.0.clone());
                user = Some(match known {
                    Some(k) => k,
                    None => self.labels_of(acct).await,
                });
            }
            if let Some(info) = user.as_ref().and_then(|u| u.iter().find(|i| i.id == l)) {
                out.push(info.name.clone());
            }
        }
        out
    }

    pub(crate) async fn asset(
        self: &Arc<Self>,
        id: &AccountId,
        msg: &MessageId,
        part: PartId,
    ) -> Result<Asset, CmdError> {
        let acct = self.get(id)?;
        let parsed = self.parsed_of(&acct, msg).await?;
        let (bytes, mime, _) = mail_render::part(&parsed, part).ok_or_else(|| CmdError::not_found("unknown part"))?;
        Ok(Asset { bytes, headers: mail_render::asset_headers(&mime) })
    }

    pub(crate) async fn save_attachment(
        self: &Arc<Self>,
        id: &AccountId,
        msg: &MessageId,
        part: PartId,
        saver: &dyn FileSaver,
    ) -> Result<SaveResult, CmdError> {
        let acct = self.get(id)?;
        let parsed = self.parsed_of(&acct, msg).await?;
        let (bytes, _, name) = mail_render::part(&parsed, part).ok_or_else(|| CmdError::not_found("unknown attachment"))?;
        saver.save(name.as_deref().unwrap_or("attachment"), &bytes)
    }
}


/// The per-account poll task: poll now (the cold fill of a new account), then wait for the timer, a manual
/// trigger, a reschedule or a stop.
async fn poll_loop(inner: Arc<Inner>, acct: Arc<Acct>) {
    inner.trigger(&acct, Trigger::Timer, true).await;
    loop {
        if acct.stopped() {
            return;
        }
        let next = lock(&acct.mem).next_at.max(Instant::now() + MIN_GAP);
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {
                if acct.stopped() { return; }
                inner.trigger(&acct, Trigger::Timer, false).await;
            }
            _ = acct.poll_now.notified() => {
                inner.trigger(&acct, Trigger::Manual, false).await;
            }
            _ = acct.reschedule.notified() => {}
            _ = acct.stop.notified() => return,
        }
    }
}
