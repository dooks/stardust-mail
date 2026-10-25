# S1: authentication, accounts and the read-only guarantee

> **Status: final.** The user cut user steps to the essentials (2026-10-25). Section A was partly done (results not recorded). B (Production) is blocked on branding pages. From C, only the revoke check was run. Every other PENDING item is **skipped (user's call)**.

Code:
- `core\` is the library `authcore` (OAuth, account store, sign-in, check, revoke, tokeninfo).
- `src-tauri\` + `ui\` are the Tauri 2.12 app (`com.ziggy.gmail-ro-spikes-s1`). The `embedded` window gets no IPC capability.
- `cli\` is `s1cli`, the error-matrix harness over gtoken's accounts.

Build, clippy and 31 unit tests pass (synthetic data).

## TL;DR
- **Sign-in in Tauri:**
  - `begin_login` binds the loopback listener and opens the browser.
  - `complete_login` waits for the redirect. It can be cancelled and has a configurable timeout.
  - One sign-in can be pending at a time, and the window is raised after the redirect.
- **Read-only is verified** for the existing tokens. `tokeninfo` (POST, form body) lists only `email gmail.readonly userinfo.email openid` for dylan and ziggy. A `labels.list` GET returns 200.
- **Docs confirm the 7-day refresh-token expiry applies to Testing only.** They don't say what flipping to Production does to tokens issued earlier, and they don't explicitly bless personal unverified use of `gmail.readonly`. User steps settle both.
- **Sign-out:** `POST /revoke` (form body), then always delete locally. If the revoke fails, still delete, and tell the user to remove access on myaccount.google.com.
- **Revoking kills the whole grant.** Per the native-app doc, it removes all scopes granted to the project and invalidates tokens for all its clients, so gtoken's token for that account dies too (PENDING live confirmation on dook).

## Answers

### 1. Flow inside Tauri: DONE (code and tests). UI behaviour PENDING.
- The single pending slot is reserved before any await, so a second Add returns `already_pending`. The banner then offers "Re-open browser page" and "Cancel sign-in".
- `cancel_add` → `cancelled`. An early cancel still wins (tested).
- A timeout (default 300 s) → `timeout`. The listener is dropped and the slot cleared.
- `error=access_denied` → "cancelled on the Google page". A state mismatch is rejected. Favicon and other noise get a 404.
- To raise the window it does: unminimize, show, a brief always-on-top toggle, then focus. Whether this actually brings the window forward on Windows 11 is PENDING.

### 2. Embedded WebView2: PENDING
A separate `embedded` window opens the auth URL with a dead redirect and no exchange. Navigation is logged as scheme, host, path and parameter names only. WebView2's user agent looks like Edge, so a refusal isn't guaranteed.

### 3. Consent screen and granular consent: PENDING (user reports)
- From S0: Gmail comes back unticked, and the exchange still succeeds with `openid userinfo.email`.
- The app detects this from `scope`, stores nothing, and shows "tick 'Read your email' and try again" with a retry button.
- An optional probe makes one `labels.list` GET with the reduced token (expected 403) and then discards it.

### 4. Testing vs Production
Docs:
- https://developers.google.com/identity/protocols/oauth2 ("Refresh token expiration"):
  - Testing + External → refresh tokens expire after 7 days, unless only basic scopes are requested.
  - Tokens also die when unused for 6 months, when the password changes (for Gmail scopes), when access is revoked, and at more than 100 refresh tokens per account per client (the oldest goes silently).
- https://support.google.com/cloud/answer/15549945:
  - Authorizations by test users expire 7 days after consent.
  - Unverified apps are limited to 100 new users over the project's lifetime.
  - It says nothing about existing tokens when the project is published.
- https://support.google.com/cloud/answer/7454865: the unverified-app screen appears for sensitive or restricted scopes, with a 100-user cap.
- https://developers.google.com/workspace/gmail/api/auth/scopes: `gmail.readonly` is restricted. Verification and a security assessment are required when the data is stored on servers.

Conclusion: in Production, tokens are expected to be long-lived, and a personal unverified app with at most 100 users should work behind the warning screen. No document says so outright, so the user test decides.

**Live finding (user, 2026-10-25):** the Console **won't publish to Production until branding is complete**: an app home page URL, a privacy policy URL and a terms of service URL. So getting out of the 7-day expiry means hosting three small public pages, normally on a domain listed under "Authorized domains". The alternative is to stay in Testing and sign in each account again every 7 days.

**Decision (user, 2026-10-25): stay in Testing for now.** The branding pages will be made in a separate project, and the app is published to Production later. Until then, **every account needs a sign-in again every 7 days**. So the real app must make `needs_reauth` painless:
- A per-account "Access expired, sign in again" button with `login_hint`.
- The cache stays readable meanwhile.
- No error spam from polling: stop polling that account until it is re-authorised.

Tokens issued in Testing: assume they still die at day 7 (about 2026-11-01) and **sign in again after the flip**. To settle it: leave ziggy's gtoken token alone through the flip, then run `s1cli gtoken-check account-b@example.com` on or after **2026-11-02**. A 200 means the flip rescued it; `invalid_grant` means it didn't.

### 5. Multiple accounts: DONE (design and docs). Live chooser PENDING.
- **Key:** `sub`. It's stable across email changes. Re-login replaces the file (tested, no `.tmp` strays).
- **Email:** taken from the id_token, falling back to `getProfile`, and refreshed on each sign-in.
- **Prompt:** `login_hint` plus `prompt=consent select_account` (consent forces a fresh refresh token). If the user picks a different account than the hint, the app reports `hint_mismatch` instead of failing.
- **Limit:** 100 refresh tokens per account per client. Orphans, for example from an offline sign-out, count toward it.

### 6. Token error matrix
See the table below.

### 7. Sign-out and revoke
- **Order:** revoke first, then always delete locally.
- **Failure rule:** if the revoke fails, show "removed from this app; remove access at myaccount.google.com > Security > Third-party apps and services". Treat `400 invalid_token` as "already gone".
- **Offline** is simulated with `https://127.0.0.1:9/revoke`.
- PENDING: the live responses, whole-grant behaviour, the listing on myaccount.google.com, and external revocation.

### 8. Read-only proof: DONE for existing tokens
- `POST tokeninfo` → `email gmail.readonly userinfo.email openid`, so read_only=true.
- `labels.list` GET → 200 (34 labels for dylan, 29 for ziggy).
- The `judge_scopes` verdict flags anything beyond `gmail.readonly`, `openid` and `email`.
- The only POSTs are `/token`, `/revoke` and `/tokeninfo`.

## Error table
| Case | HTTP | `error` / status | Source |
|---|---|---|---|
| Gmail GET, garbage bearer | 401 | `UNAUTHENTICATED` | measured |
| Gmail GET, no Authorization header | 401 | `UNAUTHENTICATED` | measured |
| Gmail GET, expired real token | expected 401 `UNAUTHENTICATED` | | not tested (garbage covers the handling) |
| `tokeninfo` POST, garbage token | 400 | `invalid_token` | measured |
| Refresh, garbage refresh token | 400 | `invalid_grant` | measured |
| Revoke, garbage token | 400 | `invalid_token` | measured |
| Revoke, valid token | expected 200 | | PENDING (C3) |
| Revoke, same token again | expected 400 `invalid_token` | | PENDING (C3) |
| Revoke, offline | transport error | | PENDING (C2) |
| Refresh with a revoked grant, S1 token | expected 400 `invalid_grant` | | PENDING (C6) |
| Refresh with a revoked grant, gtoken token | **400** | **`invalid_grant`** | **measured**: sign-out in the S1 app killed gtoken's separate token for dook, so revoke kills the whole grant |
| Gmail read with a token lacking gmail.readonly | expected 403 `PERMISSION_DENIED` | | PENDING (A5) |
| Granular consent, Gmail unticked | token exchange 200, scope lacks gmail.readonly | | seen in S0. UI path PENDING. |

App handling:
- A 401 triggers one refresh and a retry.
- `invalid_grant` → `needs_reauth`.
- 403 `PERMISSION_DENIED` → `needs_reauth`.
- A network error → "offline", with no state change.

## Recommended auth state machine
- `signed_out`: no `<sub>.dpapi` file.
- `pending`: one at a time app-wide, entered from Add or "Sign in again".
  - Success → `signed_in`.
  - Cancel, timeout, deny or missing scope → back to the previous state, with a specific message.
- `signed_in`: refresh token on disk. Access tokens live in memory, refreshed 60 s before expiry.
- `needs_reauth`: set on `invalid_grant` (revoked, 7-day Testing expiry, password change) or 403 scope.
  - The account and its cache are kept.
  - The UI shows "access removed or expired. Sign in again", which uses `login_hint=<email>`.
  - The flag is persisted on disk.
- Sign out: revoke, then delete the account file and its cache.

## Storage layout
```
%LOCALAPPDATA%\<app-id>\
  accounts\<sub>.dpapi     DPAPI(CurrentUser, UI forbidden, app entropy) of
                           {email, sub, refresh_token, scopes[], created_at}  (tmp + rename)
```
- `sub` is validated against `[A-Za-z0-9_-]+` before use as a file name.
- A file that can't be decrypted is skipped and counted, rather than failing the whole list (an improvement on S0).
- Add `needs_reauth` to the stored record.

## Still open
- All PENDING items above.
- Whether Production rescues tokens issued in Testing (check on or after 2026-11-02).
- Whether the window really comes to the front on Windows 11.
- WebView2 refusal.
- A real expired-access-token 401.
- A password change.

## User steps (run the app with `cargo run -p s1-auth` in `spikes\s1-auth`; use "Copy log" after each)
**A. Testing mode, ziggy and dylan only.**
1. Add ziggy (hint). Record the Google screen wording and whether the window comes to the front.
2. Add twice while pending → message; then "Re-open browser page", then Cancel.
3. Timeout 10 s → message; reset it to 300.
4. Deny on Google → app message and browser text.
5. Probe on: add ziggy with Gmail unticked → message, Try again, the `labels.list -> HTTP` log line; retry with Gmail ticked.
6. Re-add ziggy → `replaced=true`, one entry.
7. Empty hint → choose dylan; hint dylan → choose ziggy → `hint_mismatch=true`.
8. Check each → `read_only` and tokeninfo log lines.
9. Embedded webview → what it shows, and the `embedded nav:` lines.

**B. Production flip.**
1. `s1cli gtoken-list`.
2. Publish the app in Console; note the dialogs.
3. Add dylan → how the screens differ; Check.
4. Re-check ziggy's old token on or after 2026-11-02.

**C. Revocation, dook only, last.**
1. gtoken-check dook; add dook in the app; Check.
2. Simulate offline → sign out → still listed on myaccount; gtoken still works.
3. Re-add, send revoke twice → 200, then 400 `invalid_token`.
4. gtoken-check dook → `invalid_grant`? Is the app gone from myaccount?
5. `gtoken login --hint dook`; re-add in the app.
6. Delete all connections on myaccount → app Check → needs re-auth; gtoken-check.
7. `gtoken login --hint dook` and check.

## Addendum: granular consent again (2026-10-25)
After the revoke test, dook was signed in again for gtoken, and **the Gmail checkbox was left unticked again**. `login` refused with `MissingScope`. That's 2 of the 5 consent screens the user went through. For a returning user, Google evidently doesn't pre-tick the restricted scope. **The real app's consent step needs an explicit, prominent instruction before the browser opens** ("On Google's page, tick 'Read all your email…'"), not only an error afterwards. dook was left signed out of gtoken; no remaining spike needs it.
