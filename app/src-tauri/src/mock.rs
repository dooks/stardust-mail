//! `MockService`: canned, synthetic data behind the same method names and signatures as
//! `mail_service::Service`, so the host can run (and the UI can be built and checked by hand) without Gmail.
//! Selected with the `mock` cargo feature; WP-I swaps in the real service by changing one type alias.
//!
//! Everything here is invented: example.com style addresses, made-up subjects. No network access, nothing on
//! disk. Not for production: the "hostile" message deliberately puts RAW hostile HTML into the page (wrapped only
//! in the page CSP, and shown by the UI in a sandboxed iframe) to prove those two layers keep it inert; the real
//! renderer sanitizes first (WP-I wires `mail_render::render`; until then the mock pages are static white pages).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mail_model::{
    ASSET_HOST, AccountId, AccountState, AccountView, Addr, AttachmentInfo, AuthError, AuthLevel, AuthSummary,
    CmdError, FolderKind, FolderView, LabelId, LabelInfo, LabelKind, ListResult, MailChanged, MessageId,
    MessageRow, MessageView, PartId, SaveResult, SignInProgress, SignInStage, SignOutResult, SyncState,
    SyncStatus, WindowState, build_tree, label, now_ms,
};
use mail_service::{Asset, EventSink, FileSaver, ServiceEvent};

use crate::assets::asset_headers;

/// Rows per page; "more" reveals the next page.
const PAGE: usize = 30;
/// Where blocked remote images in the mock point (the discard port: nothing listens, nothing leaves the machine).
const CANARY: &str = "127.0.0.1:9";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Hostile,
    Cid,
    Attachments,
}

#[derive(Debug, Clone)]
struct MockMsg {
    id: String,
    labels: Vec<String>,
    date: i64,
    from_name: String,
    from_addr: String,
    subject: String,
    snippet: String,
    unread: bool,
    starred: bool,
    kind: Kind,
}

struct MockAccount {
    id: AccountId,
    email: String,
    state: AccountState,
}

struct Inner {
    accounts: Vec<MockAccount>,
    msgs: HashMap<AccountId, Vec<MockMsg>>,
    /// Rows currently revealed per (account, folder).
    shown: HashMap<(AccountId, String), usize>,
    synced_at: i64,
    new_mail: u32,
    added_accounts: u32,
}

/// Same public methods as `mail_service::Service`.
pub struct MockService {
    inner: Arc<Mutex<Inner>>,
    sink: Arc<dyn EventSink>,
    sign_in_pending: AtomicBool,
    sign_in_cancel: AtomicBool,
}

// ---------------------------------------------------------------------------------------------
// Seed data
// ---------------------------------------------------------------------------------------------

/// (name, address, subject, snippet): invented senders and mail.
const POOL: &[(&str, &str, &str, &str)] = &[
    ("Example Bank", "alerts@bank.example.com", "Your statement is ready", "Your March statement is now available in online banking."),
    ("Ravenwood Library", "noreply@library.example.org", "Books due Friday", "You have 2 items due this Friday. Renew online to avoid fees."),
    ("Dana Whitfield", "dana@example.net", "Lunch next week?", "Are you free on Tuesday or Wednesday? Thinking of the new place on 5th."),
    ("Orchard CI", "builds@ci.example.com", "Build #4821 passed", "All 212 checks passed on main in 3m 41s."),
    ("Northwind Travel", "bookings@travel.example.com", "Your itinerary", "Flight 118 departs 07:45. Check in opens 24 hours before departure."),
    ("Pat Okafor", "pat@example.org", "Re: meeting notes", "Thanks for sending these over. I added two action items at the bottom."),
    ("Maple Utilities", "billing@utility.example.net", "Bill available", "Your account balance is $0.00. No payment is due this month."),
    ("The Daily Digest", "digest@news.example.com", "Top stories this morning", "Five stories you may have missed, picked for you."),
    ("Sam Lindqvist", "sam@example.com", "Photos from the weekend", "Here are the photos. The one by the lake came out well."),
    ("Cloud Storage", "team@storage.example.com", "You are at 80% of your quota", "Free up space or upgrade your plan to keep syncing."),
    ("Harbor Cafe", "hello@cafe.example.org", "Your receipt", "Thanks for stopping by. Receipt attached below."),
    ("Meridian Support", "support@meridian.example.com", "Ticket #5521 updated", "We have updated your ticket. Reply to this message to add details."),
];

