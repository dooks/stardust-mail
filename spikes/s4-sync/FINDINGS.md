# S4: freshness, sync and cache

> **Status: final.** The new-mail timing was measured once with the user. The other user steps were skipped (the user's call).

Crate: `spikes\s4-sync\` (CLI `s4-sync`).
- Modules: `api`, `store`, `delta`, `gate`, `signout`, `bench` and `dpapi`. `store`, `delta`, `gate` and `signout` can be lifted into the real app.
- 17 tests pass, and clippy is clean.
- Accounts used: dylan and ziggy.
- Endpoints (GET only): `getProfile`, `history.list`, `messages.list`, `messages.get` (metadata and raw). `users.watch` was never called.
- Quota costs per call, as the agent reported them: `getProfile` 1, `history.list` 2, `messages.list` 5, `messages.get` 20. The limit is 6,000 units per minute per user. (S2 independently confirmed these figures from Google's current quota page. An earlier reviewer doubt, which assumed 5 units, was wrong.)

## TL;DR
- **Polling `history.list` is the design.** A no-change poll costs 2 units and returns 29 B, with p50 94 ms and p95 128 ms. Google's docs recommend polling for installed apps (Q8).
- **Cold start:**
  - Cache-first shows 20 INBOX rows **~3–8 ms** after process start.
  - With an empty cache it takes ~0.9 s (dylan) to ~1.6 s (ziggy), and 1.68 s for both in parallel.
- **An expired or invalid history id returns 404**, which means resync. A resync was tested end to end and took ~1.3 s.
- **Encryption at rest:** **plain SQLite plus per-blob ChaCha20-Poly1305, with a random per-account key that is DPAPI-wrapped.** It runs at plain-SQLite speed, and no plaintext was found in the file.
  - SQLCipher (vendored OpenSSL) **does not build here**: Git's stripped Perl can't run OpenSSL's Configure.
  - Per-row DPAPI is 10–27× slower on scans.
- **Storage:** one DB per account, in its own directory. It stores raw RFC 822. Sign-out is rename, then delete of the directory (tested, with no stray WAL/SHM files).
- **Gmail rate limits come back as HTTP 403 `rateLimitExceeded`** ("Units per minute per user"), with **no Retry-After**. They cleared after 1–2 minutes. The parallel spikes triggered this on the shared project.

## Q1. Cold start (from process start; includes the ~200–230 ms token refresh)
| Case | dylan | ziggy |
|---|---|---|
| Empty cache: token | 231 ms | 209 ms |
| getProfile (cursor) | 189 ms | 368 ms |
| list 20 | 107 ms | 269 ms |
| hydrate 20 (10 parallel) | 442 ms | 716 ms |
| **Rows shown, empty cache** | **979 ms** | **1574 ms** |
| **Rows shown, warm cache** | **3.6 ms** | **3.5 ms** |
| Warm: token + poll finished | 421 ms | 547 ms |

- Both accounts in parallel, empty cache: 1680 ms. Warm cache: rows at 7.5 ms and 28 ms, and both refreshed within 607 ms.
- **Cache-first:** show rows from SQLite right away, then refresh behind them.
- The cursor (`getProfile.historyId`) is taken **before** `messages.list`. Replaying events is idempotent.
- A cold fill costs ~406 units (7% of the per-minute budget).

## Q2. Auto-refresh by polling
**Poll cost** (dylan, 5 s interval, 12 polls, no changes): min 87 ms, p50 94 ms, p95 128 ms, max 158 ms, 29 B, 2 units. The first poll of a process (new TLS connection) takes 210–350 ms.
**One new message (live, 2026-10-25, dylan, 5 s interval, 47 polls over 4 minutes):**
- The poll with the event returned 499 B: one `messageAdded`.
- All other polls returned 29 B. Poll latency: p50 108 ms, p95 144 ms.
- **The message became visible 15.9 s after its `internalDate`.** At a 5 s interval, polling explains at most 5 s of that, so roughly **11 s is Gmail's own delay** before the event shows in history. Faster polling won't make new mail appear much sooner, so 5 s when focused is plenty, and even 10 s would add little.
- **The test mail was labelled `SPAM`** (plus `CATEGORY_UPDATES` and `UNREAD`), and the history event still arrived. **For the real app:** sign-in codes can land in Spam, so make Spam easy to reach. A message that isn't in INBOX won't appear in the inbox list.
- The 10 s and 15 s intervals were skipped (the user's call).

**Cadence**
| Window | Active account | Others |
|---|---|---|
| Focused | 5 s | 15 s |
| Visible, unfocused | 30 s | 30 s |
| Minimized | 60 s | 60 s |

- Also poll on focus or restore, staggered per account, with ±20% jitter.
- 3 accounts all at 5 s use 72 units/min, which is 0.4% of the quota.
- Traffic is ~17 MB/day per account at 5 s, or ~1.4 MB/day at 60 s.

**Manual Refresh (`gate.rs` `PollGate`, tested):** one poll in flight per account.
- A timer tick during a poll is dropped.
- A manual click during a poll queues **exactly one trailing poll**. The in-flight poll may have started before new mail arrived.
- Further clicks collapse into that queued poll.
- A click while idle starts a poll right away and resets the timer.

**Failures (`gate.rs` `Backoff`, tested)**
| Outcome | Behaviour | UI |
|---|---|---|
| 401 (`authError`) | One forced token refresh and a retry. A second 401 stops polling. | "Sign in again" |
| 429, or 403 `rateLimitExceeded` / `userRateLimitExceeded` | Honour Retry-After if present (not seen). Otherwise 30 s, doubling, capped at 600 s. | Throttled banner from the 3rd consecutive hit |
| Offline or 5xx | 5 s, 10 s, 20 s and so on, capped at 300 s. Reset on success. | "Cached, as of …" from the 2nd consecutive failure |
| 404 on history | Resync (Q3) | normal |

Offline detection: a transport error after ~2 s.

**gtoken gap:** there is no way to force-invalidate a cached access token, so a mid-lifetime 401 can't be cured. The real app needs `invalidate(email)` or `force_refresh`.

## Q3. History expiry (live, dylan)
| `startHistoryId` | Result |
|---|---|
| `1`, `0`, `99999999999999` | **404 `notFound`** (~190 ms) |
| `abc` | 400 `invalid` |
| u64 max | 400 `invalidArgument` |

From the docs (https://developers.google.com/workspace/gmail/api/guides/sync): out of range means 404 and a full sync. History is kept "typically" at least a week.

**Resync rule:**
- **Always try `history.list` first**, even after more than a week offline. Don't decide expiry by age.
- On 404:
  - Keep rows and bodies, because message content is immutable.
  - Drop the list windows, and mark rows `stale=1`.
  - Take a new cursor from `getProfile`.
  - Re-list and re-hydrate INBOX page 1.
- Stale rows are hidden from lists and revalidated when opened. They're shown, flagged, only when offline.
- Live result: 20 rows marked stale, 20 refilled, 0 left stale, about 1.3 s.
- Other labels' windows are re-listed lazily.

## Q4. Keeping the cache true
**Delta rules (`delta.rs`, tested):**
- `messageAdded` or `labelAdded` on an uncached id: fetch its metadata only if one of its labels has a cached window. Within a partial window, only rows newer than the window's edge are admitted.
- Label added or removed on a cached id: update its label set.
- `messageDeleted`: remove the row, its labels and its body.
- Replay is idempotent.

What Gmail sends for each action:
| Action in Gmail | Events |
|---|---|
| Archive | `labelRemoved INBOX` |
| Star | `labelAdded STARRED` |
| UI "Delete" (move to Trash) | `labelAdded TRASH` + `labelRemoved INBOX` |
| Permanent delete | `messageDeleted` |

**Windows:** each cached list has `windows(label, oldest_date, complete, as_of)`.
- `complete=1` means the whole label is cached.
- Otherwise every message with `internal_date >= oldest_date` is cached.
- "Load older" is offered only online. Offline, a partial window ends with "end of cached mail".
- Offline banner: "Offline: showing cached mail, as of <synced_at>". `cache-demo --offline` read 20 rows in 1.6 ms with no network calls.

**Live checks:**
- No changes: `verify-cache` on both accounts gave 20/20 matches with 0 mismatches.
- Real archive, star and delete: PENDING (user step 2).

## Q5. Encryption at rest (1000 rows + 200 × 30 KB bodies, 3 runs, release build, noisy machine)
| Variant | Insert all, one tx | Single insert | List 50 | Scan 1000 | Get body | Plaintext in file |
|---|---|---|---|---|---|---|
| (c) plain | 64–125 ms | 0.10–0.21 ms | 2.4–3.5 ms | 14–20 ms | 0.03–0.05 ms | **yes** |
| (b) per-row DPAPI | 850–1145 ms | 0.6–1.6 ms | 13–21 ms | 237–668 ms | 2.5–5.3 ms | no |
| **(d) per-blob ChaCha20-Poly1305, DPAPI-wrapped key** | 86–195 ms | 0.15–0.47 ms | 2.7–9.2 ms | 17–49 ms | 0.06–0.16 ms | no |
| (a) SQLCipher | **didn't build** | | | | | |

**SQLCipher** (`bundled-sqlcipher-vendored-openssl`) failed in `openssl-src` `perl Configure`. Git for Windows' MSYS Perl lacks `Locale/Maketext/Simple.pm`, and there's no other Perl on the machine. It would need Strawberry Perl, a multi-minute OpenSSL build and ~230 MB of build output.

**Recommendation: (d).**
- It's pure Rust (`chacha20poly1305`) and calls DPAPI once to unwrap the key.
- Each blob gets a random nonce, with the row id as AAD.
- Ids, label ids, dates and sizes stay in clear, which is acceptable leakage.
- Limitation: there's no SQL search over encrypted text, so search would need a decrypted in-memory index. Revisit SQLCipher only if offline full-text search is needed.
- There is one key per account (`key.dpapi` in the account directory). Deleting the directory also erases the key, so the data is unreadable even if file blocks survive.

## Q6. Storage shape and size
- **Per-account DB** (`cache\<account-id>\cache.db` + `key.dpapi`). Sign-out is one directory removal, and each account has its own key.
- **Store raw RFC 822**, so `cid:` images and attachments work offline and the sanitizer can change without invalidating the cache.
- `sizeEstimate` equalled the decoded raw size in 24 of 24 samples, so the large-message policy can be decided from list data.

| Account | Bodies | Total | Avg | Range |
|---|---|---|---|---|
| dylan | 12 | 711 KB | 59 KB | 4.5–157 KB |
| ziggy | 12 | 470 KB | 39 KB | 11–108 KB |

- A metadata row is ~1 KB on disk.
- 500 opened bodies take ~25 MB per account.
- Compressing before encryption is likely a 2–4× saving (not measured).

**Eviction (proposal):**
- Bodies: cap at 200 MB per account, LRU by last opened.
- Always keep bodies opened in the last 7 days, and the newest 20 INBOX bodies.
- Keep the newest ~2000 rows per cached label. When older rows are dropped, the window edge moves up.
- Run eviction at startup and after fills.

**Large messages:** if `sizeEstimate` is over 5 MB, fetch on open and don't persist. Cache the row only, marked "online only". Never prefetch attachments.

## Q7. Sign-out (tested: `signout-test` passes, plus 3 unit tests)
| Step | Result |
|---|---|
| WAL mode with 2 connections open | `cache.db`, `-shm` and `-wal` present |
| Remove while open | refused (`PermissionDenied`), directory intact |
| After closing with checkpoint(TRUNCATE) | only `cache.db` left |
| Crash after rename, before delete | only `<acct>.deleting-0` left; the startup sweep removes it |
| Normal sign-out | 5.2 ms, root empty |

Procedure:
1. Stop polling for the account and close the store.
2. Rename `<acct>` to `<acct>.deleting-N` (one atomic step).
3. `remove_dir_all` it.
4. Sweep `*.deleting-*` at startup.
5. Revoke the token and delete `<sub>.dpapi`.

## Q8. Push (desk research, 2026-10-25)
Sources:
- https://developers.google.com/workspace/gmail/api/guides/push
- https://developers.google.com/workspace/gmail/api/reference/rest/v1/users/watch
- https://developers.google.com/workspace/gmail/api/guides/sync
- https://developers.google.com/workspace/gmail/api/reference/quota
- https://cloud.google.com/pubsub/pricing

What push would take:
- `users.watch` is a POST, which CLAUDE.md forbids.
- A Pub/Sub topic with publish rights for `gmail-api-push@system.gserviceaccount.com`.
- A subscription. A webhook needs a public HTTPS endpoint. A pull subscription needs the Pub/Sub scope or a service-account key, which every installed copy would have to hold.
- Renewal at least every 7 days.
- Notifications carry only the email and `historyId`, so `history.list` is still needed. They can be delayed or dropped, so a polling fallback is required anyway.
- Pricing: Gmail calls are free; the exact Pub/Sub figures couldn't be read from the page.
- Google itself recommends polling for installed apps.

**Verdict: polling.** Push would save about half the polling interval (~2.5 s at 5 s).

## Recommended sync algorithm
1. **Startup, per account, in parallel:**
   - Show cached rows right away.
   - Call `access_token`, then poll.
   - With no cursor, do a cold fill: `getProfile`, then `messages.list(INBOX, 20)`, then hydrate (10 parallel).
2. **Poll:**
   - Call `history.list(startHistoryId, historyTypes = messageAdded, messageDeleted, labelAdded, labelRemoved, maxResults 500)`, following `nextPageToken`.
   - Apply the events, then fetch metadata for newly interesting ids.
   - Advance the cursor to the response's `historyId`, but only after a successful apply.
3. **Cadence and Refresh:** follow the Q2 tables and the `PollGate` rules.
4. **Backoff:** follow the Q2 table. A 403 `rateLimitExceeded` counts as throttling.
5. **404:** resync (Q3).
6. **Open a message:** show the cached body if there is one. Otherwise fetch raw, cache it and show it. Stale rows get a metadata check first.

## Recommended cache schema
```
cache\<account-id>\cache.db      (WAL, synchronous=NORMAL)
cache\<account-id>\key.dpapi     (DPAPI-wrapped 32-byte key)
```
```sql
meta(k PRIMARY KEY, v)                 -- history_id, synced_at, schema_version
msgs(id PK, thread_id, internal_date, history_id, size_estimate, fetched_at, stale, meta BLOB)  -- meta = AEAD(JSON headers+snippet)
msg_labels(id, label, PK(id,label)) WITHOUT ROWID; INDEX(label,id)
bodies(id PK, raw BLOB, fetched_at, last_opened)   -- raw RFC 822, AEAD
windows(label PK, oldest_date, complete, as_of)
```

## Still open
- Arrival-to-seen latency: measured once at 5 s (15.9 s, mostly Gmail-side delay). 10 s and 15 s were skipped.
- A real archive, star and delete check (user step 2): **skipped (user's call)**. The delta rules are covered by unit tests, and the no-change live check matched.
- SQLCipher numbers (needs Perl; (d) probably makes them unnecessary).
- The compression ratio.
- A force-refresh API in gtoken.
- Quota under real single-user use. The throttling seen here came from the parallel spikes.
- Exact Pub/Sub pricing.
- Lazy re-listing of non-INBOX windows after a resync is designed but not tested.
- Search over encrypted metadata needs an in-memory index.

## User steps (in `spikes\s4-sync`; the caches in `out\cache\` must stay until step 2)
1. Run `.\target\release\s4-sync.exe watch account-a@example.com --every 5s,10s,15s --minutes 2`. After each "stage N/3 … send NOW" banner, send one mail to dylan.
2. In Gmail, among dylan's newest 20 INBOX messages, archive one, star one and delete one. Then run `.\target\release\s4-sync.exe verify-cache account-a@example.com`. Optionally repeat for ziggy.
3. Afterwards, delete `out\cache\`, which holds real mail.
