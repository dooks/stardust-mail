# S0: token helper (`gtoken`)

## TL;DR
- `gtoken` is a cargo package (lib + bin, edition 2024) that does Google's installed-app OAuth:
  - system browser, loopback redirect to `127.0.0.1:<ephemeral>`, PKCE S256, and `state`
  - one DPAPI-encrypted refresh-token file per Google account
  - access tokens cached in memory
- The OAuth code is handwritten (~150 lines). It doesn't use the `oauth2` crate.
- It builds, its 21 unit tests pass (synthetic data, no network), and `clippy -D warnings` is clean.
- **It works live:** 3 accounts are signed in, and `check` passes in a fresh process (see "Live results"). S1–S4 can now use it.

## How to run
Put Google's Desktop-app JSON at `%LOCALAPPDATA%\com.ziggy.stardust-mail-spikes\oauth-client.json`, then run these in `spikes\s0-token`:
```
cargo run -- login [--hint you@gmail.com]   # opens the browser; prints "signed in: <email> (messagesTotal N)"
cargo run -- list                           # email, scopes and created_at per account
cargo run -- check you@gmail.com            # refresh + getProfile; prints email, messagesTotal, historyId, scopes (never a token)
```
`login` also prints the consent URL to stderr as a fallback; it holds no secrets. It times out after 5 minutes.

## Public API (for S1–S4: `gtoken = { path = "../s0-token" }`)
```rust
pub struct Client { pub client_id: String, pub client_secret: String }  // Debug hides the secret
pub struct AccountInfo { pub email: String, pub sub: String, pub scopes: Vec<String>, pub created_at: u64 } // unix secs
pub struct Profile { pub email_address: String, pub messages_total: u64, pub history_id: String }
pub struct Account { pub info: AccountInfo, pub profile: Profile }
pub struct AccessToken { pub token: String, pub scope: String, /* private expiry */ } // Debug hides the token
pub fn data_dir() -> PathBuf;
pub fn load_client() -> Result<Client, Error>;
pub async fn login(hint: Option<&str>) -> Result<Account, Error>;
pub fn accounts() -> Result<Vec<AccountInfo>, Error>;
pub async fn access_token(email: &str) -> Result<String, Error>;
pub async fn access_token_full(email: &str) -> Result<AccessToken, Error>;
pub async fn profile(email: &str) -> Result<Profile, Error>;
pub fn http() -> reqwest::Client;
pub fn format_utc(secs: u64) -> String;
pub enum Error { ClientFile, Io, Dpapi, Json, Transport, BrowserOpen, AccessDenied, AuthRedirect(String),
  StateMismatch, Timeout, MissingScope{granted}, NoRefreshToken, InvalidIdToken, Revoked(String),
  OAuth{status,error}, Api{status,error}, UnknownAccount(String) }
```

Usage:
```rust
let http = gtoken::http();
for acct in gtoken::accounts()? {
    let tok = gtoken::access_token(&acct.email).await?;   // cached; refreshed 60 s before expiry
    let labels: serde_json::Value = http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/labels")
        .bearer_auth(&tok)
        .send().await?.error_for_status()?.json().await?;
    println!("{}: {} labels", acct.email, labels["labels"].as_array().map_or(0, |l| l.len()));
}
```

Errors worth knowing:
- `Revoked(email)`: `invalid_grant`. Access was revoked, or the 7-day expiry hit while the app is in Testing.
- `MissingScope{granted}`: granular consent with Gmail unticked.
- `AccessDenied`, `Timeout`, `StateMismatch`, `UnknownAccount`.
- `OAuth/Api{status,error}`.

Error messages carry only the status and Google's `error` field.

`access_token` is safe to call concurrently: there is one refresh per account at a time, and the cache key is the lower-cased email. `check` prints the `scope` from the refresh response and makes no `tokeninfo` call; S1 can call `tokeninfo` itself.

## `oauth2` crate vs handwritten
**Handwritten.** The flow is one auth URL, one code exchange, and one refresh.
- The pure parts (PKCE, the URL, redirect parsing, the scope check, the JWT payload) come to ~100 lines and are unit-tested.
- HTTP reuses the reqwest client we need anyway.
- The `oauth2` crate (v5) would add a typestate client, its own HTTP-client abstraction and error types to map. It still wouldn't provide the loopback listener, the scope check, id_token decoding, or the `invalid_grant` → `Revoked` mapping. It saves no code, and it hides the wire details that S1 needs to record.

Other choices:
- `getrandom` as the CSPRNG: the verifier is 48 bytes (64 chars) and `state` is 24 bytes.
- `sha2` for S256.
- `ShellExecuteW` via the `windows` crate to open the browser.
- `reqwest::Url` for encoding and query parsing.

## Token store layout
```
%LOCALAPPDATA%\com.ziggy.stardust-mail-spikes\
  oauth-client.json          (user-provided, never read by agents)
  accounts\<sub>.dpapi       DPAPI(CurrentUser, UI forbidden, entropy "stardust-mail-spikes-token") of
                             {"email","sub","refresh_token","scopes":[..],"created_at":<unix secs>}
```
- Each file is written as `<sub>.dpapi.tmp`, then renamed. Signing in with the same `sub` replaces the file.
- `sub` is checked against `[A-Za-z0-9_-]+` before it's used as a file name.
- Access tokens live in memory only.

## Caveats
- A redirect with the wrong `state` aborts the login. Other paths (such as favicon) get a 404 and are ignored.
- `accounts()` fails as a whole if any one file can't be decrypted.
- In Testing mode, refresh tokens are expected to die after 7 days (S1 confirms this). After that, `access_token` returns `Revoked`, and `gtoken login` has to be run again.

## Live results (2026-10-25, user present, publishing status: Testing)
- **The browser opened** via ShellExecuteW, and the loopback redirect worked first time. Google accepted the `+`-encoded scopes.
- **Granular consent bites in practice.** On the first attempt the Gmail checkbox was left unticked. Google returned only `openid` and `userinfo.email` (still a successful exchange), and `login` refused with `MissingScope`, storing nothing. The retry, with Gmail ticked, worked.
  **For the real app:** this is a normal path, not an edge case. The UI needs a clear "tick 'Read your email', then try again" message, and a one-click retry.
- **Scopes granted:** `gmail.readonly`, `userinfo.email` and `openid`. Nothing broader.
- **Accounts signed in:** 3 (a third was added by the user afterwards). Each is stored as its own `<sub>.dpapi` file, and `list` shows all of them.
- **`check` in a fresh process:** OK for both accounts. The refresh and `getProfile` both returned 200.
- Still to do in S1: the exact unverified-app interstitial steps, Testing vs Production, and token expiry and revocation.