/// (label id, lower-case slug, rows)
const FOLDERS: &[(&str, &str, usize)] = &[
    (label::INBOX, "inbox", 30),
    (label::STARRED, "starred", 30),
    (label::SENT, "sent", 30),
    (label::DRAFT, "draft", 30),
    (label::SPAM, "spam", 30),
    (label::TRASH, "trash", 30),
    (label::CATEGORY_PERSONAL, "personal", 30),
    (label::CATEGORY_UPDATES, "updates", 30),
    (label::CATEGORY_PROMOTIONS, "promotions", 30),
    (label::CATEGORY_SOCIAL, "social", 30),
    (label::CATEGORY_FORUMS, "forums", 30),
    ("Label_1", "receipts", 12),
    ("Label_2", "projects", 12),
    ("Label_3", "reports", 12),
];

const USER_LABELS: &[(&str, &str)] = &[("Label_1", "Receipts"), ("Label_2", "Work/Projects"), ("Label_3", "Work/Reports")];

fn label_name(id: &str) -> String {
    match id {
        label::INBOX => "Inbox".into(),
        label::STARRED => "Starred".into(),
        label::SENT => "Sent".into(),
        label::DRAFT => "Drafts".into(),
        label::SPAM => "Spam".into(),
        label::TRASH => "Trash".into(),
        label::CATEGORY_PERSONAL => "Personal".into(),
        label::CATEGORY_UPDATES => "Updates".into(),
        label::CATEGORY_PROMOTIONS => "Promotions".into(),
        label::CATEGORY_SOCIAL => "Social".into(),
        label::CATEGORY_FORUMS => "Forums".into(),
        other => USER_LABELS.iter().find(|(i, _)| *i == other).map_or_else(|| other.to_string(), |(_, n)| (*n).to_string()),
    }
}

fn special(acct: usize, label_id: &str, i: usize) -> Option<Kind> {
    match (acct, label_id, i) {
        (0, label::INBOX, 1) => Some(Kind::Hostile),
        (0, label::INBOX, 2) => Some(Kind::Cid),
        (0, label::INBOX, 3) | (1, label::INBOX, 1) => Some(Kind::Attachments),
        _ => None,
    }
}

fn seed_messages(acct: usize, prefix: &str, now: i64) -> Vec<MockMsg> {
    let mut out = Vec::new();
    for (fi, (label_id, slug, count)) in FOLDERS.iter().enumerate() {
        // The first account's inbox is longer than a page, to exercise "more".
        let count = if acct == 0 && *label_id == label::INBOX { 45 } else { *count };
        for i in 0..count {
            let pool = POOL[(i + fi * 5 + acct * 3) % POOL.len()];
            let date = now - (i as i64) * 41 * 60_000 - ((acct * 7 + fi * 3) as i64) * 60_000;
            let kind = special(acct, label_id, i).unwrap_or(Kind::Plain);
            let (from_name, from_addr, mut subject, mut snippet) =
                (pool.0.to_string(), pool.1.to_string(), pool.2.to_string(), pool.3.to_string());
            match (*label_id, i, acct) {
                (label::INBOX, 0, 0) => {
                    subject = "Your verification code is 482913".into();
                    snippet = "Use 482913 to finish signing in. It expires in 10 minutes.".into();
                }
                (label::SPAM, 0, 0) => {
                    subject = "Sign-in code: 731905".into();
                    snippet = "Your one-time code is 731905. Do not share it with anyone.".into();
                }
                _ => {}
            }
            match kind {
                Kind::Hostile => {
                    subject = "Hostile test message (should stay inert)".into();
                    snippet = "Scripts, forms, remote images, base and meta refresh: none of it may run.".into();
                }
                Kind::Cid => {
                    subject = "Message with an inline image".into();
                    snippet = "An embedded picture and a blocked remote image.".into();
                }
                Kind::Attachments => {
                    subject = "Documents attached".into();
                    snippet = "Three files are attached to this message.".into();
                }
                Kind::Plain => {}
            }
            out.push(MockMsg {
                id: format!("{prefix}-{slug}-{i:02}"),
                labels: vec![(*label_id).to_string()],
                date,
                from_name,
                from_addr,
                subject,
                snippet,
                unread: match *label_id {
                    label::INBOX | label::SPAM => i % 3 == 0,
                    label::SENT | label::DRAFT => false,
                    _ => i % 4 == 0,
                },
                starred: *label_id == label::STARRED || i % 11 == 5,
                kind,
            });
        }
    }
    out
}

