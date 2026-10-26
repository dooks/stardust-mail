//! Service tests against a fake Gmail API and synthetic mail. No calls to Google: the revoke endpoint points at a
//! local socket. Tokens, caches and the account store are real (DPAPI, SQLite) in a temporary directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gauth::{Client, Endpoints, Store, StoredAccount};
use mail_model::{
    AccountId, AccountState, ApiError, AuthError, CmdError, GmailApi, HistoryId, HistoryPage, LabelCounts, LabelId,
    LabelInfo, LabelKind, ListPage, MessageId, MessageMeta, PartId, Profile, SaveResult, SyncState, ThreadId, WindowState,
};
use mail_sync::Cadence;
use tokio::sync::Semaphore;

use crate::{DynApi, EventSink, FileSaver, Parts, Service, ServiceEvent};

// ---------------------------------------------------------------- fixtures

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "starmail-svc-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            mail_model::now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<ServiceEvent>>);

impl EventSink for Collect {
    fn emit(&self, event: ServiceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collect {
    fn all(&self) -> Vec<ServiceEvent> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, f: impl Fn(&ServiceEvent) -> bool) -> usize {
        self.all().iter().filter(|e| f(e)).count()
    }
}

#[derive(Default)]
struct FakeState {
    msgs: Mutex<Vec<MessageMeta>>,
    raws: Mutex<HashMap<String, Vec<u8>>>,
    fail: Mutex<Option<ApiError>>,
    history_calls: AtomicUsize,
    profile_calls: AtomicUsize,
    history_gate: Mutex<Option<Arc<Semaphore>>>,
}

impl FakeState {
    fn set_fail(&self, e: Option<ApiError>) {
        *self.fail.lock().unwrap() = e;
    }
    fn check(&self) -> Result<(), ApiError> {
        match self.fail.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn total_calls(&self) -> usize {
        self.history_calls.load(Ordering::SeqCst) + self.profile_calls.load(Ordering::SeqCst)
    }
}

struct FakeApi(Arc<FakeState>);

impl GmailApi for FakeApi {
    async fn profile(&self) -> Result<Profile, ApiError> {
        self.0.profile_calls.fetch_add(1, Ordering::SeqCst);
        self.0.check()?;
        Ok(Profile { email: "fake@example.com".into(), messages_total: 3, history_id: HistoryId(100) })
    }

    async fn labels(&self) -> Result<Vec<LabelInfo>, ApiError> {
        self.0.check()?;
        Ok(vec![LabelInfo { id: LabelId::new("Label_1"), name: "Work/Reports".into(), kind: LabelKind::User }])
    }

    async fn label_counts(&self, id: &LabelId) -> Result<LabelCounts, ApiError> {
        self.0.check()?;
        let unread = match id.as_str() {
            "INBOX" => 3,
            "SPAM" => 1,
            _ => 0,
        };
        Ok(LabelCounts { total: Some(10), unread: Some(unread) })
    }

    async fn list(&self, label: Option<&LabelId>, page_token: Option<&str>, max: u32) -> Result<ListPage, ApiError> {
        self.0.check()?;
        let mut v: Vec<MessageMeta> = self
            .0
            .msgs
            .lock()
            .unwrap()
            .iter()
            .filter(|m| label.is_none_or(|l| m.label_ids.contains(l)))
            .cloned()
            .collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.internal_date_ms));
        let start: usize = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let end = (start + max as usize).min(v.len());
        let ids = v[start.min(end)..end].iter().map(|m| (m.id.clone(), m.thread_id.clone())).collect();
        Ok(ListPage { ids, next_page_token: (end < v.len()).then(|| end.to_string()) })
    }

    async fn metadata_batch(&self, ids: &[MessageId]) -> Result<Vec<Result<MessageMeta, ApiError>>, ApiError> {
        self.0.check()?;
        let g = self.0.msgs.lock().unwrap();
        Ok(ids.iter().map(|id| g.iter().find(|m| &m.id == id).cloned().ok_or(ApiError::NotFound)).collect())
    }

    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>, ApiError> {
        self.0.check()?;
        self.0.raws.lock().unwrap().get(id.as_str()).cloned().ok_or(ApiError::NotFound)
    }

