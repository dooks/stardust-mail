//! `gtoken`: Gmail OAuth for the starmail spikes.
//!
//! * [`login`] runs the installed-app flow (system browser, loopback redirect, PKCE S256, `state`).
//! * Refresh tokens are stored DPAPI-encrypted, one file per Google account (`sub`).
//! * [`access_token`] returns a cached short-lived access token for an account.
//!
//! Only the `gmail.readonly` scope (plus `openid email`) is ever requested. Nothing here logs or
//! prints tokens, codes, or the client secret, and error messages never contain them.

mod dpapi;
mod error;
mod oauth;

pub use error::Error;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const APP_DIR: &str = "com.ziggy.starmail-spikes";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const PROFILE_ENDPOINT: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------------------------

/// The OAuth client (Google "Desktop app" credentials). `Debug` hides the secret.
#[derive(Clone)]
pub struct Client {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret (not actually secret for desktop apps, but we still never print it).
    pub client_secret: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("client_id", &self.client_id).field("client_secret", &"<hidden>").finish()
    }
}

/// Public facts about a stored account. Never contains a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Email address as reported by Google at sign-in.
    pub email: String,
    /// Stable Google account id (`sub` claim). Used as the storage key.
    pub sub: String,
    /// Scopes Google reported as granted.
    pub scopes: Vec<String>,
    /// Sign-in time, Unix seconds (UTC). See [`format_utc`].
    pub created_at: u64,
}

/// `users.getProfile` result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Profile {
    /// The account's email address.
    #[serde(rename = "emailAddress")]
    pub email_address: String,
    /// Total messages in the mailbox.
    #[serde(rename = "messagesTotal", default)]
    pub messages_total: u64,
    /// Current history id.
    #[serde(rename = "historyId", default)]
    pub history_id: String,
}

/// Result of a successful [`login`].
#[derive(Debug, Clone)]
pub struct Account {
    /// What was stored.
    pub info: AccountInfo,
    /// Profile fetched right after sign-in.
    pub profile: Profile,
}

/// A short-lived access token plus the scopes Google says it carries. `Debug` hides the token.
#[derive(Clone)]
pub struct AccessToken {
    /// Bearer token. Put it in an `Authorization: Bearer` header; never log it.
    pub token: String,
    /// Space-separated scopes from the refresh response.
    pub scope: String,
    expires_at: Instant,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken").field("token", &"<hidden>").field("scope", &self.scope).finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Paths, client file, HTTP
// ---------------------------------------------------------------------------------------------

/// `%LOCALAPPDATA%\com.ziggy.starmail-spikes`.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join(APP_DIR)
}

fn accounts_dir(root: &Path) -> PathBuf {
    root.join("accounts")
}

/// Read `<data_dir>\oauth-client.json` (Google's Desktop-app download format).
pub fn load_client() -> Result<Client, Error> {
    load_client_from(&data_dir().join("oauth-client.json"))
}

fn load_client_from(path: &Path) -> Result<Client, Error> {
    #[derive(Deserialize)]
    struct File {
        installed: Inner,
    }
    #[derive(Deserialize)]
    struct Inner {
        client_id: String,
        client_secret: String,
    }
    let bytes = std::fs::read(path).map_err(|e| {
        Error::ClientFile(format!("cannot read {} ({}); download the Desktop-app JSON there", path.display(), e.kind()))
    })?;
    let f: File = serde_json::from_slice(&bytes)
        .map_err(|_| Error::ClientFile("not valid JSON of the form {\"installed\": {client_id, client_secret, ...}}".into()))?;
    if f.installed.client_id.is_empty() || f.installed.client_secret.is_empty() {
        return Err(Error::ClientFile("client_id or client_secret is empty".into()));
    }
    Ok(Client { client_id: f.installed.client_id, client_secret: f.installed.client_secret })
}

/// Shared `reqwest` client (rustls, 30 s timeout). Cheap to clone.
pub fn http() -> reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("starmail-spikes/0.1")
            .build()
            .expect("reqwest client")
    })
    .clone()
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------------------------------------
// Account store (DPAPI files)
// ---------------------------------------------------------------------------------------------

/// What is encrypted on disk. No `Debug` on purpose.
#[derive(Serialize, Deserialize)]
struct Stored {
    email: String,
    sub: String,
    refresh_token: String,
    scopes: Vec<String>,
    created_at: u64,
}

