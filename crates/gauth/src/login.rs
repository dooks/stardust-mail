//! Loopback + PKCE sign-in in the system browser. One pending sign-in per app (global slot).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mail_model::AuthError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::client::{Client, error_field, http, transport_text};
use crate::store::{StoredAccount, valid_sub};
use crate::{SCOPE_GMAIL_READONLY, SCOPES_IDENTITY};

/// The one-pending-login slot of the whole app.
static SLOT: AtomicBool = AtomicBool::new(false);

/// Holds [`SLOT`] until dropped.
struct SlotGuard;

impl SlotGuard {
    fn take() -> Result<SlotGuard, AuthError> {
        SLOT.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| SlotGuard)
            .map_err(|_| AuthError::AlreadyPending)
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        SLOT.store(false, Ordering::Release);
    }
}

/// A prepared sign-in: loopback listener bound on `127.0.0.1:<ephemeral>`, consent URL built, and the global
/// "one pending login" slot held until this value is dropped or completed. The browser has NOT been opened yet.
pub struct PendingLogin {
    url: String,
    redirect_uri: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    client: Client,
    cancel: Arc<Notify>,
    _slot: SlotGuard,
}

/// A cloneable handle to a pending sign-in, for the UI commands (`reopen_sign_in`, `cancel_sign_in`) that run
/// while another task is awaiting [`complete_login`]. Obtain it with [`PendingLogin::handle`] before completing.
#[derive(Clone)]
pub struct LoginHandle {
    url: String,
    cancel: Arc<Notify>,
}

impl PendingLogin {
    /// The consent URL (client id, challenge and state only; no secrets).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Open (or re-open) the consent URL in the system browser via the OS. Never a webview.
    pub fn reopen_browser(&self) -> Result<(), AuthError> {
        open_browser(&self.url)
    }

    /// Cancel: the task awaiting [`complete_login`] returns `AuthError::Cancelled`. The permit is stored, so
    /// cancelling just before `complete_login` starts waiting still works.
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }

    /// A cloneable handle for reopen / cancel from other tasks.
    pub fn handle(&self) -> LoginHandle {
        LoginHandle {
            url: self.url.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl LoginHandle {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Same as [`PendingLogin::reopen_browser`].
    pub fn reopen_browser(&self) -> Result<(), AuthError> {
        open_browser(&self.url)
    }

    /// Same as [`PendingLogin::cancel`].
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }
}

/// `ShellExecuteW` "open".
fn open_browser(url: &str) -> Result<(), AuthError> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let url = HSTRING::from(url);
    // SAFETY: valid NUL-terminated wide strings for the duration of the call.
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &url,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = r.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(AuthError::Browser(format!("ShellExecute returned {code}")))
    }
}

fn random_b64(len: usize) -> Result<String, AuthError> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf)
        .map_err(|_| AuthError::Protocol("the OS random generator failed".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// PKCE S256 challenge: `base64url(sha256(verifier))`.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn scope_string() -> String {
    let mut parts: Vec<&str> = SCOPES_IDENTITY.to_vec();
    parts.push(SCOPE_GMAIL_READONLY);
    parts.join(" ")
}

fn auth_url(
    client: &Client,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    hint: Option<&str>,
) -> Result<String, AuthError> {
    let mut url = reqwest::Url::parse(&client.endpoints.auth)
        .map_err(|_| AuthError::Config("the authorization endpoint is not a valid URL".into()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scope_string())
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent select_account");
        if let Some(h) = hint {
            q.append_pair("login_hint", h);
        }
    }
    Ok(url.into())
}

/// Bind the loopback listener, build the consent URL (`access_type=offline`, `prompt=consent select_account`,
/// PKCE S256, `state`, optional `login_hint`, scopes = `gmail.readonly openid email`) and take the global slot.
/// `AuthError::AlreadyPending` if another login is pending. Does not open the browser (the UI shows the
/// "tick Read all your email" instruction first, then calls `reopen_browser`).
pub async fn begin_login(client: &Client, hint: Option<&str>) -> Result<PendingLogin, AuthError> {
    let slot = SlotGuard::take()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
        AuthError::Network(format!(
            "cannot open the loopback listener ({:?})",
            e.kind()
        ))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| AuthError::Network(format!("cannot read the listener port ({:?})", e.kind())))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_b64(48)?; // 64 chars; RFC 7636 allows 43..=128
    let state = random_b64(24)?;
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    let url = auth_url(
        client,
        &redirect_uri,
        &pkce_challenge(&verifier),
        &state,
        hint,
    )?;
    Ok(PendingLogin {
        url,
        redirect_uri,
        listener,
        state,
        verifier,
        client: client.clone(),
        cancel: Arc::new(Notify::new()),
        _slot: slot,
    })
}