    async fn history(&self, _start: HistoryId, _page_token: Option<&str>) -> Result<HistoryPage, ApiError> {
        self.0.history_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.0.history_gate.lock().unwrap().clone();
        if let Some(g) = gate {
            g.acquire().await.unwrap().forget();
        }
        self.0.check()?;
        Ok(HistoryPage { events: vec![], history_id: HistoryId(100), next_page_token: None })
    }
}

fn meta(id: &str, labels: &[&str], date: i64) -> MessageMeta {
    MessageMeta {
        id: MessageId::new(id),
        thread_id: ThreadId::new(format!("t{id}")),
        label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
        internal_date_ms: date,
        size_estimate: 1000,
        history_id: HistoryId(1),
        mime_type: "text/plain".into(),
        from_raw: "Example Sender <sender@example.com>".into(),
        to_raw: "me@example.com".into(),
        subject: format!("Subject {id}"),
        date_raw: "Tue, 14 Nov 2023 22:13:20 +0000".into(),
        snippet_raw: "It&#39;s a snippet".into(),
    }
}

fn seed() -> Vec<MessageMeta> {
    vec![
        meta("m1", &["INBOX", "UNREAD"], 3_000),
        meta("m2", &["INBOX"], 2_000),
        meta("m3", &["INBOX", "STARRED"], 1_000),
        meta("s1", &["SENT"], 2_500),
        meta("s2", &["SENT"], 500),
    ]
}

struct Env {
    dir: TempDir,
    sink: Arc<Collect>,
    fakes: Arc<Mutex<HashMap<String, Arc<FakeState>>>>,
    svc: Service,
}

impl Env {
    fn fake(&self, sub: &str) -> Arc<FakeState> {
        self.fakes.lock().unwrap().get(sub).cloned().expect("fake exists")
    }
}

fn stored(sub: &str, needs_reauth: bool) -> StoredAccount {
    StoredAccount {
        email: format!("{sub}@example.com"),
        sub: sub.into(),
        refresh_token: format!("rt-{sub}"),
        scopes: vec![gauth::SCOPE_GMAIL_READONLY.into()],
        created_at: 1,
        needs_reauth,
    }
}

fn slow_cadence() -> Cadence {
    let d = Duration::from_secs(600);
    Cadence { focused_active: d, focused_other: d, unfocused: d, minimized: d, jitter: 0.0 }
}

async fn start(subs: &[&str], with_client: bool, revoke_url: &str, cadence: Cadence) -> Env {
    let dir = TempDir::new();
    let store = Store::open(&dir.path().join("accounts")).unwrap();
    for s in subs {
        store.save(&stored(s, false)).unwrap();
    }
    start_in(dir, with_client, revoke_url, cadence, subs, |_| {}).await
}