impl Stored {
    fn info(&self) -> AccountInfo {
        AccountInfo {
            email: self.email.clone(),
            sub: self.sub.clone(),
            scopes: self.scopes.clone(),
            created_at: self.created_at,
        }
    }
}

fn account_path(root: &Path, sub: &str) -> Result<PathBuf, Error> {
    // `sub` becomes a file name; refuse anything that could escape the directory.
    if sub.is_empty() || !sub.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(Error::InvalidIdToken("sub has unexpected characters".into()));
    }
    Ok(accounts_dir(root).join(format!("{sub}.dpapi")))
}

fn save_account(root: &Path, a: &Stored) -> Result<(), Error> {
    let path = account_path(root, &a.sub)?;
    std::fs::create_dir_all(accounts_dir(root))?;
    let enc = dpapi::protect(&serde_json::to_vec(a)?).map_err(|e| Error::Dpapi(format!("CryptProtectData: {e}")))?;
    let tmp = path.with_extension("dpapi.tmp");
    std::fs::write(&tmp, enc)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn load_all(root: &Path) -> Result<Vec<Stored>, Error> {
    let dir = accounts_dir(root);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in rd {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("dpapi") {
            continue;
        }
        let plain = dpapi::unprotect(&std::fs::read(&p)?).map_err(|e| Error::Dpapi(format!("CryptUnprotectData: {e}")))?;
        out.push(serde_json::from_slice::<Stored>(&plain)?);
    }
    out.sort_by(|a, b| a.email.cmp(&b.email));
    Ok(out)
}

fn find_account(root: &Path, email: &str) -> Result<Stored, Error> {
    load_all(root)?
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(email))
        .ok_or_else(|| Error::UnknownAccount(email.to_string()))
}

/// All stored accounts (sorted by email). Never returns tokens.
pub fn accounts() -> Result<Vec<AccountInfo>, Error> {
    Ok(load_all(&data_dir())?.iter().map(Stored::info).collect())
}

// ---------------------------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
    refresh_token: Option<String>,
    #[serde(default)]
    scope: String,
    id_token: Option<String>,
}

async fn token_post(form: &[(&str, &str)]) -> Result<TokenResponse, Error> {
    let resp = http().post(TOKEN_ENDPOINT).form(form).send().await?;
    let status = resp.status();
    if status.is_success() {
        return resp.json::<TokenResponse>().await.map_err(|_| Error::Json("unexpected token response shape".into()));
    }
    #[derive(Deserialize)]
    struct E {
        error: Option<String>,
    }
    let error = resp.json::<E>().await.ok().and_then(|e| e.error).unwrap_or_else(|| "unknown".into());
    Err(Error::OAuth { status: status.as_u16(), error })
}

async fn fetch_profile(access_token: &str) -> Result<Profile, Error> {
    let resp = http().get(PROFILE_ENDPOINT).bearer_auth(access_token).send().await?;
    let status = resp.status();
    if status.is_success() {
        return resp.json::<Profile>().await.map_err(|_| Error::Json("unexpected profile response shape".into()));
    }
    #[derive(Deserialize)]
    struct Body {
        error: Option<Inner>,
    }
    #[derive(Deserialize)]
    struct Inner {
        status: Option<String>,
    }
    let error = resp
        .json::<Body>()
        .await
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| e.status)
        .unwrap_or_else(|| "unknown".into());
    Err(Error::Api { status: status.as_u16(), error })
}

/// `users.getProfile` for a signed-in account (refreshes the access token if needed).
pub async fn profile(email: &str) -> Result<Profile, Error> {
    let t = access_token_full(email).await?;
    fetch_profile(&t.token).await
}

// ---------------------------------------------------------------------------------------------
// Access tokens (cached)
// ---------------------------------------------------------------------------------------------

type Slot = Arc<tokio::sync::Mutex<Option<AccessToken>>>;

fn slot_for(email: &str) -> Slot {
    static CACHE: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    let mut map = CACHE.get_or_init(Default::default).lock().expect("cache lock");
    map.entry(email.to_ascii_lowercase()).or_default().clone()
}

