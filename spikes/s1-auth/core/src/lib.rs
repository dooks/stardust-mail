//! `authcore`: the S1 auth building blocks.
//!
//! * `lib.rs`: paths, DPAPI account store keyed by `sub`, Google endpoints (refresh, tokeninfo,
//!   getProfile, labels probe), `check`, `revoke` / `sign_out`.
//! * `login.rs`: loopback + PKCE sign-in split into `begin_login` / `complete_login`, so a UI can
//!   show the URL, cancel, and time out.
//!
//! The only non-GET Gmail-related call is `POST oauth2.googleapis.com/revoke`. Nothing here logs
//! or returns tokens, codes or the client secret; errors carry HTTP status and `error` only.

mod dpapi;
mod error;
mod login;
mod oauth;

pub use error::Error;
pub use login::{
    LoginOpts, LoginOutcome, Pending, begin_login, complete_login, embedded_probe_url, new_cancel, open_browser,
    redact_url,
};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// This spike's own app dir (tokens). The OAuth client file lives in S0's dir.
pub const APP_DIR: &str = "com.ziggy.gmail-ro-spikes-s1";
const CLIENT_DIR: &str = "com.ziggy.gmail-ro-spikes";
/// Token endpoint.
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Revoke endpoint.
pub const REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";
/// Tokeninfo endpoint (POST with a form body).
pub const TOKENINFO_ENDPOINT: &str = "https://oauth2.googleapis.com/tokeninfo";
/// `users.getProfile`.
pub const PROFILE_ENDPOINT: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";
/// `users.labels.list` (error matrix only; only the count is read).
pub const LABELS_ENDPOINT: &str = "https://gmail.googleapis.com/gmail/v1/users/me/labels";
/// An address that refuses connections at once; stands in for "offline".
pub const OFFLINE_REVOKE_URL: &str = "https://127.0.0.1:9/revoke";

// ------------------------------------------------------------------------------------------
// Paths, client, http
// ------------------------------------------------------------------------------------------

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// `%LOCALAPPDATA%\com.ziggy.gmail-ro-spikes-s1` (this spike's token store root).
pub fn data_dir() -> PathBuf {
    local_app_data().join(APP_DIR)
}

/// `<root>\accounts`.
pub fn accounts_dir(root: &Path) -> PathBuf {
    root.join("accounts")
}

/// OAuth client (Google Desktop-app JSON). `Debug` hides the secret.
#[derive(Clone)]
pub struct Client {
    /// Client id.
    pub client_id: String,
    /// Client secret; never printed.
    pub client_secret: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("client_id", &self.client_id).field("client_secret", &"<hidden>").finish()
    }
}

/// Load the client from S0's folder (`...\com.ziggy.gmail-ro-spikes\oauth-client.json`).
pub fn load_client() -> Result<Client, Error> {
    load_client_from(&local_app_data().join(CLIENT_DIR).join("oauth-client.json"))
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
    let bytes = std::fs::read(path)
        .map_err(|e| Error::ClientFile(format!("cannot read {} ({})", path.display(), e.kind())))?;
    let f: File = serde_json::from_slice(&bytes)
        .map_err(|_| Error::ClientFile("not valid JSON of the form {\"installed\": {...}}".into()))?;
    if f.installed.client_id.is_empty() || f.installed.client_secret.is_empty() {
        return Err(Error::ClientFile("client_id or client_secret is empty".into()));
    }
    Ok(Client { client_id: f.installed.client_id, client_secret: f.installed.client_secret })
}

/// Shared HTTP client (rustls, 30 s timeout).
pub fn http() -> reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("gmail-ro-spikes-s1/0.1")
            .build()
            .expect("reqwest client")
    })
    .clone()
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
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

// ------------------------------------------------------------------------------------------
// Store
// ------------------------------------------------------------------------------------------

/// Public facts about a stored account; never a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Email at sign-in time.
    pub email: String,
    /// Stable Google account id; the storage key.
    pub sub: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Sign-in time (unix secs).
    pub created_at: u64,
}

/// Encrypted on disk. No `Debug` on purpose.
#[derive(Serialize, Deserialize)]
pub struct Stored {
    /// Email.
    pub email: String,
    /// Stable id.
    pub sub: String,
    /// Refresh token.
    pub refresh_token: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Unix secs.
    pub created_at: u64,
}

impl Stored {
    /// Token-free view.
    pub fn info(&self) -> AccountInfo {
        AccountInfo {
            email: self.email.clone(),
            sub: self.sub.clone(),
            scopes: self.scopes.clone(),
            created_at: self.created_at,
        }
    }
}