async fn start_in(
    dir: TempDir,
    with_client: bool,
    revoke_url: &str,
    cadence: Cadence,
    subs: &[&str],
    prep: impl Fn(&FakeState),
) -> Env {
    let fakes: Arc<Mutex<HashMap<String, Arc<FakeState>>>> = Arc::default();
    for s in subs {
        let f = Arc::new(FakeState::default());
        *f.msgs.lock().unwrap() = seed();
        prep(&f);
        fakes.lock().unwrap().insert((*s).into(), f);
    }
    let factory_fakes = fakes.clone();
    let client = with_client.then(|| Client {
        client_id: "test-id".into(),
        client_secret: "test-secret".into(),
        endpoints: Endpoints { revoke: revoke_url.into(), ..Endpoints::default() },
    });
    let parts = Parts {
        client,
        api_factory: Arc::new(move |t| {
            let f = factory_fakes.lock().unwrap().get(&t.account_id().0).cloned().expect("fake for account");
            DynApi::new(FakeApi(f))
        }),
        cadence,
        login_timeout: Duration::from_secs(5),
    };
    let sink = Arc::new(Collect::default());
    let svc = Service::with_parts(dir.path().to_path_buf(), sink.clone(), parts).await.unwrap();
    Env { dir, sink, fakes, svc }
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn is_status(e: &ServiceEvent, sub: &str, state: SyncState) -> bool {
    matches!(e, ServiceEvent::SyncStatus(s) if s.account_id.0 == sub && s.state == state)
}

async fn wait_ready(env: &Env, sub: &str) {
    let sink = env.sink.clone();
    wait_until("initial sync", move || sink.count(|e| is_status(e, sub, SyncState::Ok)) > 0).await;
}

fn id(s: &str) -> AccountId {
    AccountId::new(s)
}

/// A one-shot HTTP server for the revoke endpoint: records whether the token file existed when the request
/// arrived, and answers 200.
async fn revoke_server(token_file: PathBuf) -> (String, Arc<Mutex<Vec<bool>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/revoke", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = sock.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            seen2.lock().unwrap().push(token_file.exists());
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            let _ = sock.shutdown().await;
        }
    });
    (url, seen)
}

struct Saver(Mutex<Vec<(String, usize)>>);