fn account(id: &str, email: &str, state: AccountState) -> MockAccount {
    MockAccount { id: AccountId::new(id), email: email.to_string(), state }
}

// ---------------------------------------------------------------------------------------------
// Pages and bytes
// ---------------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A static white page in the shape WP-I's renderer produces: CSP meta first, no-referrer, `_blank` base.
/// The CSP is PLAN §1 "Render"; with "Show images" it also allows `https:`/`http:` images.
fn page_head(show_images: bool) -> String {
    let remote = if show_images { " https: http:" } else { "" };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; \
         img-src {ASSET_HOST} data:{remote}; form-action 'none'; base-uri 'none'\">\
         <meta name=\"referrer\" content=\"no-referrer\"><base target=\"_blank\">\
         <style>body{{margin:0;padding:16px;background:#fff;color:#202124;font:14px/1.5 Segoe UI,Arial,sans-serif}}</style>"
    )
}

fn build_page(acct: &AccountId, m: &MockMsg, show_images: bool) -> (String, u32) {
    let head = page_head(show_images);
    match m.kind {
        Kind::Hostile => {
            // RAW hostile HTML on purpose (see the module docs): only the CSP and the sandbox hold it back.
            let hostile = mail_render::fixtures::hostile_html(CANARY);
            (format!("{head}</head><body>{hostile}"), 5)
        }
        Kind::Cid => {
            let remote = if show_images {
                format!("<p>Remote image (shown):</p><img src=\"http://{CANARY}/pixel.gif\" width=\"1\" height=\"1\" alt=\"\">")
            } else {
                String::new()
            };
            let body = format!(
                "</head><body><p>Inline image served by the gmail-asset protocol:</p>\
                 <img src=\"{ASSET_HOST}/{acct}/{id}/1\" width=\"96\" height=\"64\" alt=\"inline\">{remote}\
                 <p><a href=\"https://example.com/docs\">https link</a> \u{b7} <a href=\"mailto:someone@example.com\">mailto link</a> \u{b7} \
                 <a href=\"tel:+15555550123\">tel link</a> \u{b7} <a href=\"file:///C:/Windows/notepad.exe\">file link (must not open)</a> \u{b7} \
                 <a href=\"javascript:alert(1)\">javascript link (must not open)</a></p>",
                id = m.id
            );
            (format!("{head}{body}"), if show_images { 0 } else { 1 })
        }
        Kind::Attachments => (
            format!(
                "{head}</head><body><p>Hello,</p><p>Please find the documents attached.</p>\
                 <p>This is synthetic mock content.</p>"
            ),
            0,
        ),
        Kind::Plain => (
            format!(
                "{head}</head><body><p>{}</p><p style=\"color:#5f6368\">This is synthetic mock content.</p>",
                esc(&m.snippet)
            ),
            0,
        ),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// A small valid RGB PNG (diagonal gradient) using stored deflate blocks.
fn gradient_png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            raw.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 160]);
        }
    }
    assert!(raw.len() < 65_535, "single stored block");
    let mut z = vec![0x78, 0x01, 0x01];
    let len = raw.len() as u16;
    z.extend_from_slice(&len.to_le_bytes());
    z.extend_from_slice(&(!len).to_le_bytes());
    z.extend_from_slice(&raw);
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// (part id, file name, mime, bytes) of a message's non-inline parts.
fn attachment_parts(kind: Kind) -> Vec<(u32, &'static str, &'static str, Vec<u8>)> {
    if kind != Kind::Attachments {
        return Vec::new();
    }
    let pdf = format!("%PDF-1.4\n% synthetic mock attachment\n{}\n%%EOF\n", "mock content ".repeat(120)).into_bytes();
    vec![
        (2, "Quarterly report.pdf", "application/pdf", pdf),
        (3, "inv:oice*2026?.txt", "text/plain", b"Synthetic invoice text.\n".to_vec()),
        (4, "CON.txt", "text/plain", b"A reserved Windows device name as a file name.\n".to_vec()),
    ]
}