pub(crate) fn account_path(root: &Path, sub: &str) -> Result<PathBuf, Error> {
    if sub.is_empty() || !sub.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(Error::InvalidIdToken("sub has unexpected characters".into()));
    }
    Ok(accounts_dir(root).join(format!("{sub}.dpapi")))
}

/// Write (or replace) an account file atomically (tmp + rename).
pub fn save_account(root: &Path, a: &Stored) -> Result<(), Error> {
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

/// Delete an account file (and any stray tmp). Returns whether a file existed.
pub fn delete_account(root: &Path, sub: &str) -> Result<bool, Error> {
    let path = account_path(root, sub)?;
    let _ = std::fs::remove_file(path.with_extension("dpapi.tmp"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// All stored accounts, sorted by email. A file that cannot be decrypted is skipped and counted
/// in the second value (so one bad file cannot hide the other accounts).
pub fn load_all(root: &Path) -> Result<(Vec<Stored>, usize), Error> {
    let rd = match std::fs::read_dir(accounts_dir(root)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let (mut out, mut bad) = (Vec::new(), 0);
    for entry in rd {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("dpapi") {
            continue;
        }
        let parsed = std::fs::read(&p)
            .ok()
            .and_then(|b| dpapi::unprotect(&b).ok())
            .and_then(|plain| serde_json::from_slice::<Stored>(&plain).ok());
        match parsed {
            Some(s) => out.push(s),
            None => bad += 1,
        }
    }
    out.sort_by(|a, b| a.email.cmp(&b.email));
    Ok((out, bad))
}

/// Find by `sub`.
pub fn find_by_sub(root: &Path, sub: &str) -> Result<Stored, Error> {
    load_all(root)?.0.into_iter().find(|a| a.sub == sub).ok_or_else(|| Error::UnknownAccount(sub.to_string()))
}

// ------------------------------------------------------------------------------------------
// Google endpoints
// ------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: String,
    pub id_token: Option<String>,
}

pub(crate) async fn error_field(resp: reqwest::Response) -> String {
    #[derive(Deserialize)]
    struct E {
        error: Option<String>,
    }
    resp.json::<E>().await.ok().and_then(|e| e.error).unwrap_or_else(|| "unknown".into())
}

pub(crate) async fn token_post(form: &[(&str, &str)]) -> Result<TokenResponse, Error> {
    let resp = http().post(TOKEN_ENDPOINT).form(form).send().await?;
    let status = resp.status();
    if status.is_success() {
        return resp.json::<TokenResponse>().await.map_err(|_| Error::Json("unexpected token response shape".into()));
    }
    Err(Error::OAuth { status: status.as_u16(), error: error_field(resp).await })
}

/// A short-lived access token. `Debug` hides it.
#[derive(Clone)]
pub struct AccessToken {
    /// Bearer token; never log.
    pub token: String,
    /// Scopes from the refresh response.
    pub scope: String,
    /// Lifetime in seconds.
    pub expires_in: u64,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken").field("token", &"<hidden>").field("scope", &self.scope).finish()
    }
}

/// Exchange a refresh token for an access token. `invalid_grant` becomes [`Error::Revoked`].
pub async fn refresh(client: &Client, refresh_token: &str, who: &str) -> Result<AccessToken, Error> {
    let r = token_post(&[
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ])
    .await
    .map_err(|e| match e {
        Error::OAuth { error, .. } if error == "invalid_grant" => Error::Revoked(who.to_string()),
        other => other,
    })?;
    Ok(AccessToken { token: r.access_token, scope: r.scope, expires_in: r.expires_in })
}

/// `users.getProfile` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Address.
    #[serde(rename = "emailAddress")]
    pub email_address: String,
    /// Total messages.
    #[serde(rename = "messagesTotal", default)]
    pub messages_total: u64,
    /// History id.
    #[serde(rename = "historyId", default)]
    pub history_id: String,
}

/// Google API error body -> (HTTP status, `error.status` such as `UNAUTHENTICATED`).
pub async fn api_error_status(resp: reqwest::Response) -> (u16, String) {
    #[derive(Deserialize)]
    struct Body {
        error: Option<Inner>,
    }
    #[derive(Deserialize)]
    struct Inner {
        status: Option<String>,
    }
    let code = resp.status().as_u16();
    let s = resp.json::<Body>().await.ok().and_then(|b| b.error).and_then(|e| e.status).unwrap_or_else(|| "unknown".into());
    (code, s)
}

/// `users.getProfile` with an access token.
pub async fn fetch_profile(access_token: &str) -> Result<Profile, Error> {
    let resp = http().get(PROFILE_ENDPOINT).bearer_auth(access_token).send().await?;
    if resp.status().is_success() {
        return resp.json::<Profile>().await.map_err(|_| Error::Json("unexpected profile response shape".into()));
    }
    let (status, error) = api_error_status(resp).await;
    Err(Error::Api { status, error })
}

/// Result of one `labels.list` GET: status, `error.status` on failure, label count on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiProbe {
    /// HTTP status.
    pub status: u16,
    /// `error.status` if the call failed.
    pub error: Option<String>,
    /// Number of labels if it succeeded.
    pub labels: Option<usize>,
}

impl ApiProbe {
    /// One-line human form.
    pub fn describe(&self) -> String {
        match (&self.error, self.labels) {
            (Some(e), _) => format!("labels.list -> HTTP {} {}", self.status, e),
            (None, Some(n)) => format!("labels.list -> HTTP {} ({n} labels)", self.status),
            _ => format!("labels.list -> HTTP {}", self.status),
        }
    }
}

/// One GET on `labels.list` with an optional bearer token. Reads only the count. `None` sends no
/// Authorization header at all.
pub async fn labels_probe(bearer: Option<&str>) -> Result<ApiProbe, Error> {
    let mut rb = http().get(LABELS_ENDPOINT);
    if let Some(t) = bearer {
        rb = rb.bearer_auth(t);
    }
    let resp = rb.send().await?;
    if resp.status().is_success() {
        let status = resp.status().as_u16();
        let v: serde_json::Value = resp.json().await.map_err(|_| Error::Json("labels shape".into()))?;
        let n = v["labels"].as_array().map_or(0, Vec::len);
        return Ok(ApiProbe { status, error: None, labels: Some(n) });
    }
    let (status, error) = api_error_status(resp).await;
    Ok(ApiProbe { status, error: Some(error), labels: None })
}

/// `tokeninfo` answer reduced to what we need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenInfo {
    /// Scopes the token carries.
    pub scopes: Vec<String>,
    /// Seconds left.
    pub expires_in: Option<u64>,
}