impl FileSaver for Saver {
    fn save(&self, name: &str, bytes: &[u8]) -> Result<SaveResult, CmdError> {
        self.0.lock().unwrap().push((name.to_string(), bytes.len()));
        Ok(SaveResult { saved: true, file_name: Some(name.to_string()) })
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn multi_account_startup() {
    let env = start(&["acc-a", "acc-b"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    wait_ready(&env, "acc-b").await;

    let accounts = env.svc.accounts().await.unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts.iter().all(|a| a.state == AccountState::SignedIn));

    // Counts arrive after the first poll and are announced with account-changed.
    let svc = &env.svc;
    for _ in 0..300 {
        let a = svc.accounts().await.unwrap();
        if a.iter().all(|x| x.inbox_unread == Some(3) && x.spam_unread == Some(1)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let a = svc.accounts().await.unwrap();
    assert!(a.iter().all(|x| x.inbox_unread == Some(3) && x.spam_unread == Some(1)), "{a:?}");
    assert!(env.sink.count(|e| matches!(e, ServiceEvent::AccountChanged(_))) >= 2);

    // The cold fill left the Inbox cached.
    let list = svc.messages(id("acc-a"), "INBOX".into(), false).await.unwrap();
    assert_eq!(list.rows.len(), 3);
    assert_eq!(list.rows[0].id.0, "m1");
    assert!(list.rows[0].unread);
    assert_eq!(list.rows[0].snippet, "It's a snippet");
    assert!(!list.more);

    // Folder tree with unread counts.
    let tree = svc.folders(id("acc-b")).await.unwrap();
    let inbox = tree.iter().find(|f| f.id == "INBOX").unwrap();
    assert_eq!(inbox.unread, Some(3));
    let spam = tree.iter().find(|f| f.id == "SPAM").unwrap();
    assert_eq!(spam.unread, Some(1));
    assert!(tree.iter().any(|f| f.name == "Reports"), "user label nested under a Work group");

    let status = svc.status().await.unwrap();
    assert_eq!(status.len(), 2);
    assert!(status.iter().all(|s| s.state == SyncState::Ok && s.as_of.is_some()));

    assert!(svc.set_view(id("nope"), "INBOX".into()).await.is_err());
    svc.set_view(id("acc-a"), "INBOX".into()).await.unwrap();
    env.svc.shutdown().await;
}

#[tokio::test]
async fn messages_are_cache_first_then_mail_changed_after_fill() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;

    // Nothing of SENT is cached: the call returns at once with no rows, and a fill runs in the background.
    let first = env.svc.messages(id("acc-a"), "SENT".into(), false).await.unwrap();
    assert!(first.rows.is_empty());
    let sink = env.sink.clone();
    wait_until("mail-changed for SENT", move || {
        sink.count(|e| matches!(e, ServiceEvent::MailChanged(m) if m.labels == ["SENT"])) == 1
    })
    .await;

    let second = env.svc.messages(id("acc-a"), "SENT".into(), false).await.unwrap();
    assert_eq!(second.rows.iter().map(|r| r.id.0.as_str()).collect::<Vec<_>>(), ["s1", "s2"]);
    assert!(!second.more);

    // The window is fresh and nothing changed: no second fill, no event loop.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(env.sink.count(|e| matches!(e, ServiceEvent::MailChanged(m) if m.labels == ["SENT"])), 1);

    // All Mail and group folders.
    let all = env.svc.messages(id("acc-a"), "ALL".into(), false).await.unwrap();
    assert_eq!(all.rows.len(), 5);
    assert!(env.svc.messages(id("acc-a"), "group:Work".into(), false).await.is_err());
    env.svc.shutdown().await;
}

#[tokio::test]
async fn load_more_extends_the_window() {
    let dir = TempDir::new();
    let store = Store::open(&dir.path().join("accounts")).unwrap();
    store.save(&stored("acc-a", false)).unwrap();
    let env = start_in(dir, true, "http://127.0.0.1:1/revoke", slow_cadence(), &["acc-a"], |f| {
        let mut v = f.msgs.lock().unwrap();
        for i in 0..120 {
            v.push(meta(&format!("bulk{i:03}"), &["INBOX"], 10_000 + i));
        }
    })
    .await;
    wait_ready(&env, "acc-a").await;
    let page1 = env.svc.messages(id("acc-a"), "INBOX".into(), false).await.unwrap();
    assert_eq!(page1.rows.len(), 50);
    assert!(page1.more);
    let page2 = env.svc.messages(id("acc-a"), "INBOX".into(), true).await.unwrap();
    assert_eq!(page2.rows.len(), 100);
    assert!(page2.more);
    let page3 = env.svc.messages(id("acc-a"), "INBOX".into(), true).await.unwrap();
    assert_eq!(page3.rows.len(), 123);
    assert!(!page3.more);
    env.svc.shutdown().await;
}

#[tokio::test]
async fn sign_out_revokes_before_deleting_locally() {
    let dir = TempDir::new();
    let token_file = dir.path().join("accounts").join("acc-a.dpapi");
    let (url, seen) = revoke_server(token_file.clone()).await;
    let store = Store::open(&dir.path().join("accounts")).unwrap();
    store.save(&stored("acc-a", false)).unwrap();
    store.save(&stored("acc-b", false)).unwrap();
    let env = start_in(dir, true, &url, slow_cadence(), &["acc-a", "acc-b"], |_| {}).await;
    wait_ready(&env, "acc-a").await;
    wait_ready(&env, "acc-b").await;
    let cache_dir = env.dir.path().join("cache").join("acc-a");
    assert!(cache_dir.exists());

    let res = env.svc.sign_out(id("acc-a")).await.unwrap();
    assert!(res.revoked && !res.manual_removal_needed);
    assert_eq!(*seen.lock().unwrap(), vec![true], "the token file still existed when the revoke arrived");
    assert!(!token_file.exists());
    assert!(!cache_dir.exists());
    assert_eq!(env.svc.accounts().await.unwrap().len(), 1);
    assert!(env.svc.messages(id("acc-a"), "INBOX".into(), false).await.is_err());
    // The other account is untouched.
    assert!(env.dir.path().join("accounts").join("acc-b.dpapi").exists());
    assert!(env.svc.sign_out(id("acc-a")).await.is_err());
    env.svc.shutdown().await;
}

#[tokio::test]
async fn sign_out_deletes_locally_even_when_revoke_fails() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let token_file = env.dir.path().join("accounts").join("acc-a.dpapi");
    let cache_dir = env.dir.path().join("cache").join("acc-a");
    assert!(token_file.exists() && cache_dir.exists());

    let res = env.svc.sign_out(id("acc-a")).await.unwrap();
    assert!(!res.revoked);
    assert!(res.manual_removal_needed);
    assert!(!token_file.exists());
    assert!(!cache_dir.exists());
    assert!(env.svc.accounts().await.unwrap().is_empty());
    env.svc.shutdown().await;
}

#[tokio::test]
async fn needs_reauth_stops_polling_and_persists() {
    let dir = TempDir::new();
    let store = Store::open(&dir.path().join("accounts")).unwrap();
    store.save(&stored("acc-a", false)).unwrap();
    let fast = Cadence {
        focused_active: Duration::from_millis(30),
        focused_other: Duration::from_millis(30),
        unfocused: Duration::from_millis(30),
        minimized: Duration::from_millis(30),
        jitter: 0.0,
    };
    let env = start_in(dir, true, "http://127.0.0.1:1/revoke", fast, &["acc-a"], |f| {
        f.set_fail(Some(ApiError::Auth(AuthError::NeedsReauth { reason: "invalid_grant".into() })));
    })
    .await;
    let sink = env.sink.clone();
    wait_until("needsReauth status", move || sink.count(|e| is_status(e, "acc-a", SyncState::NeedsReauth)) > 0).await;

    assert_eq!(
        env.sink.count(|e| matches!(e, ServiceEvent::AccountChanged(a) if a.state == AccountState::NeedsReauth)),
        1
    );
    let accounts = env.svc.accounts().await.unwrap();
    assert_eq!(accounts[0].state, AccountState::NeedsReauth);
    let (list, _) = Store::open(&env.dir.path().join("accounts")).unwrap().list();
    assert!(list[0].needs_reauth, "the flag is persisted");

    // No more calls, even though the cadence is 30 ms.
    let fake = env.fake("acc-a");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls = fake.total_calls();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fake.total_calls(), calls, "polling stopped");
    assert!(env.svc.refresh(id("acc-a")).await.is_err());
    // The cache stays readable.
    assert!(env.svc.messages(id("acc-a"), "INBOX".into(), false).await.is_ok());
    env.svc.shutdown().await;

    // A restart keeps the account in needs-reauth and does not poll it.
    let parts_dir = env.dir.path().to_path_buf();
    drop(env.svc);
    let sink2 = Arc::new(Collect::default());
    let fake2 = Arc::new(FakeState::default());
    let f2 = fake2.clone();
    let parts = Parts {
        client: Some(Client { client_id: "i".into(), client_secret: "s".into(), endpoints: Endpoints::default() }),
        api_factory: Arc::new(move |_| DynApi::new(FakeApi(f2.clone()))),
        cadence: slow_cadence(),
        login_timeout: Duration::from_secs(1),
    };
    let svc2 = Service::with_parts(parts_dir, sink2, parts).await.unwrap();
    let a = svc2.accounts().await.unwrap();
    assert_eq!(a[0].state, AccountState::NeedsReauth);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake2.total_calls(), 0);
    svc2.shutdown().await;
}

#[tokio::test]
async fn offline_does_not_change_account_state() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.set_fail(Some(ApiError::Offline("no network".into())));