/// A good redirect.
#[derive(Debug, PartialEq, Eq)]
struct Redirect {
    code: String,
    /// The `scope` parameter Google adds to the redirect (may be absent).
    scope: Option<String>,
}

fn has_gmail_readonly(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == SCOPE_GMAIL_READONLY)
}

/// Split the first line of an HTTP request into (path, query). Only GET is accepted.
fn parse_request_line(head: &str) -> Option<(String, String)> {
    let line = head.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "GET" {
        return None;
    }
    let target = it.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some((path.to_string(), query.to_string()))
}

/// Interpret the redirect query: `state` first, then `error`, then `code`.
fn interpret_query(query: &str, expected_state: &str) -> Result<Redirect, AuthError> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1/?{query}"))
        .map_err(|_| AuthError::StateMismatch)?;
    let (mut code, mut state, mut error, mut scope) = (None, None, None, None);
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "scope" => scope = Some(v.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(AuthError::StateMismatch);
    }
    if let Some(e) = error {
        return Err(if e == "access_denied" {
            AuthError::Denied
        } else {
            AuthError::Protocol(format!(
                "authorization error: {}",
                e.chars().take(40).collect::<String>()
            ))
        });
    }
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AuthError::Protocol("the redirect carried no authorization code".into()))?;
    Ok(Redirect { code, scope })
}