/// `POST /tokeninfo` with the token in the form body (never in a URL).
pub async fn tokeninfo(access_token: &str) -> Result<TokenInfo, Error> {
    let resp = http().post(TOKENINFO_ENDPOINT).form(&[("access_token", access_token)]).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::OAuth { status: status.as_u16(), error: error_field(resp).await });
    }
    let v: serde_json::Value = resp.json().await.map_err(|_| Error::Json("tokeninfo shape".into()))?;
    Ok(parse_tokeninfo(&v))
}

/// Pure part of [`tokeninfo`].
pub fn parse_tokeninfo(v: &serde_json::Value) -> TokenInfo {
    let scopes = v["scope"].as_str().map(oauth::split_scopes).unwrap_or_default();
    let expires_in = match &v["expires_in"] {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    };
    TokenInfo { scopes, expires_in }
}

const ALLOWED_SCOPES: [&str; 4] =
    ["openid", "email", "https://www.googleapis.com/auth/userinfo.email", oauth::SCOPE_GMAIL_READONLY];

/// The read-only verdict: has `gmail.readonly` and nothing outside the allowed set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeVerdict {
    /// True if gmail.readonly is present and nothing else is.
    pub read_only: bool,
    /// Scopes outside the allowed set.
    pub extra: Vec<String>,
}

/// Judge a scope list.
pub fn judge_scopes(scopes: &[String]) -> ScopeVerdict {
    let extra: Vec<String> = scopes.iter().filter(|s| !ALLOWED_SCOPES.contains(&s.as_str())).cloned().collect();
    let has = scopes.iter().any(|s| s == oauth::SCOPE_GMAIL_READONLY);
    ScopeVerdict { read_only: has && extra.is_empty(), extra }
}

// ------------------------------------------------------------------------------------------
// Check
// ------------------------------------------------------------------------------------------

/// Result of "Check".
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    /// getProfile email.
    pub email_address: String,
    /// getProfile count.
    pub messages_total: u64,
    /// Scopes per tokeninfo (POST).
    pub tokeninfo_scopes: Vec<String>,
    /// Read-only verdict.
    pub verdict: ScopeVerdict,
    /// Access token lifetime from the refresh response.
    pub expires_in: u64,
    /// Wall time of refresh / tokeninfo / profile in ms.
    pub timings_ms: [u64; 3],
}

