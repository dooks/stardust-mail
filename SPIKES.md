# starmail: stack and investigative spikes

Step 1 of the workflow (see `CLAUDE.md`). This file fixes the stack and gives one self-contained brief per spike. A Sonnet agent should need only `CLAUDE.md`, this file, and its own spike section.

---

## 1. Stack decision

| Layer | Choice | Why |
|---|---|---|
| Shell | **Rust + Tauri 2.12**, vanilla HTML/CSS/JS, cargo only | Same as icloud-notes, so its host, UI, DPAPI and renderer patterns carry over. |
| Mail access | **Gmail REST API v1** (`gmail.googleapis.com`) via plain `reqwest` 0.12 (rustls) + serde types for the ~6 endpoints we use | The API has a **server-enforced read-only scope** (`gmail.readonly`). IMAP has none: XOAUTH2 IMAP needs the full `https://mail.google.com/` scope, and app passwords also give full access, so read-only would only be a client-side promise. The generated `google-gmail1` crate is heavy and adds nothing for 6 endpoints. |
| Auth | **OAuth 2.0 for installed apps**: system browser + loopback redirect (`http://127.0.0.1:<ephemeral>`) + PKCE (S256) + `state`. One OAuth client (the user's own Google Cloud project, "Desktop app" type) serves every account. | Google blocks OAuth inside embedded webviews (`disallowed_useragent`), so unlike icloud-notes the login cannot run in a Tauri window. The OOB / copy-paste flow is retired. |
| Scopes | `https://www.googleapis.com/auth/gmail.readonly` + `openid email` | Least privilege that still gives bodies. `gmail.metadata` has no bodies, so it's not enough. |
| Secrets | Refresh token per account, **DPAPI** (CurrentUser + app entropy) in `%LOCALAPPDATA%\com.ziggy.starmail\accounts\<sub>.dpapi`. Access tokens live in memory only. | Port `icloud-session`'s `protect`/`unprotect`. |
| MIME | `format=raw` + the **`mail-parser`** crate (to be confirmed by S3) | Handles charsets, RFC 2047 encoded words, nested multiparts, and `cid:` parts in one pass. The Gmail `payload` tree is the fallback. |
| HTML mail | Sanitize (**`ammonia`**), then show it in a sandboxed `<iframe srcdoc>` (no `allow-scripts`) with a strict CSP. Remote images are **blocked by default**. `cid:` images come via a custom protocol `http://gmail-asset.localhost/…`. Links open in the OS browser after a scheme check. | Same isolation model as icloud-notes spike 8. Email HTML is arbitrary, though, so here we must sanitize (that renderer generated its own HTML instead). |
| Local data | **Fetch-through SQLite cache** (rusqlite) for list rows and opened messages, so they stay readable offline. Encryption at rest and the storage shape are decided by S4. | User requirement: offline reading of anything already fetched. Freshness still comes first: show the cache, then refresh. |
| Identity | App id `com.ziggy.starmail`. Spikes use `com.ziggy.starmail-spikes` so they never touch the real app's data. | |

**Rejected:**
- **IMAP + app passwords.** Needs no Cloud project, but gives full mailbox access, is not read-only on the server side, and Google keeps narrowing it. Keep it as the fallback only if S1 shows the API route is unworkable. (All accounts are personal @gmail.com, so no Workspace admin policy applies.)
- **Embedding gmail.com in a webview.** Fragile, and not read-only.
- **Pub/Sub push (`users.watch`).** Needs a Pub/Sub topic, a pull subscription with an extra scope or a service-account key, and a renewal every 7 days, and `watch` is a write-ish call. Automatic updates come from polling `history.list`, which is cheap (S4).

### Read-only, in three layers
1. **Server:** the token only carries `gmail.readonly`. Google rejects every mutating call with 403.
2. **Code:** the API client crate exposes only GET calls on `labels`, `messages`, `messages.attachments`, `history`, `threads`, and `profile`. A test greps the source for forbidden path segments.
3. **UI:** there are no controls that could imply a change.

---

## 2. Requirement → spike map

| Requirement | Spikes |
|---|---|
| Display emails with pertinent metadata | S2 (list metadata), S3 (message view) |
| Display folders | S2 (labels) |
| Switch between accounts | S0, S1 |
| Read-only | S1 (granted scope, verified), all (endpoint discipline) |
| Main use case: the latest auth code, fast | S4 (freshness, cold-start latency) |

Order: **S0 runs first and alone, with the user present** (about 30 min). Then **S1–S4 run in parallel.** S1 also needs the user at the keyboard for consent screens. S4 needs the user once, to trigger an incoming email.

**Rules for every spike:** the privacy and read-only rules in `CLAUDE.md` apply. Work only in `spikes\sN-*\`. Put real data only in `spikes\sN-*\out\` (git-ignored). Finish with `FINDINGS.md`: a TL;DR first, then answers per question, measured numbers, error tables, a recommendation for the build plan, and "still open". **Don't call any endpoint outside the list in your brief.**

---

## S0: Google Cloud setup + shared token helper (sequential, user present)

**Goal:** a working OAuth client, plus a tiny library that gives S2–S4 an access token for any signed-in account without a UI.

User steps (the agent guides; the agent never sees the client secret's value):
1. Create a Google Cloud project (free) at console.cloud.google.com. Enable **Gmail API**.
2. Set up the OAuth consent screen: User type **External**, app name "starmail (personal)". Add the scopes `gmail.readonly`, `openid` and `email`. Add every Gmail address as a **test user**. (S1 decides between Testing and Production later. Start in Testing.)
3. Create credentials → OAuth client ID → **Desktop app**. Download the JSON to `%LOCALAPPDATA%\com.ziggy.starmail-spikes\oauth-client.json`. That's outside the repo, and never pasted in chat.

Agent builds `spikes\s0-token\` as a cargo workspace with:
- lib `gtoken`:
  - `fn load_client() -> Client`
  - `async fn login() -> Account`: loopback listener on `127.0.0.1:0`, PKCE S256, `state`, `access_type=offline`, `prompt=consent select_account`, open the system browser with `opener` / `ShellExecute`, exchange the code, then `users.getProfile` for `emailAddress` and `historyId`. Save `{email, sub, refresh_token, granted_scopes}` with DPAPI.
  - `fn accounts() -> Vec<String>`
  - `async fn access_token(email) -> String`: refresh with an in-memory cache that respects `expires_in`.
  - Choose between the `oauth2` crate (v5) and ~120 handwritten lines, and say why in FINDINGS.
- bin `gtoken`: subcommands `login`, `list`, `check <email>`. `check` refreshes, calls `getProfile`, and prints the email, `messagesTotal`, and the granted scopes from `https://oauth2.googleapis.com/tokeninfo`. **It never prints a token.**

Done when: the user has run `gtoken login` for **at least two accounts**, and `gtoken check` works for each in a fresh process. S1–S4 depend on `gtoken` by path.

---

## S1: authentication, accounts and the read-only guarantee (Tauri, user present)

**Goal:** the production-ready sign-in design for several accounts, inside Tauri.

Build `spikes\s1-auth\` as a minimal Tauri 2.12 app (identifier `com.ziggy.starmail-spikes-s1`, own token store, **not** S0's). It has a "Add account" button, a list of accounts, and per account a "Check" button (refresh + `getProfile`) and a "Remove" button (local delete).

Questions:
1. **Flow inside Tauri:** spawn the loopback listener from a command, open the system browser, receive the redirect, then show a "you can close this tab" page and bring the app window back to the front. How does it handle a cancelled consent, a closed tab (timeout), and a second "Add account" while one is still pending?
2. **Embedded webview:** confirm with one attempt that Google refuses OAuth in a WebView2 window. Record the exact error. Don't try to work around it.
3. **Consent screen:** what does the user see (the "unverified app" interstitial and its steps)? With **granular consent**, what happens if the user unticks Gmail? Detect it from the `scope` field of the token response, and design the error message.
4. **Publishing status (key question):** in **Testing**, refresh tokens for non-basic scopes expire after **7 days**. Confirm from Google's current docs whether a personal, unverified External app may move to **Production** with the restricted scope `gmail.readonly` (≤100 users, with the unverified warning, no CASA assessment). Also confirm that tokens are then long-lived. Flip the test project to Production, sign in once, and record what changes on the consent screen. A 7-day wait isn't needed: cite the docs and record the issue dates of the tokens.
5. **Multiple accounts:** same client, `login_hint`, `prompt=select_account`. Which stable key should we use (`sub` from the id_token, or the email from `getProfile`)? Signing into the same account twice should replace it, not duplicate it. Is there a refresh-token limit per client per account (Google documents about 100)?
6. **Token errors:** record the HTTP status and body (`error` field only) for: a refresh with a revoked grant (`invalid_grant`), an API call with an expired or garbage access token (401), and a missing scope (403 `insufficientPermissions`, triggered by a *read* call that a scope-reduced token can't make; **never** by a write call).
   Do the revocation tests **last**, on one account. **Revoking also cancels S0's grant for that account, so tell the user to re-run `gtoken login` for it afterwards.**
7. **Sign-out (fixed by the user: revoke at Google, then delete locally).** Call `POST https://oauth2.googleapis.com/revoke` with the refresh token (form body, never in the URL).
   - Record the response for a valid token, an already-revoked token, and when offline.
   - Does it revoke the whole grant, so every token for that account and client?
   - Does the app disappear from myaccount.google.com → Security → Third-party access?
   Recommend the order and the failure rule: if the revoke fails because we're offline, still delete locally, and tell the user to remove access on that page. Also record what an external revocation (done by the user on that page) looks like to the app, so it can be shown as "access removed, sign in again".
8. **Read-only proof:** `tokeninfo` lists only the requested scopes. Don't attempt any write call.

Deliverable: the recommended auth state machine (signed out → pending → signed in → needs re-auth), an error table like icloud-notes spike 4's, and the storage layout.

---

## S2: labels (folders) and message lists (headless, uses `gtoken`)

**Goal:** know exactly how to show the folder tree and a fast message list for one account.

Allowed endpoints: `labels.list`, `labels.get`, `messages.list`, `messages.get?format=metadata`, `threads.list`, `threads.get?format=metadata`, `getProfile`, and the batch endpoint `POST https://gmail.googleapis.com/batch/gmail/v1`. **The batch body may contain only GET sub-requests.**

Build `spikes\s2-list\` (a CLI) with: `labels <email>`, `list <email> <labelId> [--n 50]`, `bench <email>`.

Questions:
1. **Labels:** the full set of system label ids (INBOX, SENT, DRAFT, SPAM, TRASH, STARRED, IMPORTANT, UNREAD, CHAT, CATEGORY_*). `type`, `labelListVisibility` and `messageListVisibility`, colors, and nesting by `/` in names (is there a parent without its own label?). Do the counts (`messagesTotal`, `messagesUnread`, `threadsTotal`) come only from `labels.get`, so N calls? What does it cost per account?
   Propose the folder tree to show: Inbox, Starred, Sent, Drafts, All Mail (no label filter), Spam, Trash, the categories, then user labels nested. Report counts and shapes only, never label names.
2. **Listing:** `messages.list` with `labelIds`, `maxResults`, `pageToken`, `includeSpamTrash` (needed for Spam and Trash), and `q` (`newer_than:1d`, `category:primary`). Is the order newest first, and is that guaranteed? Is `resultSizeEstimate` reliable?
3. **Hydrating a page of 50:** compare (a) 50 concurrent `messages.get?format=metadata&metadataHeaders=From,To,Subject,Date` with a concurrency of 5/10/20, (b) one batch request, (c) `threads.list` + `threads.get`. Report p50/p95 wall time, quota units used, and any 429s. Recommend one.
4. **Messages or threads:** in each account's INBOX, what share of threads has more than one message? Recommend a message list or a conversation list, given the auth-code use case.
5. **Metadata for the list row:** sender display name and address (RFC 2047 decoding), subject, `internalDate` vs the `Date` header (which one to sort and show?), `snippet` (HTML entities?), the UNREAD/STARRED/IMPORTANT flags, an attachment indicator (is there one without the full payload?), and `sizeEstimate`.
6. **Quota:** state the per-user units per second, and the cost of a cold "open account → show inbox".

---

## S3: message view and rendering (headless + HTML output for the user to eyeball)

**Goal:** turn any message into safe, faithful HTML plus a metadata header.

Allowed endpoints: `messages.list`, `messages.get` (`format=raw` and `format=full`), `messages.attachments.get`.

Build `spikes\s3-render\` (a CLI): `fetch <email> [--n 40]` saves raw messages to `out\raw\` (a mix of newsletters, receipts, auth-code mails, plain-text mails, mails with attachments, and non-UTF-8 ones if there are any). `stats` prints content-free counts. `render <id>` writes `out\<id>.html`, which the user opens to compare with Gmail.

Questions:
1. **Parsing:** `format=raw` + `mail-parser` vs the `format=full` payload tree. Check correctness on the sample: charsets, encoded words, `multipart/alternative|related|mixed` nesting, missing parts. How large does `raw` get, and when does it need `attachments.get`?
2. **Choosing the body:** HTML when present, else text/plain turned into HTML (linkified, `pre-wrap`). Measure how often each case occurs.
3. **Sanitizing:** an `ammonia` configuration that keeps layout (tables, inline `style`, `bgcolor`) but strips scripts, event handlers, forms, `<base>`, `meta refresh`, and `javascript:` / `data:` URLs (except `data:image` if needed).
   Wrap the result in a page with the CSP `default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:` and show it in `<iframe sandbox srcdoc>`.
   Confirm in WebView2 (a tiny Tauri harness, or open the html in Edge) that scripts can't run, that external CSS and fonts are blocked, and that link clicks can be intercepted by the parent.
4. **Remote images:** blocked by default (privacy: tracking pixels). Design the "Show images" toggle: rewrite remote `src` to a host proxy, or relax the CSP per message? Recommend one, with its privacy cost.
5. **Inline `cid:` images and attachments:** map `cid:` to `http://gmail-asset.localhost/<account>/<msgId>/<partId>`. List attachments with name, MIME type and size. Should "Save attachment" exist at all? It writes to the local disk, not to Gmail. Note it as a user decision.
6. **Pertinent metadata:** From (name + address), To, Cc, Reply-To, Date (local plus relative), labels, and a **sender authenticity** line parsed from `Authentication-Results` (SPF, DKIM and DMARC pass/fail, like Gmail's "mailed-by" / "signed-by").
   Is parsing it reliable on the sample?
7. **Dark mode:** emails assume a white background. Recommend whether to render them on white always or try to adapt.

Port the page wrapper and the link-interception idea from `icloud-notes\spikes\spike8-render` and `crates\notes-render`. Copy the code; don't link it.

---

## S4: freshness, sync and cache (headless, user present once)

**Goal:** decide how the app stays current, and design the offline cache.

**Fixed by the user:** anything already fetched (list rows *and* opened message bodies) must stay readable offline. The cache is a **fetch-through** cache. It doesn't prefetch the whole mailbox: a folder page or message is cached when it is first shown.

Allowed endpoints: `getProfile`, `history.list`, `messages.list`, `messages.get` (`format=metadata` and `format=raw`).

Build `spikes\s4-sync\` (a CLI) with:
- `cold <email>`: time to show the first 20 INBOX rows, from process start with a refresh token on disk, both from an empty cache and from a warm one.
- `watch <email> --every 5s`: polls `history.list` and prints the event types, ids, and the latency between the message's `internalDate` and when we saw it.
- `stale <email> <historyId>`
- `cache-demo <email>`: fill the cache from INBOX plus a few opened bodies, apply history deltas, then show the same rows with the network off (pass `--offline` to skip all calls).

Questions:
1. **Cold start:** the wall time for token refresh, list and hydrate, per account and for all accounts in parallel. Compare it with showing cached rows first and refreshing behind them.
2. **Auto-refresh (user requirement: new mail appears without clicking, and a manual Refresh button also exists).** Design it as `history.list` polling.
   - What does one poll (with `startHistoryId` from `getProfile`, and `historyTypes`) cost with no changes, and with one new message?
   - Propose a cadence for when the window is focused vs in the background, for every signed-in account. Check it against the quota.
   - Manual Refresh runs the same poll immediately. How does it combine with a poll that is already running (one in flight per account)?
   - **The user triggers a real incoming mail** (for example a sign-in code from any service, or a mail from another account). Report the arrival → seen latency at 5 s, 10 s and 15 s intervals.
   - What should a poll do when it hits a 401, a 429, or no network (backoff, and the UI state)?
3. **History expiry:** what does a too-old or invalid `startHistoryId` return (404)? Try one obviously ancient id. Docs say history is kept for about a week. What is the resync rule for a cache that has been offline longer than that?
4. **Keeping the cache true:** apply `messageAdded`, `messageDeleted`, `labelAdded` and `labelRemoved` to the cached rows. Then make the change in Gmail yourself on the phone or web: the user archives one message, deletes one, and stars one. Check that the cache matches afterwards. (The app itself never writes.) How does a cached page know it's complete versus a partial window into a folder? What does the UI show offline (a "cached, as of …" banner)?
5. **Encryption at rest:** mail on disk is sensitive. Compare these, with numbers (build impact on Windows, query speed, complexity):
   - (a) **SQLCipher** via `rusqlite` `bundled-sqlcipher-vendored-openssl`, with a random key stored DPAPI-protected
   - (b) plain SQLite with headers and bodies stored as per-row DPAPI blobs, plus only ids, label ids and dates in clear
   - (c) plain SQLite, relying on the Windows user profile.
   Recommend one.
6. **Storage shape and size:** one DB per account or one shared DB? Store raw RFC 822 bodies (needed so `cid:` images and attachments render offline) or sanitized HTML? Measure the bytes per message on the sample. Propose an eviction rule (for example, bodies older than N days or a total cap) and say whether attachments larger than X are fetched only on demand and never cached.
7. **Sign-out:** the account's cache must be deleted together with its token. Confirm that it can be removed atomically, with no stray WAL/SHM files.
8. **Push, as desk research only. Never call `users.watch`.** Confirm from Google's current docs what real push would take: `users.watch` + a Pub/Sub topic + a pull subscription, the extra OAuth scope or service-account key, renewal every 7 days, and cost. Compare its latency gain with polling at the chosen cadence. The expected verdict is "polling", but record the evidence.

---

## 3. Deliberately not spiked

- **The UI shell** (three panes, account switcher, keyboard navigation): covered by icloud-notes `app\ui` and WP-H/WP-I. It gets designed in `PLAN.md` from S1–S4's outputs.
- **Packaging** (NSIS, per-user): same as icloud-notes WP-L.
- **Search:** Gmail's `q` parameter makes it a thin wrapper. S2 checks that `q` works, which is enough.

## 4. Questions for the user before step 2

1. ~~Any Google Workspace accounts?~~ **Answered: all are personal @gmail.com.**
2. Are you OK creating a free **Google Cloud project** (S0, about 15 min, guided)? It's required for the Gmail API route.
3. ~~Extras?~~ **Answered: core only.** No all-accounts view, no code highlight, and no toasts. These can be added after v1.
4. ~~Offline?~~ **Answered: yes, for anything already fetched** (see S4).
5. ~~Sign-out?~~ **Answered: also revoke the app's access at Google** (see S1, question 7).
6. **Added:** new mail shows up automatically, and a manual Refresh button is kept. This is done by polling `history.list`, not Pub/Sub push (see S4, questions 2 and 8).