    // Two failed polls: the banner goes offline, the account stays signed in.
    for round in 1..=2usize {
        let before = fake.history_calls.load(Ordering::SeqCst);
        env.svc.refresh(id("acc-a")).await.unwrap();
        let f = fake.clone();
        wait_until("poll attempt", move || f.history_calls.load(Ordering::SeqCst) > before).await;
        let sink = env.sink.clone();
        if round == 2 {
            wait_until("offline status", move || sink.count(|e| is_status(e, "acc-a", SyncState::Offline)) > 0).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(env.svc.accounts().await.unwrap()[0].state, AccountState::SignedIn);
    assert_eq!(env.svc.status().await.unwrap()[0].state, SyncState::Offline);
    let (list, _) = Store::open(&env.dir.path().join("accounts")).unwrap().list();
    assert!(!list[0].needs_reauth);
    assert_eq!(env.sink.count(|e| matches!(e, ServiceEvent::AccountChanged(a) if a.state == AccountState::NeedsReauth)), 0);

    // Cached mail stays readable; a partial window would end with "end of cache".
    let l = env.svc.messages(id("acc-a"), "INBOX".into(), false).await.unwrap();
    assert_eq!(l.rows.len(), 3);
    let sent = env.svc.messages(id("acc-a"), "SENT".into(), false).await.unwrap();
    assert!(sent.rows.is_empty() && sent.end_of_cache && !sent.more);

    // Back online: the next poll succeeds and the status recovers.
    fake.set_fail(None);
    let before = env.sink.count(|e| is_status(e, "acc-a", SyncState::Ok));
    env.svc.refresh(id("acc-a")).await.unwrap();
    let sink = env.sink.clone();
    wait_until("recovered", move || sink.count(|e| is_status(e, "acc-a", SyncState::Ok)) > before).await;
    assert_eq!(env.svc.status().await.unwrap()[0].state, SyncState::Ok);
    env.svc.shutdown().await;
}

#[tokio::test]
async fn refresh_coalesces_into_one_trailing_poll() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    let gate = Arc::new(Semaphore::new(0));
    *fake.history_gate.lock().unwrap() = Some(gate.clone());
    let base = fake.history_calls.load(Ordering::SeqCst);

    env.svc.refresh(id("acc-a")).await.unwrap();
    let f = fake.clone();
    wait_until("poll in flight", move || f.history_calls.load(Ordering::SeqCst) == base + 1).await;
    for _ in 0..4 {
        env.svc.refresh(id("acc-a")).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.history_calls.load(Ordering::SeqCst), base + 1, "nothing runs in parallel");

    gate.add_permits(10);
    let f = fake.clone();
    wait_until("trailing poll", move || f.history_calls.load(Ordering::SeqCst) >= base + 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.history_calls.load(Ordering::SeqCst), base + 2, "exactly one trailing poll");
    env.svc.shutdown().await;
}

#[tokio::test]
async fn focus_and_restore_poll_immediately() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    let base = fake.history_calls.load(Ordering::SeqCst);
    env.svc.set_window_state(WindowState::Minimized);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(fake.history_calls.load(Ordering::SeqCst), base, "minimizing does not poll");
    env.svc.set_window_state(WindowState::Focused);
    let f = fake.clone();
    wait_until("poll on restore", move || f.history_calls.load(Ordering::SeqCst) > base).await;
    env.svc.shutdown().await;
}