/// Refresh, `tokeninfo`, `getProfile` for the stored account `sub`.
pub async fn check(root: &Path, sub: &str) -> Result<CheckReport, Error> {
    let stored = find_by_sub(root, sub)?;
    let client = load_client()?;
    let t0 = Instant::now();
    let at = refresh(&client, &stored.refresh_token, &stored.email).await?;
    let t1 = Instant::now();
    let ti = tokeninfo(&at.token).await?;
    let t2 = Instant::now();
    let p = fetch_profile(&at.token).await?;
    let t3 = Instant::now();
    let ms = |a: Instant, b: Instant| b.duration_since(a).as_millis() as u64;
    Ok(CheckReport {
        email_address: p.email_address,
        messages_total: p.messages_total,
        verdict: judge_scopes(&ti.scopes),
        tokeninfo_scopes: ti.scopes,
        expires_in: at.expires_in,
        timings_ms: [ms(t0, t1), ms(t1, t2), ms(t2, t3)],
    })
}

// ------------------------------------------------------------------------------------------
// Revoke / sign out
// ------------------------------------------------------------------------------------------

/// What Google (or the network) said to a revoke call. Status and `error` only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevokeOutcome {
    /// `revoked` | `already_invalid` | `offline` | `failed`.
    pub result: String,
    /// HTTP status, if any response arrived.
    pub status: Option<u16>,
    /// Google's `error` field, or the transport error text for `offline`.
    pub error: Option<String>,
}

/// Map an HTTP answer to an outcome (pure, tested).
pub fn classify_revoke(status: u16, error: Option<&str>) -> RevokeOutcome {
    let result = match (status, error) {
        (200, _) => "revoked",
        (400, Some("invalid_token")) => "already_invalid",
        _ => "failed",
    };
    RevokeOutcome { result: result.into(), status: Some(status), error: error.map(str::to_string) }
}

/// `POST <url>` with `token=<t>` in the form body. `url` is a parameter only so tests can
/// simulate being offline; production passes [`REVOKE_ENDPOINT`].
pub async fn revoke(token: &str, url: &str) -> RevokeOutcome {
    let req = http().post(url).timeout(Duration::from_secs(10)).form(&[("token", token)]);
    match req.send().await {
        Err(e) => RevokeOutcome { result: "offline".into(), status: None, error: Some(e.without_url().to_string()) },
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 200 {
                return classify_revoke(200, None);
            }
            let err = error_field(resp).await;
            classify_revoke(status, Some(&err))
        }
    }
}

/// Sign-out options (diagnostics for the spike).
#[derive(Debug, Clone, Default)]
pub struct SignOutOpts {
    /// Point the revoke at an unreachable address (simulated offline).
    pub simulate_offline: bool,
    /// Send the same revoke a second time to record the "already revoked" answer.
    pub resend_revoke: bool,
}

/// What sign-out did.
#[derive(Debug, Clone, Serialize)]
pub struct SignOutReport {
    /// First revoke.
    pub revoke: RevokeOutcome,
    /// Second revoke if requested.
    pub revoke_again: Option<RevokeOutcome>,
    /// Local file removed.
    pub local_deleted: bool,
    /// True if Google may still hold the grant (the revoke neither succeeded nor was already invalid).
    pub remove_manually: bool,
}