fn auth_for(kind: Kind) -> AuthSummary {
    match kind {
        Kind::Hostile => AuthSummary {
            line: "SPF fail \u{b7} DKIM none \u{b7} DMARC fail".into(),
            level: AuthLevel::Bad,
            mailed_by: Some("mallory.example.com".into()),
            signed_by: None,
        },
        Kind::Cid => AuthSummary {
            line: "SPF pass \u{b7} DKIM none \u{b7} DMARC pass".into(),
            level: AuthLevel::Mixed,
            mailed_by: Some("example.com".into()),
            signed_by: None,
        },
        _ => AuthSummary {
            line: "SPF pass \u{b7} DKIM pass (example.com) \u{b7} DMARC pass".into(),
            level: AuthLevel::Good,
            mailed_by: Some("example.com".into()),
            signed_by: Some("example.com".into()),
        },
    }
}

fn row_of(m: &MockMsg) -> MessageRow {
    MessageRow {
        id: MessageId::new(&m.id),
        internal_date: m.date,
        from_name: m.from_name.clone(),
        from_addr: m.from_addr.clone(),
        subject: m.subject.clone(),
        snippet: m.snippet.clone(),
        unread: m.unread,
        starred: m.starred,
        important: m.unread && m.kind == Kind::Plain,
        attachment_hint: m.kind == Kind::Attachments,
        size: 4_000 + (m.id.len() as u64) * 313,
    }
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

impl Inner {
    fn acct(&self, id: &AccountId) -> Result<&MockAccount, CmdError> {
        self.accounts.iter().find(|a| &a.id == id).ok_or_else(|| CmdError::not_found("unknown account"))
    }

    fn view(&self, a: &MockAccount) -> AccountView {
        let count = |l: &str| {
            self.msgs
                .get(&a.id)
                .map(|v| v.iter().filter(|m| m.unread && m.labels.iter().any(|x| x == l)).count() as u32)
        };
        AccountView {
            id: a.id.clone(),
            email: a.email.clone(),
            state: a.state,
            inbox_unread: count(label::INBOX),
            spam_unread: count(label::SPAM),
        }
    }

    fn status_of(&self, a: &MockAccount) -> SyncStatus {
        let state = if a.state == AccountState::NeedsReauth { SyncState::NeedsReauth } else { SyncState::Ok };
        SyncStatus { account_id: a.id.clone(), state, as_of: Some(self.synced_at) }
    }

    fn find(&self, acct: &AccountId, msg: &MessageId) -> Result<&MockMsg, CmdError> {
        self.acct(acct)?;
        self.msgs
            .get(acct)
            .and_then(|v| v.iter().find(|m| m.id == msg.as_str()))
            .ok_or_else(|| CmdError::not_found("unknown message"))
    }

    fn rows(&self, acct: &AccountId, folder: &str, limit: usize) -> (Vec<MessageRow>, bool) {
        let mut all: Vec<&MockMsg> = self
            .msgs
            .get(acct)
            .map(|v| v.iter().filter(|m| folder == label::ALL_MAIL || m.labels.iter().any(|l| l == folder)).collect())
            .unwrap_or_default();
        all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.id.cmp(&b.id)));
        let more = all.len() > limit;
        (all.into_iter().take(limit).map(row_of).collect(), more)
    }
}

impl MockService {
    /// Mirrors `Service::open`. The data dir is unused (the mock touches no disk).
    pub async fn open(_data_dir: PathBuf, sink: Arc<dyn EventSink>) -> Result<MockService, CmdError> {
        Ok(Self::new(sink, now_ms()))
    }