/// Fixed texts only: nothing from the request is ever echoed.
fn page_for(result: &Result<Redirect, AuthError>) -> (&'static str, &'static str) {
    match result {
        Ok(r) if r.scope.as_deref().is_some_and(|s| !has_gmail_readonly(s)) => (
            "200 OK",
            "The Gmail permission was not granted. Go back to the app and try again, and this time tick \"Read all your email\". You can close this tab.",
        ),
        Ok(_) => (
            "200 OK",
            "Signed in. You can close this tab and return to the app.",
        ),
        Err(AuthError::Denied) => ("200 OK", "Sign-in was cancelled. You can close this tab."),
        Err(AuthError::StateMismatch) => (
            "400 Bad Request",
            "Sign-in rejected. You can close this tab.",
        ),
        Err(_) => (
            "200 OK",
            "Sign-in failed. You can close this tab and see the app for details.",
        ),
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
    if buf.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

async fn respond(sock: &mut TcpStream, status: &str, msg: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Stardust Mail</title>\
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

/// Accept connections until a usable redirect arrives, the timeout elapses, or `cancel` fires.
async fn wait_for_redirect(
    listener: &TcpListener,
    state: &str,
    timeout: Duration,
    cancel: &Notify,
) -> Result<Redirect, AuthError> {
    let work = async {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let head =
                match tokio::time::timeout(Duration::from_secs(5), read_head(&mut sock)).await {
                    Ok(Some(h)) => h,
                    _ => continue,
                };
            let Some((path, query)) = parse_request_line(&head) else {
                respond(&mut sock, "405 Method Not Allowed", "Not supported.").await;
                continue;
            };
            if path != "/" || query.is_empty() {
                respond(&mut sock, "404 Not Found", "Not found.").await;
                continue;
            }
            let result = interpret_query(&query, state);
            let (status, msg) = page_for(&result);
            respond(&mut sock, status, msg).await;
            return result;
        }
    };
    tokio::select! {
        biased;
        _ = cancel.notified() => Err(AuthError::Cancelled),
        r = work => r,
        _ = tokio::time::sleep(timeout) => Err(AuthError::Timeout),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    refresh_token: Option<String>,
    #[serde(default)]
    scope: String,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    email: Option<String>,
}

/// Decode the payload of a JWT without verifying the signature (it came from Google's token endpoint over TLS).
fn decode_id_token(jwt: &str) -> Result<IdClaims, AuthError> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| AuthError::Protocol("the id_token is not a JWT".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| AuthError::Protocol("the id_token payload is not base64url".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| AuthError::Protocol("the id_token payload is not the expected JSON".into()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wait for the redirect (404 for stray requests), verify `state`, exchange the code, decode the id token, check
/// the granted scope contains `gmail.readonly` (otherwise `MissingScope{granted}` and NOTHING is stored), and
/// return the account. The caller saves it with [`crate::Store::save`] (so a re-sign-in replaces the old file and
/// clears `needs_reauth`). Errors: `Cancelled`, `Timeout`, `Denied`, `StateMismatch`, `Network`, `Protocol`.
pub async fn complete_login(
    pending: PendingLogin,
    timeout: Duration,
) -> Result<StoredAccount, AuthError> {
    let redirect =
        wait_for_redirect(&pending.listener, &pending.state, timeout, &pending.cancel).await?;

    let resp = http()
        .post(&pending.client.endpoints.token)
        .form(&[
            ("code", redirect.code.as_str()),
            ("client_id", pending.client.client_id.as_str()),
            ("client_secret", pending.client.client_secret.as_str()),
            ("code_verifier", pending.verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", pending.redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AuthError::Network(transport_text(e)))?;
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let err = error_field(resp).await;
        return Err(if code >= 500 {
            AuthError::Network(format!("token endpoint HTTP {code}"))
        } else {
            AuthError::Protocol(format!("token endpoint HTTP {code}: {err}"))
        });
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .map_err(|_| AuthError::Protocol("unexpected token response shape".into()))?;

    if !has_gmail_readonly(&tr.scope) {
        return Err(AuthError::MissingScope {
            granted: tr.scope.split_whitespace().map(str::to_string).collect(),
        });
    }
    let refresh_token = tr
        .refresh_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AuthError::Protocol("Google returned no refresh token".into()))?;
    let id_token = tr
        .id_token
        .ok_or_else(|| AuthError::Protocol("Google returned no id_token".into()))?;
    let claims = decode_id_token(&id_token)?;
    if !valid_sub(&claims.sub) {
        return Err(AuthError::Protocol(
            "the id_token sub has unexpected characters".into(),
        ));
    }
    let email = claims
        .email
        .filter(|e| !e.is_empty())
        .ok_or_else(|| AuthError::Protocol("the id_token carries no email".into()))?;
    Ok(StoredAccount {
        email,
        sub: claims.sub,
        refresh_token,
        scopes: tr.scope.split_whitespace().map(str::to_string).collect(),
        created_at: now_secs(),
        needs_reauth: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Endpoints;
    use httpmock::prelude::*;

    /// The slot is global, so every test that calls `begin_login` runs one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn client_for(server: Option<&MockServer>) -> Client {
        let mut endpoints = Endpoints::default();
        if let Some(s) = server {
            endpoints.token = s.url("/token");
        }
        Client {
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: "csecret".into(),
            endpoints,
        }
    }

    async fn get(port: u16, path: impl Into<String>) -> String {
        let path: String = path.into();
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn bound() -> (TcpListener, u16) {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    fn port_of(p: &PendingLogin) -> u16 {
        p.listener.local_addr().unwrap().port()
    }

    fn jwt(claims: &str) -> String {
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.as_bytes())
        )
    }

    const GMAIL: &str = "https://www.googleapis.com/auth/gmail.readonly";

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_have_the_right_shape() {
        let v = random_b64(48).unwrap();
        assert!((43..=128).contains(&v.len()));
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(v, random_b64(48).unwrap());
    }

    #[test]
    fn auth_url_has_all_params_encoded() {
        let c = client_for(None);
        let u = auth_url(
            &c,
            "http://127.0.0.1:5555",
            "CHAL",
            "ST",
            Some("a+b@gmail.com"),
        )
        .unwrap();
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555"));
        assert!(u.contains("login_hint=a%2Bb%40gmail.com"));
        assert!(!u.contains(' '));
        let parsed = reqwest::Url::parse(&u).unwrap();
        let get = |k: &str| {
            parsed
                .query_pairs()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(get("response_type").as_deref(), Some("code"));
        assert_eq!(
            get("scope").as_deref(),
            Some("openid email https://www.googleapis.com/auth/gmail.readonly")
        );
        assert_eq!(get("code_challenge").as_deref(), Some("CHAL"));
        assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
        assert_eq!(get("state").as_deref(), Some("ST"));
        assert_eq!(get("access_type").as_deref(), Some("offline"));
        assert_eq!(get("prompt").as_deref(), Some("consent select_account"));
        assert_eq!(get("login_hint").as_deref(), Some("a+b@gmail.com"));
        assert!(!u.contains("csecret"));
        let no_hint = auth_url(&c, "http://127.0.0.1:1", "x", "s", None).unwrap();
        assert!(!no_hint.contains("login_hint"));
    }

    #[test]
    fn request_line_parsing() {
        assert_eq!(
            parse_request_line("GET /?code=a&state=b HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("/".into(), "code=a&state=b".into()))
        );
        assert_eq!(
            parse_request_line("GET /favicon.ico HTTP/1.1\r\n"),
            Some(("/favicon.ico".into(), String::new()))
        );
        assert_eq!(parse_request_line("POST / HTTP/1.1\r\n"), None);
        assert_eq!(parse_request_line(""), None);
    }

    #[test]
    fn query_interpretation() {
        let r = interpret_query("state=S1&code=4%2Fabc&scope=x+y", "S1").unwrap();
        assert_eq!(
            (r.code.as_str(), r.scope.as_deref()),
            ("4/abc", Some("x y"))
        );
        assert_eq!(
            interpret_query("error=access_denied&state=S1", "S1"),
            Err(AuthError::Denied)
        );
        assert!(matches!(
            interpret_query("error=server_error&state=S1", "S1"),
            Err(AuthError::Protocol(_))
        ));
        assert_eq!(
            interpret_query("code=abc", "S1"),
            Err(AuthError::StateMismatch)
        );
        assert_eq!(
            interpret_query("code=abc&state=EVIL", "S1"),
            Err(AuthError::StateMismatch)
        );
        assert!(matches!(
            interpret_query("state=S1", "S1"),
            Err(AuthError::Protocol(_))
        ));
    }

    #[test]
    fn scope_check_is_exact() {
        assert!(has_gmail_readonly(&format!(
            "openid https://www.googleapis.com/auth/userinfo.email {GMAIL}"
        )));
        assert!(!has_gmail_readonly(&format!("openid email {GMAIL}.extra")));
        assert!(!has_gmail_readonly("openid email"));
    }

    #[test]
    fn id_token_decode() {
        let c = decode_id_token(&jwt(
            r#"{"iss":"x","sub":"1234567890","email":"test@example.com"}"#,
        ))
        .unwrap();
        assert_eq!(c.sub, "1234567890");
        assert_eq!(c.email.as_deref(), Some("test@example.com"));
        assert!(decode_id_token("nope").is_err());
        assert!(decode_id_token("a.!!!.c").is_err());
    }

    #[tokio::test]
    async fn listener_ignores_noise_then_accepts_and_never_echoes() {
        let (l, port) = bound().await;
        let cancel = Arc::new(Notify::new());
        let c2 = cancel.clone();
        let t =
            tokio::spawn(
                async move { wait_for_redirect(&l, "ST", Duration::from_secs(10), &c2).await },
            );
        assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(get(port, "/other?x=1").await.starts_with("HTTP/1.1 404"));
        assert!(get(port, "/").await.starts_with("HTTP/1.1 404"));
        let ok = get(
            port,
            format!(
                "/?code=abc&state=ST&scope=openid+{}",
                GMAIL.replace(':', "%3A").replace('/', "%2F")
            ),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("Signed in"));
        assert_eq!(t.await.unwrap().unwrap().code, "abc");
    }

    #[tokio::test]
    async fn pages_are_fixed_text_that_never_echo_query_values() {
        for q in [
            "code=SECRETCODE&state=ST&scope=openid+email",
            "error=ECHOME&state=ST",
            "code=SECRETCODE&state=ECHOSTATE",
            "error=access_denied&state=ST",
        ] {
            let (l, port) = bound().await;
            let cancel = Notify::new();
            let (r, page) = tokio::join!(
                wait_for_redirect(&l, "ST", Duration::from_secs(10), &cancel),
                get(port, format!("/?{q}"))
            );
            let _ = r;
            for needle in ["SECRETCODE", "ECHOME", "ECHOSTATE"] {
                assert!(!page.contains(needle), "{needle} echoed for {q}");
            }
        }
    }

    #[tokio::test]
    async fn missing_gmail_scope_page_tells_user_to_retry() {
        let (l, port) = bound().await;
        let cancel = Notify::new();
        let (r, page) = tokio::join!(
            wait_for_redirect(&l, "ST", Duration::from_secs(10), &cancel),
            get(port, "/?code=abc&state=ST&scope=openid+email")
        );
        assert!(r.is_ok());
        assert!(page.contains("not granted") && page.contains("tick"));
    }

    #[tokio::test]
    async fn access_denied_and_state_mismatch() {
        for (q, want, err) in [
            (
                "error=access_denied&state=ST",
                "cancelled",
                AuthError::Denied,
            ),
            ("code=abc&state=EVIL", "rejected", AuthError::StateMismatch),
        ] {
            let (l, port) = bound().await;
            let cancel = Notify::new();
            let (r, page) = tokio::join!(
                wait_for_redirect(&l, "ST", Duration::from_secs(10), &cancel),
                get(port, format!("/?{q}"))
            );
            assert_eq!(r.unwrap_err(), err);
            assert!(page.contains(want), "{page}");
        }
    }

    #[tokio::test]
    async fn times_out_when_tab_is_closed() {
        let (l, _) = bound().await;
        let cancel = Notify::new();
        let r = wait_for_redirect(&l, "ST", Duration::from_millis(150), &cancel).await;
        assert_eq!(r.unwrap_err(), AuthError::Timeout);
    }

    #[tokio::test]
    async fn cancel_wins_even_if_signalled_early() {
        let (l, _) = bound().await;
        let cancel = Notify::new();
        cancel.notify_one(); // stored permit: cancel pressed before the wait began
        let r = wait_for_redirect(&l, "ST", Duration::from_secs(30), &cancel).await;
        assert_eq!(r.unwrap_err(), AuthError::Cancelled);
    }

    #[tokio::test]
    async fn one_pending_login_app_wide() {
        let _g = SERIAL.lock().await;
        let c = client_for(None);
        let first = begin_login(&c, None).await.unwrap();
        assert!(matches!(
            begin_login(&c, None).await,
            Err(AuthError::AlreadyPending)
        ));
        drop(first);
        let again = begin_login(&c, Some("  ")).await.unwrap();
        assert!(!again.url().contains("login_hint"));
        // Completing (here: cancelling) also frees the slot.
        again.cancel();
        assert_eq!(
            complete_login(again, Duration::from_secs(5))
                .await
                .unwrap_err(),
            AuthError::Cancelled
        );
        drop(begin_login(&c, None).await.unwrap());
    }

    #[tokio::test]
    async fn early_cancel_through_handle_and_timeout() {
        let _g = SERIAL.lock().await;
        let c = client_for(None);
        let p = begin_login(&c, None).await.unwrap();
        let h = p.handle();
        assert_eq!(h.url(), p.url());
        h.cancel();
        assert_eq!(
            complete_login(p, Duration::from_secs(30))
                .await
                .unwrap_err(),
            AuthError::Cancelled
        );
        let p = begin_login(&c, None).await.unwrap();
        assert_eq!(
            complete_login(p, Duration::from_millis(100))
                .await
                .unwrap_err(),
            AuthError::Timeout
        );
    }

    #[tokio::test]
    async fn full_login_exchanges_code_with_pkce_and_returns_account() {
        let _g = SERIAL.lock().await;
        let server = MockServer::start_async().await;
        let id_token = jwt(r#"{"sub":"1234567890","email":"test@example.com"}"#);
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/token")
                    .form_urlencoded_tuple("grant_type", "authorization_code")
                    .form_urlencoded_tuple("code", "the-code")
                    .form_urlencoded_tuple("client_id", "cid.apps.googleusercontent.com");
                then.status(200).json_body(serde_json::json!({
                    "access_token": "at", "expires_in": 3599, "refresh_token": "rt",
                    "scope": format!("openid https://www.googleapis.com/auth/userinfo.email {GMAIL}"),
                    "id_token": id_token
                }));
            })
            .await;
        let c = client_for(Some(&server));
        let p = begin_login(&c, None).await.unwrap();
        let (port, state, verifier) = (port_of(&p), p.state.clone(), p.verifier.clone());
        let redirect_uri = p.redirect_uri.clone();
        let (r, page) = tokio::join!(
            complete_login(p, Duration::from_secs(10)),
            get(
                port,
                format!("/?code=the-code&state={state}&scope=openid+{GMAIL}")
            )
        );
        assert!(page.contains("Signed in"));
        let a = r.unwrap();
        assert_eq!(
            (a.sub.as_str(), a.email.as_str(), a.refresh_token.as_str()),
            ("1234567890", "test@example.com", "rt")
        );
        assert!(a.scopes.iter().any(|s| s == GMAIL) && !a.needs_reauth && a.created_at > 0);
        assert!(!format!("{a:?}").contains("rt\""));
        m.assert_calls_async(1).await;
        // The verifier sent matches the challenge in the URL is covered by pkce tests; here: shape only.
        assert_eq!(verifier.len(), 64);
        assert!(redirect_uri.starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn missing_scope_returns_granted_and_stores_nothing() {
        let _g = SERIAL.lock().await;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(200).json_body(serde_json::json!({
                    "access_token": "at", "expires_in": 3599, "refresh_token": "rt",
                    "scope": "openid https://www.googleapis.com/auth/userinfo.email",
                    "id_token": jwt(r#"{"sub":"1","email":"t@example.com"}"#)
                }));
            })
            .await;
        let p = begin_login(&client_for(Some(&server)), None).await.unwrap();
        let (port, state) = (port_of(&p), p.state.clone());
        let (r, _) = tokio::join!(
            complete_login(p, Duration::from_secs(10)),
            get(port, format!("/?code=c&state={state}"))
        );
        match r {
            Err(AuthError::MissingScope { granted }) => {
                assert_eq!(
                    granted,
                    vec!["openid", "https://www.googleapis.com/auth/userinfo.email"]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_sub_and_exchange_failures() {
        let _g = SERIAL.lock().await;
        for (status, body, want_network) in [
            (
                200,
                serde_json::json!({"access_token":"at","refresh_token":"rt","scope":GMAIL,"id_token":jwt(r#"{"sub":"../evil","email":"t@example.com"}"#)}),
                false,
            ),
            (
                200,
                serde_json::json!({"access_token":"at","scope":GMAIL,"id_token":jwt(r#"{"sub":"1","email":"t@example.com"}"#)}),
                false,
            ),
            (400, serde_json::json!({"error":"invalid_grant"}), false),
            (503, serde_json::json!({}), true),
        ] {
            let server = MockServer::start_async().await;
            server
                .mock_async(|when, then| {
                    when.method(POST).path("/token");
                    then.status(status).json_body(body.clone());
                })
                .await;
            let p = begin_login(&client_for(Some(&server)), None).await.unwrap();
            let (port, state) = (port_of(&p), p.state.clone());
            let (r, _) = tokio::join!(
                complete_login(p, Duration::from_secs(10)),
                get(port, format!("/?code=c&state={state}"))
            );
            match r {
                Err(AuthError::Network(_)) => assert!(want_network),
                Err(AuthError::Protocol(_)) => assert!(!want_network),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn state_mismatch_and_denied_fail_the_login() {
        let _g = SERIAL.lock().await;
        let c = client_for(None);
        for (q, want) in [
            ("code=c&state=EVIL", AuthError::StateMismatch),
            ("error=access_denied&state=", AuthError::Denied),
        ] {
            let p = begin_login(&c, None).await.unwrap();
            let (port, state) = (port_of(&p), p.state.clone());
            let q = if q.ends_with("state=") {
                format!("{q}{state}")
            } else {
                q.to_string()
            };
            let (r, _) = tokio::join!(
                complete_login(p, Duration::from_secs(10)),
                get(port, format!("/?{q}"))
            );
            assert_eq!(r.unwrap_err(), want);
        }
    }
}
