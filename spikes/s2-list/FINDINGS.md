# S2: labels (folders) and message lists

> **Status: final.** Measured on 2026-10-25 on dylan (15.8k messages) and ziggy (5.3k).
> Ziggy's grant was revoked partway through the last runs (`invalid_grant`), most likely by a Sign out in the S1 app (revoke kills the whole grant, see S1). So ziggy's ordering and cold-open numbers rest on fewer samples.

Crate: `spikes\s2-list\`.
- CLI subcommands: `labels`, `list`, `semantics`, `bench`, `threads`, `rows` and `cold`. It refuses the reserved dook account.
- `src\lib.rs` holds the pure helpers: label tree, RFC 2047 decoder, address parser, batch framing, unit costs and percentiles.
- 8 tests pass, and clippy is clean.
- Endpoints: `labels.list/get`, `messages.list`, `messages.get` (metadata), `threads.list/get` (metadata), `getProfile`, and the batch POST, which carries GET sub-requests only.

## TL;DR
- **Quota is the binding constraint.** Current costs (https://developers.google.com/workspace/gmail/api/reference/quota):

  | Call | Units |
  |---|---|
  | `messages.get` | **20** (60 for full or raw) |
  | `threads.get` | 40 |
  | `threads.list` | 10 |
  | `messages.list` | 5 |
  | `history.list` | 2 |
  | labels, profile | 1 |

  - The limit is **6,000 units/minute per user**, so **a 50-row page costs 1,000 units**, a sixth of a minute's budget.
  - Unpaced scans hit **403 `rateLimitExceeded`** (never 429).
  - A full budget does allow bursts: 2,000 units went through in 0.56 s.
- **Hydrate with one batch request** of at most 50 `messages.get?format=metadata`. p50 is **341 ms (dylan) and 522–563 ms (ziggy)**, the fastest option. Concurrent gets at concurrency 20 are the fallback.
- **Use a message list, not conversations.** Only 5.5% and 3.5% of INBOX threads have more than one message, and threads cost twice the units.
- **Gmail already decodes RFC 2047 headers in `format=metadata`** (0 of about 900 rows were left encoded). **Snippets are HTML-escaped**, so decode the entities.
- **List order is not strictly newest-first.** There were 14 inversions in dylan's top 300, the first at position 38, so sort hydrated rows by `internalDate` on the client.

## Q1. Labels
- `labels.list` costs 1 unit and takes 110–355 ms. dylan has 34 labels and ziggy 29.
- **System labels (15 on both):** INBOX, SENT, DRAFT, SPAM, TRASH, STARRED, IMPORTANT, UNREAD, CHAT, YELLOW_STAR, CATEGORY_PERSONAL, CATEGORY_UPDATES, CATEGORY_PROMOTIONS, CATEGORY_SOCIAL and CATEGORY_FORUMS.
- **User labels** are all `Label_N`. There are no colours on either account.
- **Visibility fields** (`labelListVisibility`, `messageListVisibility`) are inconsistent: ziggy hides its main system labels. **Don't use them to choose what to show.** Treat an absent value as "shown".
- **Nesting** is by `/`. dylan has 2 nested user labels (depth 1) whose **parent exists only as a prefix**. `build_tree` adds a non-clickable grouping node for it.
- **Counts** come only from `labels.get`, one call (1 unit) per label: 34 calls in 505 ms at concurrency 10.
- **Counts are not fully reliable.** One user label reports 0 but lists 89 messages, and YELLOW_STAR reports 0 but lists 41 and 18. Elsewhere `messagesTotal` matches what the list returns.
- `getProfile.messagesTotal` covers all mail except spam and trash.

**Proposed folder tree:**
1. Inbox (`INBOX`, with the unread count)
2. Starred
3. Sent
4. Drafts (`DRAFT`)
5. All Mail (no `labelIds`)
6. Spam
7. Trash
8. Categories, collapsible (`labelIds=CATEGORY_*`)
9. User labels, nested

Hidden: UNREAD, IMPORTANT (could be an optional view), CHAT and YELLOW_STAR.

On open, fetch counts only for about 7 folders. User-label counts are fetched lazily.

## Q2. Listing semantics
- **`maxResults`:** the default is 100 and the maximum 500 (anything larger is silently clamped).
  - `pageToken` paging is clean: 14,040 ids came back in 29 pages with 0 duplicates.
  - A bad token or an unknown label id returns 400 `invalidArgument`.
- **Entries** are `{id, threadId}` only.
- **`resultSizeEstimate` is useless.** It reads 201 or 501 whenever there are more results, and is exact only for small labels.
- **`includeSpamTrash`:** `labelIds=SPAM` and `labelIds=TRASH` work without it. Without `labelIds`, the flag adds spam and trash to All Mail.
- **`q`:**
  - `newer_than:1d` works.
  - **`category:primary` returns 0 on both accounts.** Use `labelIds=CATEGORY_PERSONAL` (or `category:personal`).
  - Multiple `labelIds` are AND-ed.
- **Order:** mostly newest-first, but not strictly.
  - dylan INBOX top 300: 14 inversions, the first at position 38, ranging from 79 s to 13.5 h.
  - ziggy: 2 inversions.
  - The `Date` header gives the same inversions. Page boundaries had none.
  - **Sort each page on the client.** A message newer than row 1 could occasionally sit a little deeper in the list.

## Q3. Hydrating 50 rows (INBOX, 6 paced runs per account, no errors)
| Method | dylan p50 / p95 ms | ziggy p50 / p95 ms | Units | KB |
|---|---|---|---|---|
| gets, concurrency 5 | 1266 / 1372 | 2710 / 3035 | 1000 | 52 |
| gets, concurrency 10 | 683 / 763 | 1463 / 1528 | 1000 | 52 |
| gets, concurrency 20 | 476 / 1903 | 911 / 1049 | 1000 | 52 |
| gets, concurrency 10, HTTP/1.1 | 734 / 867 | 1543 / 1568 | 1000 | 52 |
| **batch (one POST)** | **341 / 800** | **563 / 1197** | 1000 | 62 |
| batch + `fields` | 372 / 1282 | 522 / 582 | 1000 | 61 |
| threads.list + 50 threads.get | 897 / 1497 | 1834 / 2015 | 2010 | 87 |

- The batch endpoint is `https://gmail.googleapis.com/batch/gmail/v1`. It takes at most 100 sub-requests, and Google advises 50 or fewer. **Check each sub-response's status**: one failure doesn't fail the batch.
- HTTP/2 is about 7% faster (turn on reqwest's `http2`). `fields` makes no real difference.
- Retry on 403 `rateLimitExceeded` or 429 with a back-off of 1.5 s × attempt, up to 6 attempts. Show "busy", not an error.

## Q4. Messages vs threads (newest 200 INBOX threads)
- 5.5% (dylan) and 3.5% (ziggy) of threads have more than one message, with at most 5 messages.
- 200 messages collapse to 190–192 threads.
- **Recommendation: a message list.** It's simpler, costs half the units, and the newest mail is row 1. A threaded view could come later.

## Q5. List-row metadata (about 900 messages)
- **Always present:** `id`, `threadId`, `labelIds`, `snippet`, `internalDate`, `sizeEstimate`, `historyId`, `payload.mimeType` and `payload.headers` (exactly the requested From/To/Subject/Date). There are no parts.
- **RFC 2047:** already decoded. Non-ASCII appears in about 20% of subjects. Our decoder is only a safety net.
- **From:** a quoted display name in 10–20% of rows, and no name in at most 1.5%. `parse_first_address` handles quotes, commas in names, and bare addresses.
- **Dates:** `internalDate` and `Date` were within 1 minute on every row. **Sort and show `internalDate`.**
- **Snippet:** HTML-escaped (`&#39;` in 33% of rows, `&amp;` in 7%), about 200 characters. Decode the entities.
- **Flags:** UNREAD, STARRED, IMPORTANT and CATEGORY_* come in `labelIds`.
- **Attachment indicator:** there's no label for it. The heuristic is `payload.mimeType == multipart/mixed`, which matched 55/60 and 43/60 rows with attachments, against 5/59 and 6/60 without. Show a paperclip on that basis, or confirm with `q=has:attachment`.
- **`sizeEstimate`:** p50 67 KB (dylan) and 40 KB (ziggy); max 27.9 MB.

## Q6. Quota
- Costs are in the table in the TL;DR. Batch sub-requests count individually.
- Limits: 6,000 units/minute per user, and 1.2M/minute per project.
- **A cold "open account → show inbox"** (labels.list + 7 labels.get + list 50 + 50 gets) is 1,013 units:

  | Method | Wall time from process start |
  |---|---|
  | **batch** | **777 ms** (dylan) |
  | gets, concurrency 10 | 1,495 ms (dylan), 2,123 ms (ziggy) |
  | threads | 1,254 ms (dylan), 2,018 units |

- Each account has its own per-user quota, so accounts don't compete.
- After a heavy run had drained the budget, one list call stalled for 9.4 s.
- **Rule of thumb: at most about 5 pages of 50 per minute per account.**

## Recommended "show a folder" algorithm
1. Get an access token (~200 ms). Show cached rows immediately.
2. In parallel: `labels.list`, plus `labels.get` for the displayed folders.
3. `messages.list(labelIds=<folder>, maxResults=50)`, adding `includeSpamTrash=true` for Spam and Trash.
4. Skip ids that are already cached. Hydrate the rest with **one batch request**: `format=metadata`, `metadataHeaders=From,To,Subject,Date`. For the newest rows, send the first 20 ids as their own batch first (400 units), then the other 30.
5. Sort by `internalDate` descending, decode snippet entities, and take flags from `labelIds`.
6. Fetch the next page only when the user scrolls, and no more than about 5 pages per minute.
7. On 403 `rateLimitExceeded` or 429, back off (1.5 s × attempt, up to 6 attempts) and show a "busy" state.

## Still open
- Why the list order has inversions.
- Whether the per-user limit is a rolling minute or a token bucket.
- Checking the attachment heuristic against `format=full` (S3 has the data).
- `history.list` costs 2 units according to the docs, and S4 measured it at that.
