# starmail: build plan

A native Windows app (Rust + Tauri 2) that signs into **several Gmail accounts** and shows their mail **read-only**. The main job is: open the app, see the newest mail (often a sign-in code), and read it.

This plan is written to be run by **many Sonnet agents working in parallel**:
- **Wave 0** fixes every crate boundary and public API.
- **Wave 1** has 7 independent work packages (WPs), each owning its own files.
- **Wave 2** wires them together.

Each WP section is a self-contained brief. An agent should need only `CLAUDE.md`, this file, the spike files it names, and its own crate.

Evidence for every decision is in `spikes\sN-*\FINDINGS.md`.

---

## 1. Fixed decisions (do not revisit inside a WP)

| Topic | Decision | Source |
|---|---|---|
| API | Gmail REST v1 over plain `reqwest` 0.12 (rustls, **http2**) + serde. Requests GET only, plus the batch POST, whose sub-requests are GETs. | S0, S2 |
| Scopes | `gmail.readonly` + `openid email`. The granted `scope` is checked on every sign-in. | S0, S1 |
| Sign-in | System browser + loopback `127.0.0.1:<ephemeral>` + PKCE S256 + `state` + `access_type=offline` + `prompt=consent select_account` + `login_hint`. One pending sign-in app-wide. Cancel and a 5-minute timeout. Raise the window afterwards. **Never in a webview.** | S0, S1 |
| Granular consent | Gmail is often left **unticked** (2 of 5 real sign-ins). Show the instruction "On Google's page, tick *Read all your email*" **before** opening the browser. If the scope is still missing: store nothing, explain, and offer one-click retry. | S0, S1 |
| Publishing | The project stays in **Testing**, so refresh tokens die after **7 days**. `needs_reauth` must be painless: a per-account banner with a one-click "Sign in again" (`login_hint`), the cache stays readable, and that account stops polling. Production comes later, when branding pages exist. | S1, user |
| Token store | `%LOCALAPPDATA%\com.ziggy.starmail\accounts\<sub>.dpapi`: DPAPI (CurrentUser, UI forbidden, entropy `starmail-token`) of `{email, sub, refresh_token, scopes, created_at, needs_reauth}`. Written via tmp + rename. A file that fails to decrypt is skipped, not fatal. Access tokens are in memory only, with `invalidate()`. | S0, S1, S4 |
| Sign-out | `POST https://oauth2.googleapis.com/revoke` (form body), then **always** delete locally (token file + cache directory). If the revoke fails, tell the user to remove access at myaccount.google.com → Security → Third-party apps. `400 invalid_token` means it was already gone. **A revoke kills the whole grant for that account** (every token from this client). | S1 |
| Auth errors | Gmail 401: invalidate the token, refresh once, retry once. `invalid_grant` or 403 `PERMISSION_DENIED` (scope) → `needs_reauth`. A network error is "offline" and doesn't change the account state. | S1, S4 |
| Quota | `messages.get` **20** units (raw 60), `messages.list` 5, `history.list` 2, `labels.*`/profile 1. **6,000 units/min per user.** Throttling arrives as **403 `rateLimitExceeded`** (or 429), usually without Retry-After. Back off as in §3.6. Never fetch more than ~5 pages of 50 per minute per account. | S2, S4 |
| Folders | Fixed order: Inbox · Starred · Sent · Drafts · All Mail · Spam · Trash · Categories (collapsible: Personal, Updates, Promotions, Social, Forums) · user labels nested by `/` (a parent that exists only as a prefix becomes a non-clickable group). Hidden: UNREAD, IMPORTANT, CHAT, YELLOW_STAR. Ignore the API's visibility fields. "Primary" = `CATEGORY_PERSONAL` (`category:primary` returns nothing). Counts come from `labels.get` for the visible system folders, lazily for user labels. Counts can be wrong: show the unread count only. | S2 |
| Spam | Sign-in codes land in Spam (S4's live test mail did). Show Spam's unread count in the folder list, like Inbox. | S4 |
| Lists | **Messages, not threads.** `messages.list(labelIds, maxResults=50)` → ids. Uncached ids are hydrated with **one batch request** of `messages.get?format=metadata&metadataHeaders=From,To,Subject,Date`, with the first 20 ids as their own batch for speed. **Sort by `internalDate` on the client** (Gmail's order has inversions). Decode HTML entities in the snippet. The paperclip hint is `payload.mimeType == multipart/mixed`. Headers arrive already RFC 2047-decoded. | S2 |
| Freshness | **Polling `history.list`** (2 units). There is no push and never `users.watch`. Cadence: focused, 5 s for the active account and 15 s for others; visible but unfocused, 30 s; minimized, 60 s. Also poll on focus or restore, with ±20% jitter per account. Manual Refresh: one poll in flight per account; a click during a poll queues exactly one trailing poll. A history 404 → resync. New mail typically appears about 16 s after arrival; most of that is Gmail's own delay. | S4 |
| Cache | **Fetch-through, per account:** `cache\<sub>\cache.db` (SQLite, WAL) + `key.dpapi` (32-byte key). Headers, snippet and raw bodies are **ChaCha20-Poly1305 blobs** with a random nonce and the row id as AAD. Ids, label ids, dates and sizes are in the clear. Rows are immutable: only labels change. Raw bodies over 5 MB are shown but not stored. Eviction: bodies capped at 200 MB per account (LRU, but always keep the last 7 days opened and the newest 20 in INBOX), and list rows capped at 2,000 per label. Schema in §3.3. | S4 |
| Offline | Cached rows and bodies stay readable. Banner: "Offline: showing cached mail as of …". A partial window ends with "end of cached mail". | S4 |
| Message fetch | `messages.get?format=raw`, base64url-decoded. Parsed with **`mail-parser` 1.0**, plus the C1/windows-1252 repair and `text_items` for inline images in text. `attachments.get` is never needed. Asset `partId` = mail-parser part index. | S3 |
| Render | HTML if present, otherwise text → HTML. **`ammonia`** with S3's exact config (tags, attribute filter, `scrub_css`, `<body>` lifting). Page CSP `default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action 'none'; base-uri 'none'`, plus the iframe `csp=` attribute as a second layer. `<iframe sandbox="allow-popups" srcdoc>` (variant B): link clicks go to Rust `on_new_window`, which checks the scheme (http, https, mailto, tel) and opens them with the OS, never in-app. | S3 |
| Images | Remote images are **blocked in the sanitizer** by default. "Show images" re-renders that one message with `ImagePolicy::Allow` and `img-src … https: http:`. There is no proxy. CSS background URLs follow the same policy. `cid:` images → `http://gmail-asset.localhost/<sub>/<msgId>/<partId>`. | S3 |
| Theme | **The app UI is always dark.** **Email HTML always renders on white**, with no inversion. Plain-text emails render in our own HTML, also on white, for consistency inside the reading pane. | user, S3 |
| Header block | From (name + address), To, Cc, Reply-To (only if it differs), Subject, Date (local + relative; `Date` header, falling back to `internalDate`), and labels. **Sender check:** the first `Authentication-Results` header with a `*.google.com` authserv-id only → `SPF pass · DKIM pass (domain) · DMARC pass`, with a good/mixed/bad badge. | S3 |
| Attachments | List name, MIME type and size. Images used inline are not listed. **"Save…" is an explicit user action only:** a native save dialog, prefilled with the file name, opening in the **last folder used** (persisted). It defaults to Downloads the first time. Rust writes the bytes. JS never receives them. Nothing is ever saved automatically. | user, S3 |
| Out of scope (v1) | Threads view, search, push, notifications, compose/reply/any write, Production publishing, unified all-accounts view. | user |
| UI tech | Vanilla HTML/CSS/JS (no Node), cargo only. Tauri 2.12, WebView2. | CLAUDE.md |

**Privacy rules for every WP** (from CLAUDE.md):
- Never log tokens, codes or Authorization headers.
- Never log subjects, senders, addresses, snippets or bodies. Log ids, counts, sizes, label ids (system ones only), timings and status codes.
- Test fixtures are synthetic.
- Tests against real mail are `#[ignore]` and print stats only.

---

## 2. Repository layout (created in Wave 0)

```
starmail\
  Cargo.toml                 workspace (resolver 2), shared [workspace.dependencies]
  PLAN.md  SPIKES.md  CLAUDE.md
  crates\
    mail-model\              shared plain types, traits, errors, command DTOs (no I/O)
    win-dpapi\               protect/unprotect (complete in Wave 0)
    gauth\                   OAuth sign-in, token store, access-token cache, revoke
    gmail-api\               HTTP client: GETs + batch, error classification
    mail-cache\              per-account encrypted SQLite cache, eviction, deletion
    mail-sync\               poll scheduler, PollGate/Backoff, history deltas, folder fill, resync
    mail-render\             MIME parse, body choice, sanitize, page wrap, auth summary, attachments
    mail-service\            facade the app calls (Wave 2)
  app\
    src-tauri\               Tauri host: commands, gmail-asset protocol, dialogs, link opening, focus tracking
    ui\                      index.html, app.js, app.css (dark), mock.js
  spikes\                    read-only reference; never edited by WPs
```

Dependency graph (acyclic, enforced by Cargo):
```
mail-model ← win-dpapi(none)          win-dpapi ← gauth, mail-cache
mail-model ← gauth ← gmail-api        (gmail-api needs a TokenSource; gauth implements it)
mail-model ← mail-cache
mail-model ← mail-sync                (only via the GmailApi + MailCache traits)
mail-model ← mail-render
everything ← mail-service ← app/src-tauri
app/ui ↔ app/src-tauri only through §3.7
```

---

## 3. Wave 0: skeleton and contracts (one agent, sequential, must finish first)

**Goal:** a workspace that compiles, where every public item below exists with its final signature, a doc comment, and a `todo!()` body (except `mail-model` and `win-dpapi`, which are complete).

Steps:
1. Workspace `Cargo.toml`.
   - Pin `[workspace.dependencies]`: tokio 1, reqwest 0.12 (default-features=false; rustls-tls, json, http2, charset), serde/serde_json, thiserror 2, tracing, rusqlite 0.41 (bundled), chacha20poly1305 0.10, getrandom 0.3, sha2 0.10, base64 0.23, mail-parser 1.0 (full_encoding), ammonia 4, windows 0.62 (Win32_Foundation, Win32_Security_Cryptography, Win32_UI_Shell, Win32_UI_WindowsAndMessaging), tauri 2.12, tauri-build 2, tauri-plugin-dialog 2, tauri-plugin-opener 2, httpmock 0.9 (dev).
   - Run `cargo generate-lockfile` once.
2. Create every crate in §2 with stubs. Copy `spikes\s0-token\src\dpapi.rs` into `win-dpapi` and finish it, taking the entropy as a parameter.
3. Write the contracts below. `cargo build --workspace` and `cargo test --workspace` must pass.
4. Scaffold `app\` with `cargo create-tauri-app -m cargo -t vanilla`.
   - Point `frontendDist` at `..\ui`.
   - Identifier `com.ziggy.starmail`.
   - Register the §3.7 commands as stubs.
   - The capability grants `core:default` plus `dialog:allow-save` only.
5. Commit as "skeleton". Each Wave 1 agent works in **its own git worktree/branch** `wp-<letter>` off this commit.

### 3.1 `mail-model` (complete in Wave 0)
- **Ids:**
  - `AccountId(String)` (Google `sub`)
  - `MessageId(String)`, `ThreadId(String)`, `LabelId(String)`, `PartId(u32)`
  - `HistoryId(u64)`
  - Constants for the system label ids: `INBOX`, `STARRED`, `SENT`, `DRAFT`, `SPAM`, `TRASH`, `UNREAD`, `IMPORTANT`, `CATEGORY_*`.
- **Gmail shapes:**
  - `LabelInfo { id, name, kind: System|User }`
  - `LabelCounts { total: Option<u32>, unread: Option<u32> }`
  - `ListPage { ids: Vec<(MessageId, ThreadId)>, next_page_token: Option<String> }`
  - `MessageMeta { id, thread_id, label_ids: Vec<LabelId>, internal_date_ms: i64, size_estimate: u64, history_id: HistoryId, mime_type: String, from_raw: String, to_raw: String, subject: String, date_raw: String, snippet_raw: String }`
  - `HistoryPage { events: Vec<HistoryEvent>, history_id: HistoryId, next_page_token: Option<String> }`
  - `HistoryEvent = Added{id, thread_id, label_ids} | Deleted{id} | LabelsAdded{id, label_ids} | LabelsRemoved{id, label_ids}`
  - `Profile { email, messages_total, history_id }`
- **Errors:**
  - `AuthError = NotSignedIn | NeedsReauth{reason} | MissingScope{granted} | Cancelled | Timeout | Denied | AlreadyPending | StateMismatch | Browser(String)`
  - `ApiError = Auth(AuthError) | RateLimited{retry_after_s: Option<u64>} | HistoryExpired | NotFound | Offline(String) | Status{code: u16, reason: String} | Decode(String)`
  - `CacheError = Db(String) | Io(String) | Crypto(String) | Locked`
  - `RenderError = Parse(String)`
  - `CmdError { code: CmdErrorCode(auth|offline|throttled|not_found|internal), message }`, with `From` for each error type.
- **Traits** (RPITIT with `+ Send` futures):
  ```rust
  pub trait TokenSource { fn access_token(&self, force_refresh: bool) -> impl Future<Output=Result<String, AuthError>> + Send; }
  pub trait GmailApi {
      fn profile(&self) -> impl Future<Output=Result<Profile, ApiError>> + Send;
      fn labels(&self) -> impl Future<Output=Result<Vec<LabelInfo>, ApiError>> + Send;
      fn label_counts(&self, id: &LabelId) -> impl Future<Output=Result<LabelCounts, ApiError>> + Send;
      fn list(&self, label: Option<&LabelId>, page_token: Option<&str>, max: u32) -> impl Future<Output=Result<ListPage, ApiError>> + Send; // sets includeSpamTrash for SPAM/TRASH
      fn metadata_batch(&self, ids: &[MessageId]) -> impl Future<Output=Result<Vec<Result<MessageMeta, ApiError>>, ApiError>> + Send; // ≤50
      fn raw(&self, id: &MessageId) -> impl Future<Output=Result<Vec<u8>, ApiError>> + Send; // decoded RFC 822
      fn history(&self, start: HistoryId, page_token: Option<&str>) -> impl Future<Output=Result<HistoryPage, ApiError>> + Send; // 404 → HistoryExpired
  }
  pub trait MailCache { /* §3.3 */ }
  ```
- **View and command types** (`view.rs`, camelCase JSON):
  - `AccountView { id, email, state: signedIn|needsReauth, inboxUnread: Option<u32>, spamUnread: Option<u32> }`
  - `FolderView { id: String, name, kind: inbox|starred|sent|drafts|allMail|spam|trash|categoryGroup|category|user|group, depth: u8, unread: Option<u32>, clickable: bool }` (All Mail's id is `"ALL"`)
  - `MessageRow { id, internalDate: i64, fromName, fromAddr, subject, snippet, unread, starred, important, attachmentHint, size }` (snippet entity-decoded)
  - `ListResult { rows: Vec<MessageRow>, more: bool, endOfCache: bool }`
  - `MessageView { id, subject, from: Addr, to: Vec<Addr>, cc: Vec<Addr>, replyTo: Option<Addr>, date: i64, labels: Vec<String>, auth: AuthSummary, attachments: Vec<AttachmentInfo>, page: String /*srcdoc*/, remoteImagesBlocked: u32, imagesShown: bool, cached: bool }`
  - `Addr { name: Option<String>, addr: String }`
  - `AuthSummary { line: String, level: good|mixed|bad|none, mailedBy: Option<String>, signedBy: Option<String> }`
  - `AttachmentInfo { partId: u32, name, mime, size: u64 }`
  - `SyncStatus { accountId, state: ok|syncing|offline|throttled|needsReauth, asOf: Option<i64> }`
  - `SaveResult { saved: bool, fileName: Option<String> }` (never the full path in logs)
- Constants: `APP_ID = "com.ziggy.starmail"`, `ASSET_HOST = "http://gmail-asset.localhost"`, `MAX_CACHED_BODY = 5 * 1024 * 1024`.

### 3.2 `gauth` public API
- `Client::load(path)` reads `%LOCALAPPDATA%\com.ziggy.starmail\oauth-client.json`. The user copies it there from the spike folder.
- `struct Store` (`open(dir)`, `list() -> (Vec<StoredAccount>, skipped: usize)`, `save`, `set_needs_reauth(&AccountId, bool)`, `delete(&AccountId)`)
- `struct PendingLogin` (`url()`, `reopen_browser()`, `cancel()`)
- `async fn begin_login(client, hint: Option<&str>) -> Result<PendingLogin, AuthError>`
- `async fn complete_login(PendingLogin, timeout) -> Result<StoredAccount, AuthError>`. It enforces one pending login via a global slot, and checks scopes.
- `struct Tokens` implements `TokenSource` per account. It caches tokens with `expires_in − 60 s`, `invalidate()`, and one refresh at a time per account. `invalid_grant` → `NeedsReauth`, and the flag is persisted.
- `async fn revoke(refresh_token) -> RevokeOutcome { Revoked | AlreadyInvalid | Failed(String) }`
- `async fn tokeninfo_scopes(access_token) -> Result<Vec<String>, AuthError>` (POST form body)

### 3.3 `mail-cache` public API
- `CacheRoot::open(dir)`:
  - `account(&AccountId) -> Result<AccountCache, CacheError>` creates the directory, `key.dpapi` and the schema.
  - `delete_account(&AccountId)` renames to `<id>.deleting-N`, then runs `remove_dir_all`.
  - `sweep()` runs at startup.
- **Schema:**
  ```sql
  meta(k PRIMARY KEY, v)                                  -- history_id, synced_at, schema_version
  msgs(id PK, thread_id, internal_date, size_estimate, mime_type, fetched_at, stale INT, meta BLOB)  -- meta = AEAD(JSON MessageMeta text fields)
  msg_labels(id, label, PRIMARY KEY(id,label)) WITHOUT ROWID;  CREATE INDEX msg_labels_label ON msg_labels(label, id)
  bodies(id PK, raw BLOB, fetched_at, last_opened)        -- AEAD(raw RFC 822)
  windows(label PK, oldest_date, complete INT, next_page_token, as_of)
  ```
- `AccountCache` implements `mail_model::MailCache`:
  - `cursor`, `set_cursor`
  - `upsert_meta(&[MessageMeta])`
  - `rows(label, before: Option<i64>, limit) -> Vec<MessageMeta>`, sorted by `internal_date` descending, with stale rows excluded unless `offline`
  - `has(ids) -> set`
  - `apply_events(&[HistoryEvent]) -> Vec<MessageId>` (the ids to fetch, per S4's admit rule)
  - `window(label)`, `set_window`
  - `mark_all_stale_and_drop_windows`
  - `body(id)`, `put_body(id, raw)` (skipped above `MAX_CACHED_BODY`), `touch_opened`
  - `label_ids(id)`
  - `evict()`
  - `close()`
  - `synced_at`
- Port `spikes\s4-sync\src\{store,delta,signout,dpapi}.rs` and the AEAD variant (d) from `bench.rs`.

### 3.4 `gmail-api` public API
- `GmailClient::new(tokens: impl TokenSource, base: Option<Url>)` (the base is for httpmock) implements `GmailApi`.
  - On 401 it calls `access_token(force_refresh=true)` once and retries.
  - It classifies errors:
    - 403 `rateLimitExceeded`/`userRateLimitExceeded` or 429 → `RateLimited`
    - 404 on history → `HistoryExpired`
    - 403 `PERMISSION_DENIED`/`insufficientPermissions` → `Auth(NeedsReauth)`
    - transport errors → `Offline`
- `metadata_batch` uses `POST https://gmail.googleapis.com/batch/gmail/v1` (multipart/mixed, GET sub-requests) and parses each sub-response status. If the batch itself fails, fall back to concurrent GETs at concurrency 20.
- `raw` uses `format=raw`.
- `history` uses `historyTypes=messageAdded,messageDeleted,labelAdded,labelRemoved&maxResults=500`.
- **A test that greps this crate's source for forbidden path segments** (`/modify`, `/trash`, `/untrash`, `/send`, `/insert`, `/import`, `/batchModify`, `/batchDelete`, `/watch`, `/stop`, `/drafts`, `/settings`, `"DELETE"`, and `labels` with `POST`/`PUT`/`PATCH`) fails the build if any appear.
- Port from `spikes\s2-list\src\lib.rs` (batch framing, address parsing, entity decoding) and `spikes\s4-sync\src\api.rs` (classify).

### 3.5 `mail-render` public API
- `fn parse(raw: &[u8]) -> Result<Parsed, RenderError>`. `Parsed` holds the headers, html/text/`text_items`, the attachments, the cid map, `Authentication-Results`, and the parts by index.
- `fn render(parsed: &Parsed, account: &AccountId, msg: &MessageId, images: ImagePolicy) -> Rendered { page: String, remote_images_blocked: u32 }`. The page is on white and includes the CSP meta, `<base target=_blank>` and referrer no-referrer.
- `fn auth_summary(parsed) -> AuthSummary`, `fn header_block(parsed) -> (Addr, Vec<Addr>, …)`, `fn attachments(parsed) -> Vec<AttachmentInfo>`.
- `fn part(parsed, PartId) -> Option<(Vec<u8>, mime: String, file_name: Option<String>)>` serves assets and saves.
- `fn asset_headers(mime) -> Vec<(&str, String)>`: inline-safe raster images only, otherwise `octet-stream` + `attachment`, plus `nosniff` and a sandbox CSP.
- `fn snippet_text(raw: &str) -> String` decodes entities. `fn parse_from(raw) -> Addr`.
- Port `spikes\s3-render\src\{parse,auth,text,sanitize,render}.rs` and its fixtures and tests.

### 3.6 `mail-sync` public API
- `PollGate`, `Backoff`, `Cadence { focused_active: 5s, focused_other: 15s, unfocused: 30s, minimized: 60s, jitter: 0.2 }`. Port `spikes\s4-sync\src\gate.rs`.
- `async fn poll<A: GmailApi, C: MailCache>(api, cache) -> Result<PollReport { added, changed, removed }, ApiError>`. It follows `nextPageToken`, applies events, hydrates the ids to fetch via `metadata_batch`, and advances the cursor only after success. On `HistoryExpired` it calls `resync`.
- `async fn cold_fill(api, cache)`: `profile` (for the cursor) **before** `list(INBOX, 50)`, then hydrate (first 20, then 30).
- `async fn fill_folder(api, cache, label, more: bool) -> Result<(), ApiError>`. It lists, skips cached ids, hydrates, and updates the window. `more` continues from the window's `next_page_token`.
- `async fn resync(api, cache)`: mark everything stale, drop the windows, take a new cursor, refill INBOX.
- `async fn folder_counts(api, labels) -> Vec<(LabelId, LabelCounts)>`, concurrency 7.
- Backoff:
  - `RateLimited`: 30 s doubling, capped at 600 s (honour Retry-After).
  - `Offline` or 5xx: 5 s doubling, capped at 300 s.
  - `Auth(NeedsReauth)`: stop.

### 3.7 Tauri command contract (`app/src-tauri` ↔ `app/ui`)
All commands are async and return JSON. Failures are `CmdError`.

| Command | Args | Returns |
|---|---|---|
| `accounts` | – | `AccountView[]` |
| `add_account` | `{hint?}` | `AccountView`. Resolves when sign-in completes. |
| `reopen_sign_in` / `cancel_sign_in` | – | – |
| `reauth` | `{accountId}` | `AccountView` (uses `login_hint`) |
| `sign_out` | `{accountId}` | `{revoked: bool, manualRemovalNeeded: bool}` |
| `folders` | `{accountId}` | `FolderView[]` |
| `messages` | `{accountId, folderId, more?}` | `ListResult`. Returns cached rows **immediately**, and starts a fill that emits `mail-changed`. |
| `message` | `{accountId, messageId, showImages}` | `MessageView` |
| `refresh` | `{accountId}` | – (a manual poll via `PollGate`) |
| `set_view` | `{accountId, folderId}` | – (sets the active account for the cadence) |
| `save_attachment` | `{accountId, messageId, partId}` | `SaveResult`. Rust opens the save dialog (plugin-dialog, from Rust): prefilled file name, the last folder used (stored in `settings.json` in the app data dir, defaulting to Downloads), writes the file, then records the folder. Cancel → `saved: false`. |
| `status` | – | `SyncStatus[]` |

- **Events:**
  - `mail-changed {accountId, labels: string[]}`
  - `sync-status SyncStatus`
  - `account-changed AccountView`
  - `sign-in-progress {stage: browser|waiting|done|error, message?}`
- **Protocol:** `http://gmail-asset.localhost/<accountId>/<messageId>/<partId>`. Segments are checked against `[A-Za-z0-9_.@+-]`, and the bytes come from the cached or just-fetched raw.
- **Links:** the iframe uses `sandbox="allow-popups"`, and the host's `on_new_window` checks the scheme (http, https, mailto, tel) and opens the link with `tauri-plugin-opener` **from Rust**. Every navigation is denied.
- **Window focus and minimize state** are tracked in Rust and feed the `Cadence`.

### 3.8 As built in Wave 0 (this overrides §3.1–3.7 where they differ)
**The code in the `skeleton` commit is the contract.** Read the signatures in the crate sources. Notable choices:
- **`MailCache`** (in `mail-model\src\traits.rs`):
  - Every method is sync and returns `Result<_, CacheError>`.
  - `now_ms` is passed in explicitly (`upsert_meta`, `put_body`, `touch_opened`, `evict`).
  - `set_cursor(cursor, synced_at_ms)`; `rows(label, before, limit, include_stale)`; `put_body` returns `false` above the cap; `evict` returns `EvictReport`.
  - `upsert_meta(.., UpsertMode)`: `Always` for folder fills, `Admit` for poll hydration (S4's admit rule).
  - A window labelled `"ALL"` is All Mail and matches any label set.
- **`AuthError`** has the extra variants `Config | Network | Store | Protocol`. An offline token refresh is `Network`, never `NeedsReauth`.
- **`CmdError.detail`** is a machine-readable sub-code (`missing_scope`, `cancelled`, `timeout`, `needs_reauth`, …). The UI branches on it.
- **gauth login:**
  - `begin_login` does **not** open the browser. The UI shows the "tick *Read all your email*" modal first, then calls `reopen_browser`.
  - `PendingLogin::handle()` returns a cloneable `LoginHandle` (cancel or reopen while another task awaits).
  - `complete_login` doesn't save; the caller calls `Store::save`.
  - `Endpoints` on `Client` and the `*_at` functions exist for httpmock.
- **`build_tree`** is implemented in `mail-model`.
  - It always emits the fixed system folders and the 5 categories.
  - Group ids are `group:<path>`, the Categories header is `group:categories`, and All Mail is `ALL`.
  - Unread counts are filled in by the service.
- **mail-sync:**
  - `Kind::of(&ApiError)` drives `Backoff::next(kind, base) -> Plan { delay, ui, resync }`.
  - `NeedsReauth` stops polling at once.
  - `Cadence::jittered(base, unit)` takes the random unit as an argument, for tests.
  - `PollReport.resynced`.
- **mail-render:**
  - `Parsed` carries every part with its bytes (`parts: Vec<Part>`), so `part()` doesn't re-parse.
  - Public extras: `message_row`, `asset_url`, `parse_asset_path`, `inline_safe_mime`.
  - `fixtures` module: `hostile_html(canary)`.
- **mail-service:**
  - `save_attachment(.., &dyn FileSaver)`. The host implements `FileSaver` (dialog + `settings.json`), so the service stays Tauri-free.
  - Also `asset()`, `set_window_state()` and `shutdown()`.
- **Host:**
  - The main window is created in code (label `main`, so the navigation and new-window hooks can be attached).
  - Features: `default = ["custom-protocol"]`, plus `mock`.
  - The Tauri CSP is `null` for now. **WP-F decides** on a tight app CSP that still lets the srcdoc iframe load `gmail-asset.localhost`, `data:` and, with "Show images", `https:`/`http:` (a srcdoc iframe inherits the parent's CSP).
- **UI expectations (WP-G, as merged):**
  - The UI shows the "tick *Read all your email*" modal itself, then calls `add_account` or `reauth`. **Those service commands must open the browser themselves:** `begin_login` → `handle.reopen_browser()` → `complete_login`. `reopen_sign_in` is only for the Re-open button.
  - `messages(more: true)` may return either the extended list or only the new rows. The UI merges by id and sorts by `internalDate`.
- **Extra workspace deps:** `chrono`, `encoding_rs`. Use `reqwest::Url` (there's no separate `url` crate).

**Stub lint allowances:** stub files start with `#![allow(unused_variables, dead_code)]` under a "Wave 0 stubs" comment. Remove it when the file is implemented.

---

## 4. Wave 1: parallel work packages

**Every WP:**
- Owns only the listed paths and doesn't change signatures from §3. If a WP needs a contract change, it stops and reports it.
- Adds tests. `cargo test -p <crate>` and `cargo clippy -p <crate> --all-targets -- -D warnings` must pass.
- Follows the privacy rules in §1.
- **Makes no calls to Google**, except where its section says otherwise (those calls run with the user present, and are reads only).
- Reports back: what was done, any contract problems, and anything left unverified.

Start each agent with: "Read `CLAUDE.md`, `PLAN.md` §1, §3 and your WP section, and the spike FINDINGS it names. Work only in your paths, in worktree branch `wp-<letter>`. Don't change §3 signatures. Report contract problems instead of working around them. Write files with the Write/Edit tools, not shell heredocs (Defender flags hostile-looking command lines)."

### WP-A: `gauth` (`crates\gauth\**`)
- Port `spikes\s0-token` and `spikes\s1-auth\core`: the loopback listener (404 for noise), PKCE, state, the scope check, id_token decode, the store (tmp + rename, skip bad files, the `needs_reauth` flag), the token cache with `invalidate`, revoke and tokeninfo.
- One pending login app-wide (the cancel permit is stored).
- Tests: everything synthetic from S0 and S1, plus `invalidate` forcing a refresh (httpmock for the token endpoint).
- **No live calls.**

### WP-B: `gmail-api` (`crates\gmail-api\**`)
- Implement `GmailApi` with httpmock tests for every endpoint:
  - batch framing and parsing, including a failed sub-response and a failed batch falling back to GETs
  - classification of 401 (retry once with force), 403 `rateLimitExceeded`, 429, history 404, 403 `PERMISSION_DENIED`, and transport errors
  - `includeSpamTrash` for SPAM and TRASH
  - snippet passed through raw (decoded in render)
- Add the forbidden-endpoint grep test.
- Optional `#[ignore]` live test against dylan via gtoken, printing counts only.

### WP-C: `mail-cache` (`crates\mail-cache\**`)
- Port the S4 store, delta and signout code, with AEAD variant (d).
- Tests:
  - round trip; no plaintext in the file (marker search)
  - the delta admit rules (S4's 5 tests)
  - window semantics
  - stale handling
  - body size cap
  - eviction (LRU with the keep-rules)
  - deletion with open connections refused; rename and sweep; no WAL/SHM strays
- A benchmark test, `#[ignore]`: 1,000 rows.

### WP-D: `mail-sync` (`crates\mail-sync\**`)
- Implement §3.6 against **fakes** of `GmailApi` and `MailCache` (in-crate test doubles). Don't depend on the real cache or API crates.
- Tests:
  - the gate (a click during a poll gives exactly one trailing poll)
  - backoff tables
  - the cursor taken before the list on cold fill
  - the cursor not advanced on a failed apply
  - 404 → resync
  - batch split (20 then 30)
  - `NeedsReauth` stops polling
  - cadence selection

### WP-E: `mail-render` (`crates\mail-render\**`)
- Port the S3 pipeline, its 25 unit tests, 8 integration tests and fixtures, **unchanged in behaviour**.
- Add `snippet_text` and `parse_from` (port from S2).
- **Theme:** the page background is always white with dark text, including plain text. Remove the HtmlInvert experiment.
- Tests: the hostile fixture produces no script/on*/form/iframe/svg/remote URL in block mode; `cid` mapping; C1 repair; asset headers.

### WP-F: Tauri host (`app\src-tauri\**`)
- Commands from §3.7 against a `MockService` with the same method names as `mail-service`. Mock data is synthetic, including a hostile message, a message with a cid image, and one with attachments.
- The `gmail-asset` protocol.
- The iframe `on_new_window` link policy.
- The save dialog: the "last folder" persisted in `settings.json`, defaulting to Downloads, with the file name sanitized for Windows (reserved names and `\/:*?"<>|`).
- Focus and minimize tracking.
- Raising the window after sign-in.
- The capability: `core:default` + `dialog:allow-save` (dialogs are opened from Rust, so check whether even that is needed).
- Manual check (user present, mock only): run the app, save an attachment twice (the second time opens in the same folder), click links (they open in the browser), and see the hostile message stay inert.

### WP-G: UI (`app\ui\**`)
- Vanilla JS, **dark theme throughout**, the reading pane's email frame on white. Works with `?mock=1` (a `mock.js` shim of §3.7).
- **Layout:**
  - Left: an account switcher (email, unread badge, needs-reauth badge, "Add account"), then the folder tree (§1 order, Spam unread visible, categories collapsible, user labels nested).
  - Middle: the message list (sender, subject, snippet, relative time, unread dot, star, paperclip). "Load older" when online and `more`; "End of cached mail" when offline.
  - Right: the header block (with the sender-check badge), the attachments row with **Save…** buttons, a "Show images (N blocked)" bar, and the email iframe (`sandbox="allow-popups"`, `srcdoc`).
- **Top bar:** the Refresh button (spins while a poll runs), the status (offline / throttled / "as of …").
- **Sign-in UX:**
  - Before the browser opens, a modal with the "tick *Read all your email*" instruction and a Continue button.
  - While pending: "Waiting for Google…" with Re-open and Cancel.
  - Missing-scope error: an explanation and a Try again button.
  - `needsReauth`: a per-account banner with a "Sign in again" button.
- Sign out sits in the account context menu, behind a confirm dialog that explains it also removes the app's access at Google.
- **No keyboard shortcuts** (user decision). Escape only closes an open menu.
- No frameworks. All text inserted via `textContent` (never innerHTML) except the srcdoc.

---

## 5. Wave 2: integration (sequential)

### WP-H: `mail-service` facade (`crates\mail-service\**`), one agent
- `Service::open(data_dir, Arc<dyn EventSink>)` loads accounts (skipping bad files), opens caches, sweeps deletions, and starts one poll task per signed-in account, driven by `Cadence`, `PollGate` and focus state.
- Every method from §3.7, with the same names as `MockService`.
- `messages` returns the cache and spawns `fill_folder`. `message`:
  1. Read the cached body, or fetch `raw`.
  2. Cache it (under the cap) and touch it.
  3. Render with the policy.
  4. Revalidate stale rows first.
- `folder` views come from `labels` + `folder_counts` + `build_tree`.
- `sign_out` stops the poll task, closes the cache, revokes, deletes the token file and the cache directory, and emits `account-changed`.
- A `NeedsReauth` anywhere persists the flag, stops polling, and emits.
- Asset and save byte sources come from `mail-render::part`.
- Tests use fakes: the multi-account startup, sign-out ordering, the needs-reauth flow, and offline behaviour.

### WP-I: wire the host and check end to end, one agent with the user present
- Swap `MockService` for `Service`.
- The user copies `oauth-client.json` from `%LOCALAPPDATA%\com.ziggy.starmail-spikes\` to `%LOCALAPPDATA%\com.ziggy.starmail\`.
- Sign in all three accounts, watching for the unticked Gmail box.
- Check, with the user:
  - inbox shows instantly on the second launch
  - a sign-in code email (requested from any service) appears without clicking Refresh
  - Spam shows its unread count
  - Refresh works
  - an HTML newsletter renders on white
  - "Show images"
  - a cid image
  - Save attachment, twice (same folder)
  - links open in the browser
  - offline (Wi-Fi off) shows cached mail with the banner
  - sign out of one account removes it and its cache
  - after Testing's 7 days (or a manual revoke), "Sign in again" works

---

## 6. Wave 3: finishing (parallel, after WP-I)
- **WP-J, release binary (no installer; user decision):** a single portable `starmail.exe` from `cargo build --release -p starmail`, to drop into `C:\Apps`. The UI is embedded, it runs as a GUI app (no console), and its data lives in `%LOCALAPPDATA%\com.ziggy.starmail`. It relies on the WebView2 runtime that ships with Windows 11. Done by the coordinator after K1–K3 merge.
- **WP-K1, render hardening** (`crates\mail-render\**`):
  - proptest `parse`, `render` and the sanitizer on arbitrary and mutated input (no panics, and the output invariants hold)
  - a 2 MB cap on rendered page size, falling back to plain text
- **WP-K2, stale-message fix** (`mail-model` trait, `mail-cache`, `mail-sync` test fakes, `mail-service`):
  - add `MailCache::meta(id) -> Option<(MessageMeta, stale: bool)>`
  - `Service::message` revalidates a stale row with one metadata get before opening it: a deleted message shows a friendly "no longer in Gmail" state, not an error.
  - The internalDate fallback uses `meta` instead of scanning.
- **WP-K3, logging and audit** (`app\src-tauri\**`, `crates\mail-service` logging calls only if needed):
  - a redacted rolling log file at `%LOCALAPPDATA%\com.ziggy.starmail\logs`, with the dates of sign-ins and re-auth triggers
  - a final read-only audit: grep the whole workspace for non-GET Gmail calls and forbidden endpoints, reported as a test

---

## 7. Parallelism summary
| Wave | Agents | Packages | Blocked by |
|---|---|---|---|
| 0 | 1 | skeleton + contracts | – |
| 1 | **7 in parallel** | A, B, C, D, E, F, G | Wave 0 |
| 2 | 1, then 1 (+user) | H, then I | all of Wave 1 |
| 3 | 3 in parallel, then J | K1, K2, K3, then J | I |

## 8. Definition of done
- Three accounts are signed in.
- After the first launch, the app starts without a browser and shows each account's cached inbox in under 100 ms, offline too.
- A new sign-in code email appears on its own within about 20 s while the window is focused, in Inbox or Spam.
- Every sampled email renders on white inside a dark app. Remote images stay off until asked. Links open in the browser, and nothing in an email can run script or make a request.
- Attachments save only when the user asks, to a chosen folder that is remembered.
- Signing out revokes access at Google and deletes the account's token and cache.
- No code path can modify Gmail. No token, address, subject or body appears in any log.