#[tokio::test]
async fn message_renders_from_the_cached_body() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.raws.lock().unwrap().insert("m1".into(), mail_render::fixtures::synthetic("alt").unwrap());

    let v = env.svc.message(id("acc-a"), MessageId::new("m1"), false).await.unwrap();
    assert!(!v.cached);
    assert!(v.page.contains("Content-Security-Policy"));
    assert!(!v.subject.is_empty());
    assert!(!v.images_shown);
    assert!(v.labels.contains(&"Inbox".to_string()));
    assert!(!v.labels.iter().any(|l| l == "UNREAD"));

    // Now the network is gone: the cached body still opens, and "show images" re-renders it.
    fake.set_fail(Some(ApiError::Offline("no network".into())));
    let again = env.svc.message(id("acc-a"), MessageId::new("m1"), true).await.unwrap();
    assert!(again.cached);
    assert!(again.images_shown);
    assert_eq!(again.subject, v.subject);

    // A message that was never fetched cannot open offline, and the error is an offline error.
    let err = env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap_err();
    assert_eq!(err.code, mail_model::CmdErrorCode::Offline);
    env.svc.shutdown().await;
}

fn mark_stale(env: &Env) {
    use mail_model::MailCache;
    let acct = env.svc.inner.get(&id("acc-a")).unwrap();
    assert!(acct.cache.mark_all_stale_and_drop_windows().unwrap() > 0);
}

fn cached_meta(env: &Env, m: &str) -> Option<(MessageMeta, bool)> {
    use mail_model::MailCache;
    env.svc.inner.get(&id("acc-a")).unwrap().cache.meta(&MessageId::new(m)).unwrap()
}