/// A valid access token for `email`: cached in-process until 60 s before expiry, refreshed
/// otherwise. Safe to call from many tasks at once (one refresh per account at a time).
/// Returns [`Error::Revoked`] on `invalid_grant`.
pub async fn access_token(email: &str) -> Result<String, Error> {
    Ok(access_token_full(email).await?.token)
}

/// Like [`access_token`] but also returns the scopes from the refresh response.
pub async fn access_token_full(email: &str) -> Result<AccessToken, Error> {
    let slot = slot_for(email);
    let mut guard = slot.lock().await;
    if let Some(t) = guard.as_ref().filter(|t| t.expires_at > Instant::now()) {
        return Ok(t.clone());
    }
    let stored = find_account(&data_dir(), email)?;
    let client = load_client()?;
    let fresh = refresh(&client, &stored.refresh_token, &stored.email).await?;
    *guard = Some(fresh.clone());
    Ok(fresh)
}

async fn refresh(client: &Client, refresh_token: &str, email: &str) -> Result<AccessToken, Error> {
    let r = token_post(&[
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ])
    .await
    .map_err(|e| match e {
        Error::OAuth { error, .. } if error == "invalid_grant" => Error::Revoked(email.to_string()),
        other => other,
    })?;
    let ttl = Duration::from_secs(r.expires_in).saturating_sub(EXPIRY_MARGIN);
    Ok(AccessToken { token: r.access_token, scope: r.scope, expires_at: Instant::now() + ttl })
}

// ---------------------------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------------------------

/// Interactive sign-in. Opens the system browser, waits up to 5 minutes for the redirect,
/// stores the account (replacing any previous file for the same `sub`) and returns it.
pub async fn login(hint: Option<&str>) -> Result<Account, Error> {
    let client = load_client()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = oauth::pkce_verifier()?;
    let state = oauth::random_b64(24)?;
    let url = oauth::auth_url(&oauth::AuthUrlParams {
        client_id: &client.client_id,
        redirect_uri: &redirect_uri,
        code_challenge: &oauth::pkce_challenge(&verifier),
        state: &state,
        login_hint: hint,
    });

    // The URL holds no secrets (client_id, challenge, state only), so printing is fine.
    eprintln!("Opening your browser to sign in. If nothing opens, paste this URL into a browser:\n{url}\n");
    open_browser(&url)?;

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_redirect(&listener, &state))
        .await
        .map_err(|_| Error::Timeout)??;
    drop(listener);

    let tr = token_post(&[
        ("code", &code),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
        ("code_verifier", &verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", &redirect_uri),
    ])
    .await?;

    if !oauth::has_gmail_readonly(&tr.scope) {
        return Err(Error::MissingScope { granted: tr.scope });
    }
    let refresh_token = tr.refresh_token.filter(|t| !t.is_empty()).ok_or(Error::NoRefreshToken)?;
    let id_token = tr.id_token.ok_or_else(|| Error::InvalidIdToken("missing".into()))?;
    let claims = oauth::decode_id_token(&id_token)?;

    let profile = fetch_profile(&tr.access_token).await?;
    let email = claims.email.unwrap_or_else(|| profile.email_address.clone());
    let created_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let stored = Stored { email, sub: claims.sub, refresh_token, scopes: oauth::split_scopes(&tr.scope), created_at };
    save_account(&data_dir(), &stored)?;
    Ok(Account { info: stored.info(), profile })
}

fn open_browser(url: &str) -> Result<(), Error> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let url = HSTRING::from(url);
    // SAFETY: all pointers are valid NUL-terminated wide strings for the duration of the call.
    let r = unsafe { ShellExecuteW(None, w!("open"), &url, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Per docs, values > 32 mean success.
    if r.0 as isize > 32 { Ok(()) } else { Err(Error::BrowserOpen(format!("ShellExecute returned {}", r.0 as isize))) }
}

/// Accept connections until one carries a usable redirect; answer others with 404 and keep waiting.
async fn wait_for_redirect(listener: &TcpListener, state: &str) -> Result<String, Error> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let head = match tokio::time::timeout(Duration::from_secs(5), read_head(&mut sock)).await {
            Ok(Some(h)) => h,
            _ => continue,
        };
        let Some((path, query)) = oauth::parse_request_line(&head) else {
            respond(&mut sock, "405 Method Not Allowed", "Not supported.").await;
            continue;
        };
        if path != "/" || query.is_empty() {
            respond(&mut sock, "404 Not Found", "Not found.").await;
            continue;
        }
        let result = oauth::interpret_query(&query, state);
        let (status, msg) = match &result {
            Ok(_) => ("200 OK", "Signed in. You can close this tab and return to the app."),
            Err(Error::AccessDenied) => ("200 OK", "Sign-in was cancelled. You can close this tab."),
            Err(Error::StateMismatch) => ("400 Bad Request", "Sign-in rejected (state mismatch). You can close this tab."),
            Err(_) => ("200 OK", "Sign-in failed. You can close this tab; see the terminal for details."),
        };
        respond(&mut sock, status, msg).await;
        return result;
    }
}