/// Revoke at Google, then ALWAYS delete locally (even if revoke failed).
pub async fn sign_out(root: &Path, sub: &str, opts: &SignOutOpts) -> Result<SignOutReport, Error> {
    let stored = find_by_sub(root, sub)?;
    let url = if opts.simulate_offline { OFFLINE_REVOKE_URL } else { REVOKE_ENDPOINT };
    let first = revoke(&stored.refresh_token, url).await;
    let second = if opts.resend_revoke { Some(revoke(&stored.refresh_token, url).await) } else { None };
    let local_deleted = delete_account(root, sub)?;
    let remove_manually = !matches!(first.result.as_str(), "revoked" | "already_invalid");
    Ok(SignOutReport { revoke: first, revoke_again: second, local_deleted, remove_manually })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("authcore-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
    fn store_replace_and_delete_leave_no_strays() {
        let d = tmp("store");
        save_account(&d, &sample("111", "a@example.com")).unwrap();
        save_account(&d, &sample("222", "b@example.com")).unwrap();
        // Same sub again: replaced, still two accounts, email follows the latest sign-in.
        save_account(&d, &sample("111", "a-renamed@example.com")).unwrap();
        let (all, bad) = load_all(&d).unwrap();
        assert_eq!((all.len(), bad), (2, 0));
        assert_eq!(find_by_sub(&d, "111").unwrap().email, "a-renamed@example.com");
        let raw = std::fs::read(accounts_dir(&d).join("111.dpapi")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("synthetic-refresh"));
        assert!(delete_account(&d, "111").unwrap());
        assert!(!delete_account(&d, "111").unwrap());
        let names: Vec<_> = std::fs::read_dir(accounts_dir(&d)).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("222.dpapi")]);
        assert!(matches!(find_by_sub(&d, "111"), Err(Error::UnknownAccount(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_file_is_counted_not_fatal() {
        let d = tmp("corrupt");
        save_account(&d, &sample("111", "a@example.com")).unwrap();
        std::fs::write(accounts_dir(&d).join("999.dpapi"), b"garbage").unwrap();
        let (all, bad) = load_all(&d).unwrap();
        assert_eq!((all.len(), bad), (1, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_sub_rejected() {
        let d = tmp("badsub");
        assert!(save_account(&d, &sample("../evil", "x@example.com")).is_err());
        assert!(delete_account(&d, "a/b").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scope_verdicts() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let ok = judge_scopes(&s(&[
            "openid",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/gmail.readonly",
        ]));
        assert!(ok.read_only && ok.extra.is_empty());
        let wide = judge_scopes(&s(&["https://www.googleapis.com/auth/gmail.readonly", "https://mail.google.com/"]));
        assert!(!wide.read_only);
        assert_eq!(wide.extra, vec!["https://mail.google.com/"]);
        assert!(!judge_scopes(&s(&["openid", "email"])).read_only);
    }

    #[test]
    fn tokeninfo_parsing() {
        let v = serde_json::json!({"scope":"openid https://www.googleapis.com/auth/gmail.readonly","expires_in":"3599","aud":"x"});
        let t = parse_tokeninfo(&v);
        assert_eq!(t.scopes.len(), 2);
        assert_eq!(t.expires_in, Some(3599));
        assert_eq!(parse_tokeninfo(&serde_json::json!({"expires_in":12})).expires_in, Some(12));
        assert!(parse_tokeninfo(&serde_json::json!({})).scopes.is_empty());
    }

    #[test]
    fn revoke_classification() {
        assert_eq!(classify_revoke(200, None).result, "revoked");
        let again = classify_revoke(400, Some("invalid_token"));
        assert_eq!((again.result.as_str(), again.status), ("already_invalid", Some(400)));
        assert_eq!(classify_revoke(400, Some("invalid_request")).result, "failed");
        assert_eq!(classify_revoke(500, None).result, "failed");
    }

    #[tokio::test]
    async fn revoke_offline_is_reported_not_panicked() {
        let r = revoke("synthetic", OFFLINE_REVOKE_URL).await;
        assert_eq!(r.result, "offline");
        assert_eq!(r.status, None);
        assert!(!r.error.unwrap_or_default().contains("synthetic"));
    }

    #[tokio::test]
    async fn sign_out_offline_still_deletes_locally() {
        let d = tmp("signout");
        save_account(&d, &sample("111", "a@example.com")).unwrap();
        let rep = sign_out(&d, "111", &SignOutOpts { simulate_offline: true, resend_revoke: true }).await.unwrap();
        assert_eq!(rep.revoke.result, "offline");
        assert!(rep.local_deleted && rep.remove_manually);
        assert!(rep.revoke_again.is_some());
        assert!(load_all(&d).unwrap().0.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn client_json_parsing_hides_secret() {
        let d = tmp("client");
        let p = d.join("c.json");
        std::fs::write(&p, r#"{"installed":{"client_id":"id","client_secret":"SYNTHETIC"}}"#).unwrap();
        let c = load_client_from(&p).unwrap();
        assert!(!format!("{c:?}").contains("SYNTHETIC"));
        std::fs::write(&p, r#"{"web":{}}"#).unwrap();
        assert!(load_client_from(&p).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn utc_and_error_kinds() {
        assert_eq!(Error::MissingScope { granted: String::new(), probe: None }.kind(), "missing_scope");
        assert_eq!(Error::Revoked("x".into()).kind(), "revoked");
        assert_eq!(Error::Timeout(5).kind(), "timeout");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