#[tokio::test]
async fn stale_row_is_revalidated_then_rendered() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.raws.lock().unwrap().insert("m2".into(), mail_render::fixtures::synthetic("alt").unwrap());
    assert!(!cached_meta(&env, "m2").unwrap().1);
    mark_stale(&env);
    assert!(cached_meta(&env, "m2").unwrap().1);

    let v = env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap();
    assert!(v.page.contains("Content-Security-Policy"));
    assert!(!cached_meta(&env, "m2").unwrap().1, "revalidation clears stale");
    env.svc.shutdown().await;
}

#[tokio::test]
async fn deleted_message_is_gone_and_removed_from_the_cache() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.raws.lock().unwrap().insert("m2".into(), mail_render::fixtures::synthetic("alt").unwrap());
    env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap();
    mark_stale(&env);
    fake.msgs.lock().unwrap().retain(|m| m.id.as_str() != "m2");

    let err = env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap_err();
    assert_eq!(err.code, mail_model::CmdErrorCode::NotFound);
    assert_eq!(err.detail.as_deref(), Some("gone"));
    assert_eq!(err.message, "This message is no longer in Gmail.");
    assert!(cached_meta(&env, "m2").is_none());
    env.svc.shutdown().await;
}

#[tokio::test]
async fn offline_stale_row_still_opens_from_the_cache() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.raws.lock().unwrap().insert("m2".into(), mail_render::fixtures::synthetic("alt").unwrap());
    env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap();
    mark_stale(&env);
    fake.set_fail(Some(ApiError::Offline("no network".into())));

    let v = env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap();
    assert!(v.cached);
    assert!(cached_meta(&env, "m2").unwrap().1, "still stale: nothing could revalidate it");
    env.svc.shutdown().await;
}

#[tokio::test]
async fn assets_and_attachment_saves_come_from_the_parsed_message() {
    let env = start(&["acc-a"], true, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    wait_ready(&env, "acc-a").await;
    let fake = env.fake("acc-a");
    fake.raws.lock().unwrap().insert("m2".into(), mail_render::fixtures::synthetic("attachment_only").unwrap());

    let v = env.svc.message(id("acc-a"), MessageId::new("m2"), false).await.unwrap();
    let att = v.attachments.first().expect("an attachment is listed").clone();
    let saver = Saver(Mutex::default());
    let res = env.svc.save_attachment(id("acc-a"), MessageId::new("m2"), att.part_id, &saver).await.unwrap();
    assert!(res.saved);
    let saved = saver.0.lock().unwrap().clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].1 as u64, att.size);

    let asset = env.svc.asset(id("acc-a"), MessageId::new("m2"), att.part_id).await.unwrap();
    assert_eq!(asset.bytes.len() as u64, att.size);
    assert!(asset.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("x-content-type-options") && v == "nosniff"));

    let missing = env.svc.asset(id("acc-a"), MessageId::new("m2"), PartId(999)).await.unwrap_err();
    assert_eq!(missing.code, mail_model::CmdErrorCode::NotFound);
    env.svc.shutdown().await;
}

#[tokio::test]
async fn without_an_oauth_client_there_are_no_accounts_and_sign_in_is_a_config_error() {
    let env = start(&["acc-a"], false, "http://127.0.0.1:1/revoke", slow_cadence()).await;
    assert!(env.svc.accounts().await.unwrap().is_empty());
    let err = env.svc.add_account(None).await.unwrap_err();
    assert_eq!(err.detail.as_deref(), Some("config"));
    assert!(err.message.contains("oauth-client.json"));
    assert_eq!(
        env.sink.count(|e| matches!(e, ServiceEvent::SignInProgress(p) if p.stage == mail_model::SignInStage::Error)),
        1
    );
    assert!(env.svc.reopen_sign_in().await.is_err());
    env.svc.cancel_sign_in().await.unwrap();
    assert!(env.svc.reauth(id("acc-a")).await.is_err());
    env.svc.shutdown().await;
}