async fn read_head(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.is_empty() { None } else { Some(String::from_utf8_lossy(&buf).into_owned()) }
}

async fn respond(sock: &mut TcpStream, status: &str, msg: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>starmail</title>\
         <body style=\"font-family:system-ui;margin:3rem\"><p>{msg}</p></body>"
    );
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gtoken-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn client_json_parsing() {
        let d = tmp("client");
        let p = d.join("oauth-client.json");
        std::fs::write(
            &p,
            r#"{"installed":{"client_id":"id.apps.googleusercontent.com","client_secret":"SYNTHETIC","auth_uri":"a","token_uri":"t","redirect_uris":["http://localhost"]}}"#,
        )
        .unwrap();
        let c = load_client_from(&p).unwrap();
        assert_eq!(c.client_id, "id.apps.googleusercontent.com");
        assert_eq!(c.client_secret, "SYNTHETIC");
        assert!(!format!("{c:?}").contains("SYNTHETIC"));

        std::fs::write(&p, r#"{"web":{"client_id":"x","client_secret":"y"}}"#).unwrap();
        assert!(matches!(load_client_from(&p), Err(Error::ClientFile(_))));
        assert!(matches!(load_client_from(&d.join("missing.json")), Err(Error::ClientFile(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn sample(sub: &str, email: &str) -> Stored {
        Stored {
            email: email.into(),
            sub: sub.into(),
            refresh_token: "synthetic-refresh".into(),
            scopes: vec!["openid".into(), oauth::SCOPE_GMAIL_READONLY.into()],
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn account_store_roundtrip_and_replace() {
        let d = tmp("store");
        assert!(load_all(&d).unwrap().is_empty());
        save_account(&d, &sample("111", "b@example.com")).unwrap();
        save_account(&d, &sample("222", "a@example.com")).unwrap();
        let all = load_all(&d).unwrap();
        assert_eq!(all.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["a@example.com", "b@example.com"]);
        assert_eq!(all[0].refresh_token, "synthetic-refresh");

        // File on disk is not plaintext.
        let raw = std::fs::read(accounts_dir(&d).join("111.dpapi")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("synthetic-refresh"));

        // Re-login of same sub replaces.
        let mut again = sample("111", "b-new@example.com");
        again.refresh_token = "newer".into();
        save_account(&d, &again).unwrap();
        let all = load_all(&d).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(find_account(&d, "B-NEW@example.com").unwrap().refresh_token, "newer");
        assert!(matches!(find_account(&d, "nobody@example.com"), Err(Error::UnknownAccount(_))));
        assert!(!accounts_dir(&d).join("111.dpapi.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_sub_rejected() {
        let d = tmp("badsub");
        assert!(save_account(&d, &sample("../evil", "x@example.com")).is_err());
        assert!(save_account(&d, &sample("", "x@example.com")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn utc_format() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn debug_hides_secrets() {
        let t = AccessToken { token: "SECRETTOKEN".into(), scope: "s".into(), expires_at: Instant::now() };
        assert!(!format!("{t:?}").contains("SECRETTOKEN"));
    }

    /// Loopback server end-to-end with a synthetic browser (no network beyond 127.0.0.1).
    #[tokio::test]
    async fn redirect_listener_ignores_noise_then_accepts() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { wait_for_redirect(&listener, "ST").await });
        let get = |path: &'static str| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let ok = get("/?code=abc&state=ST").await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("close this tab"));
        assert_eq!(server.await.unwrap().unwrap(), "abc");
    }
}