    fn new(sink: Arc<dyn EventSink>, now: i64) -> MockService {
        let accounts = vec![
            account("mock-alice", "alice@example.com", AccountState::SignedIn),
            account("mock-bob", "bob@example.org", AccountState::SignedIn),
            account("mock-carol", "carol@example.net", AccountState::NeedsReauth),
        ];
        let prefixes = ["m1", "m2", "m3"];
        let msgs = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.clone(), seed_messages(i, prefixes[i], now)))
            .collect();
        MockService {
            inner: Arc::new(Mutex::new(Inner {
                accounts,
                msgs,
                shown: HashMap::new(),
                synced_at: now,
                new_mail: 0,
                added_accounts: 0,
            })),
            sink,
            sign_in_pending: AtomicBool::new(false),
            sign_in_cancel: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn progress(&self, stage: SignInStage, message: Option<&str>) {
        self.sink.emit(ServiceEvent::SignInProgress(SignInProgress { stage, message: message.map(str::to_string) }));
    }

    /// Wait `ms` in 50 ms steps; false when cancelled meanwhile.
    async fn wait_unless_cancelled(&self, ms: u64) -> bool {
        for _ in 0..(ms / 50).max(1) {
            if self.sign_in_cancel.load(Ordering::Relaxed) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !self.sign_in_cancel.load(Ordering::Relaxed)
    }

    /// Simulated sign-in shared by `add_account` and `reauth`. Returns when the "browser" step is done.
    async fn simulate_sign_in(&self) -> Result<(), CmdError> {
        if self.sign_in_pending.swap(true, Ordering::SeqCst) {
            return Err(AuthError::AlreadyPending.into());
        }
        self.sign_in_cancel.store(false, Ordering::SeqCst);
        self.progress(SignInStage::Browser, None);
        let mut ok = self.wait_unless_cancelled(500).await;
        if ok {
            self.progress(SignInStage::Waiting, None);
            ok = self.wait_unless_cancelled(2_000).await;
        }
        self.sign_in_pending.store(false, Ordering::SeqCst);
        if ok {
            Ok(())
        } else {
            self.progress(SignInStage::Error, Some("cancelled"));
            Err(AuthError::Cancelled.into())
        }
    }

    pub async fn shutdown(&self) {}

    pub fn set_window_state(&self, state: WindowState) {
        tracing::debug!(?state, "mock: window state");
    }

    pub async fn accounts(&self) -> Result<Vec<AccountView>, CmdError> {
        let g = self.lock();
        Ok(g.accounts.iter().map(|a| g.view(a)).collect())
    }

    pub async fn add_account(&self, hint: Option<String>) -> Result<AccountView, CmdError> {
        self.simulate_sign_in().await?;
        let view = {
            let mut g = self.lock();
            g.added_accounts += 1;
            let n = g.added_accounts;
            let email = hint.filter(|h| h.contains('@') && h.len() < 100).unwrap_or_else(|| format!("new{n}@example.com"));
            let id = AccountId::new(format!("mock-new{n}"));
            let idx = g.accounts.len();
            g.msgs.insert(id.clone(), seed_messages(idx, &format!("n{n}"), now_ms()));
            g.accounts.push(MockAccount { id: id.clone(), email, state: AccountState::SignedIn });
            let a = g.accounts.last().expect("just pushed");
            g.view(a)
        };
        self.progress(SignInStage::Done, None);
        self.sink.emit(ServiceEvent::AccountChanged(view.clone()));
        Ok(view)
    }

    pub async fn reopen_sign_in(&self) -> Result<(), CmdError> {
        if self.sign_in_pending.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(CmdError::not_found("no sign-in is pending"))
        }
    }

    pub async fn cancel_sign_in(&self) -> Result<(), CmdError> {
        self.sign_in_cancel.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn reauth(&self, account_id: AccountId) -> Result<AccountView, CmdError> {
        self.lock().acct(&account_id)?;
        self.simulate_sign_in().await?;
        let (view, status) = {
            let mut g = self.lock();
            g.synced_at = now_ms();
            let a = g.accounts.iter_mut().find(|a| a.id == account_id).ok_or_else(|| CmdError::not_found("unknown account"))?;
            a.state = AccountState::SignedIn;
            let a = g.acct(&account_id)?;
            (g.view(a), g.status_of(a))
        };
        self.progress(SignInStage::Done, None);
        self.sink.emit(ServiceEvent::AccountChanged(view.clone()));
        self.sink.emit(ServiceEvent::SyncStatus(status));
        Ok(view)
    }

    pub async fn sign_out(&self, account_id: AccountId) -> Result<SignOutResult, CmdError> {
        {
            let mut g = self.lock();
            g.acct(&account_id)?;
            g.accounts.retain(|a| a.id != account_id);
            g.msgs.remove(&account_id);
            g.shown.retain(|(a, _), _| a != &account_id);
        }
        // `revoked: true` would be the normal outcome; the mock never talks to Google.
        Ok(SignOutResult { revoked: true, manual_removal_needed: false })
    }

    pub async fn folders(&self, account_id: AccountId) -> Result<Vec<FolderView>, CmdError> {
        let g = self.lock();
        g.acct(&account_id)?;
        let labels: Vec<LabelInfo> = USER_LABELS
            .iter()
            .map(|(id, name)| LabelInfo { id: LabelId::new(*id), name: (*name).to_string(), kind: LabelKind::User })
            .collect();
        let msgs = g.msgs.get(&account_id);
        let mut tree = build_tree(&labels);
        for f in &mut tree {
            f.unread = match f.kind {
                FolderKind::AllMail | FolderKind::CategoryGroup | FolderKind::Group | FolderKind::Drafts | FolderKind::Trash => None,
                _ => msgs.map(|v| v.iter().filter(|m| m.unread && m.labels.contains(&f.id)).count() as u32),
            };
        }
        Ok(tree)
    }

    pub async fn messages(&self, account_id: AccountId, folder_id: String, more: bool) -> Result<ListResult, CmdError> {
        let mut g = self.lock();
        g.acct(&account_id)?;
        let key = (account_id.clone(), folder_id.clone());
        let shown = if more { g.shown.get(&key).copied().unwrap_or(PAGE) + PAGE } else { PAGE };
        let (rows, has_more) = g.rows(&account_id, &folder_id, shown);
        g.shown.insert(key, shown);
        Ok(ListResult { rows, more: has_more, end_of_cache: false })
    }

    pub async fn message(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        show_images: bool,
    ) -> Result<MessageView, CmdError> {
        let g = self.lock();
        let m = g.find(&account_id, &message_id)?;
        let email = g.acct(&account_id)?.email.clone();
        let (page, blocked) = build_page(&account_id, m, show_images);
        let attachments = attachment_parts(m.kind)
            .into_iter()
            .map(|(id, name, mime, bytes)| AttachmentInfo {
                part_id: PartId(id),
                name: name.to_string(),
                mime: mime.to_string(),
                size: bytes.len() as u64,
            })
            .collect();
        Ok(MessageView {
            id: m.id.clone().into(),
            subject: m.subject.clone(),
            from: Addr { name: Some(m.from_name.clone()), addr: m.from_addr.clone() },
            to: vec![Addr { name: None, addr: email }],
            cc: if m.kind == Kind::Cid { vec![Addr { name: Some("Dana Whitfield".into()), addr: "dana@example.net".into() }] } else { Vec::new() },
            reply_to: (m.kind == Kind::Hostile).then(|| Addr { name: None, addr: "replies@elsewhere.example.org".into() }),
            date: m.date,
            labels: m.labels.iter().map(|l| label_name(l)).collect(),
            auth: auth_for(m.kind),
            attachments,
            page,
            remote_images_blocked: blocked,
            images_shown: show_images,
            cached: false,
        })
    }

    pub async fn refresh(&self, account_id: AccountId) -> Result<(), CmdError> {
        {
            let g = self.lock();
            if g.acct(&account_id)?.state == AccountState::NeedsReauth {
                return Err(AuthError::NeedsReauth { reason: "mock: sign-in expired".into() }.into());
            }
        }
        self.sink.emit(ServiceEvent::SyncStatus(SyncStatus {
            account_id: account_id.clone(),
            state: SyncState::Syncing,
            as_of: Some(self.lock().synced_at),
        }));
        let (inner, sink) = (self.inner.clone(), self.sink.clone());
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let now = now_ms();
            let status = {
                let mut g = inner.lock().unwrap_or_else(|e| e.into_inner());
                if !g.accounts.iter().any(|a| a.id == account_id) {
                    return;
                }
                g.new_mail += 1;
                let n = g.new_mail;
                g.synced_at = now;
                let code = 100_000 + (n * 7919) % 900_000;
                if let Some(v) = g.msgs.get_mut(&account_id) {
                    v.push(MockMsg {
                        id: format!("new-{n:03}"),
                        labels: vec![label::INBOX.to_string()],
                        date: now,
                        from_name: "Example Security".into(),
                        from_addr: "security@example.com".into(),
                        subject: format!("Your verification code is {code}"),
                        snippet: format!("Use {code} to finish signing in. It expires in 10 minutes."),
                        unread: true,
                        starred: false,
                        kind: Kind::Plain,
                    });
                }
                g.acct(&account_id).ok().map(|a| g.status_of(a))
            };
            sink.emit(ServiceEvent::MailChanged(MailChanged {
                account_id: account_id.clone(),
                labels: vec![label::INBOX.to_string()],
            }));
            if let Some(s) = status {
                sink.emit(ServiceEvent::SyncStatus(s));
            }
        });
        Ok(())
    }

    pub async fn set_view(&self, account_id: AccountId, folder_id: String) -> Result<(), CmdError> {
        self.lock().acct(&account_id)?;
        tracing::debug!(folder = %folder_id, "mock: set_view");
        Ok(())
    }

    pub async fn save_attachment(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
        saver: &dyn FileSaver,
    ) -> Result<SaveResult, CmdError> {
        let (name, bytes) = {
            let g = self.lock();
            let m = g.find(&account_id, &message_id)?;
            attachment_parts(m.kind)
                .into_iter()
                .find(|p| p.0 == part_id.0)
                .map(|(_, name, _, bytes)| (name.to_string(), bytes))
                .ok_or_else(|| CmdError::not_found("unknown attachment"))?
        };
        saver.save(&name, &bytes)
    }

    pub async fn status(&self) -> Result<Vec<SyncStatus>, CmdError> {
        let g = self.lock();
        Ok(g.accounts.iter().map(|a| g.status_of(a)).collect())
    }

    pub async fn asset(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
    ) -> Result<Asset, CmdError> {
        let g = self.lock();
        let m = g.find(&account_id, &message_id)?;
        if m.kind == Kind::Cid && part_id.0 == 1 {
            return Ok(Asset { bytes: gradient_png(96, 64), headers: asset_headers("image/png") });
        }
        attachment_parts(m.kind)
            .into_iter()
            .find(|p| p.0 == part_id.0)
            .map(|(_, _, mime, bytes)| Asset { bytes, headers: asset_headers(mime) })
            .ok_or_else(|| CmdError::not_found("unknown part"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<ServiceEvent>>);

    impl EventSink for Collect {
        fn emit(&self, event: ServiceEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Saver(Mutex<Vec<(String, usize)>>);

    impl FileSaver for Saver {
        fn save(&self, name: &str, bytes: &[u8]) -> Result<SaveResult, CmdError> {
            self.0.lock().unwrap().push((name.to_string(), bytes.len()));
            Ok(SaveResult { saved: true, file_name: Some(name.to_string()) })
        }
    }

    fn svc() -> (MockService, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        (MockService::new(sink.clone(), now_ms() - 3_600_000), sink)
    }

    fn id(s: &str) -> AccountId {
        AccountId::new(s)
    }

    #[tokio::test]
    async fn three_accounts_one_needs_reauth() {
        let (s, _) = svc();
        let a = s.accounts().await.unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a.iter().filter(|a| a.state == AccountState::NeedsReauth).count(), 1);
        assert!(a[0].inbox_unread.unwrap() > 0 && a[0].spam_unread.unwrap() > 0);
    }

    #[tokio::test]
    async fn every_folder_has_rows_and_ids_fit_the_asset_path_rules() {
        let (s, _) = svc();
        for a in ["mock-alice", "mock-bob", "mock-carol"] {
            let tree = s.folders(id(a)).await.unwrap();
            assert!(tree.iter().any(|f| f.kind == FolderKind::Group), "Work group present");
            for f in tree.iter().filter(|f| f.clickable) {
                let r = s.messages(id(a), f.id.clone(), false).await.unwrap();
                assert!(r.rows.len() >= 12, "{a} {}", f.id);
                for row in &r.rows {
                    let path = format!("/{a}/{}/1", row.id);
                    assert!(crate::assets::parse_asset_path(&path).is_some(), "{path}");
                }
                assert!(r.rows.windows(2).all(|w| w[0].internal_date >= w[1].internal_date));
            }
        }
    }

    #[tokio::test]
    async fn paging() {
        let (s, _) = svc();
        let first = s.messages(id("mock-alice"), "INBOX".into(), false).await.unwrap();
        assert_eq!((first.rows.len(), first.more), (PAGE, true));
        let second = s.messages(id("mock-alice"), "INBOX".into(), true).await.unwrap();
        assert_eq!((second.rows.len(), second.more), (45, false));
        let all = s.messages(id("mock-alice"), "ALL".into(), false).await.unwrap();
        assert!(all.more);
    }

    #[tokio::test]
    async fn hostile_message_is_wrapped_in_the_page_csp() {
        let (s, _) = svc();
        let v = s.message(id("mock-alice"), "m1-inbox-01".into(), false).await.unwrap();
        assert!(v.page.starts_with("<!doctype html><html><head>"));
        let csp = v.page.find("Content-Security-Policy").unwrap();
        assert!(csp < v.page.find("<script").unwrap(), "CSP comes before any hostile element");
        assert!(v.page.contains("img-src http://gmail-asset.localhost data:;"));
        assert_eq!(v.auth.level, AuthLevel::Bad);
        assert!(v.remote_images_blocked > 0 && !v.images_shown);
        let shown = s.message(id("mock-alice"), "m1-inbox-01".into(), true).await.unwrap();
        assert!(shown.page.contains("https: http:") && shown.images_shown);
    }

    #[tokio::test]
    async fn cid_image_is_served_as_a_png() {
        let (s, _) = svc();
        let v = s.message(id("mock-alice"), "m1-inbox-02".into(), false).await.unwrap();
        assert!(v.page.contains("http://gmail-asset.localhost/mock-alice/m1-inbox-02/1"));
        assert!(v.attachments.is_empty(), "inline images are not listed");
        let a = s.asset(id("mock-alice"), "m1-inbox-02".into(), PartId(1)).await.unwrap();
        assert_eq!(&a.bytes[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(a.headers.contains(&("Content-Type", "image/png".to_string())));
        assert!(s.asset(id("mock-alice"), "m1-inbox-02".into(), PartId(9)).await.is_err());
        assert!(s.asset(id("mock-alice"), "m1-inbox-00".into(), PartId(1)).await.is_err());
    }

    #[tokio::test]
    async fn attachments_list_save_and_download_headers() {
        let (s, _) = svc();
        let v = s.message(id("mock-alice"), "m1-inbox-03".into(), false).await.unwrap();
        assert_eq!(v.attachments.len(), 3);
        let saver = Saver(Mutex::new(Vec::new()));
        let r = s.save_attachment(id("mock-alice"), "m1-inbox-03".into(), PartId(2), &saver).await.unwrap();
        assert!(r.saved);
        assert_eq!(saver.0.lock().unwrap()[0].0, "Quarterly report.pdf");
        assert!(s.save_attachment(id("mock-alice"), "m1-inbox-03".into(), PartId(77), &saver).await.is_err());
        let a = s.asset(id("mock-alice"), "m1-inbox-03".into(), PartId(2)).await.unwrap();
        assert!(a.headers.contains(&("Content-Disposition", "attachment".to_string())));
    }

    #[tokio::test]
    async fn png_is_structurally_valid() {
        let png = gradient_png(96, 64);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[tokio::test]
    async fn refresh_adds_mail_and_emits() {
        let (s, sink) = svc();
        assert!(s.refresh(id("mock-carol")).await.is_err(), "needs reauth");
        s.refresh(id("mock-alice")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(900)).await;
        let ev = sink.0.lock().unwrap().clone();
        assert!(ev.iter().any(|e| matches!(e, ServiceEvent::MailChanged(c) if c.labels == ["INBOX"])));
        assert!(ev.iter().any(|e| matches!(e, ServiceEvent::SyncStatus(st) if st.state == SyncState::Ok)));
        let rows = s.messages(id("mock-alice"), "INBOX".into(), false).await.unwrap();
        assert!(rows.rows[0].subject.starts_with("Your verification code is"));
    }

    #[tokio::test]
    async fn sign_in_flow_can_be_cancelled_and_completed() {
        let (s, sink) = svc();
        let s = Arc::new(s);
        let s2 = s.clone();
        let h = tokio::spawn(async move { s2.add_account(None).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(s.add_account(None).await.is_err(), "one pending sign-in");
        s.cancel_sign_in().await.unwrap();
        let err = h.await.unwrap().unwrap_err();
        assert_eq!(err.detail.as_deref(), Some("cancelled"));
        let v = s.add_account(Some("zed@example.com".into())).await.unwrap();
        assert_eq!(v.email, "zed@example.com");
        assert_eq!(s.accounts().await.unwrap().len(), 4);
        let ev = sink.0.lock().unwrap().clone();
        assert!(ev.iter().any(|e| matches!(e, ServiceEvent::AccountChanged(_))));
        assert!(ev.iter().any(|e| matches!(e, ServiceEvent::SignInProgress(p) if p.stage == SignInStage::Done)));
    }

    #[tokio::test]
    async fn reauth_and_sign_out() {
        let (s, _) = svc();
        let v = s.reauth(id("mock-carol")).await.unwrap();
        assert_eq!(v.state, AccountState::SignedIn);
        let r = s.sign_out(id("mock-bob")).await.unwrap();
        assert!(r.revoked && !r.manual_removal_needed);
        assert_eq!(s.accounts().await.unwrap().len(), 2);
        assert!(s.folders(id("mock-bob")).await.is_err());
    }
}
