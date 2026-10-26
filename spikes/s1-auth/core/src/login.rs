//! Loopback + PKCE sign-in: `begin_login` -> `complete_login`, with timeout and cancel.

use crate::{
    AccountInfo, Client, Error, Profile, ScopeVerdict, Stored, account_path, fetch_profile, judge_scopes, labels_probe,
    load_client, now_secs, oauth, save_account, token_post, tokeninfo,
};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// A prepared sign-in: listener bound, URL built, nothing opened yet.
pub struct Pending {
    /// Consent URL (client id, challenge, state only; no secrets).
    pub url: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
    client: Client,
    hint: Option<String>,
}

/// Prepare a sign-in. `login_hint` pre-selects an account; `prompt=consent select_account` is
/// always sent (consent guarantees a refresh token; select_account shows the chooser).
pub async fn begin_login(hint: Option<&str>) -> Result<Pending, Error> {
    let client = load_client()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = oauth::pkce_verifier()?;
    let state = oauth::random_b64(24)?;
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    let url = oauth::auth_url(&oauth::AuthUrlParams {
        client_id: &client.client_id,
        redirect_uri: &redirect_uri,
        code_challenge: &oauth::pkce_challenge(&verifier),
        state: &state,
        login_hint: hint,
        prompt: "consent select_account",
    });
    Ok(Pending { url, listener, state, verifier, redirect_uri, client, hint: hint.map(str::to_string) })
}

/// Options for [`complete_login`].
#[derive(Debug, Clone)]
pub struct LoginOpts {
    /// How long to wait for the redirect.
    pub timeout: Duration,
    /// Diagnostic: if Gmail was not granted, make ONE `labels.list` GET with the scope-reduced
    /// access token and report its status (the token is then discarded: not stored, not revoked).
    pub probe_on_missing_scope: bool,
}

/// A finished sign-in.
#[derive(Debug, Clone, Serialize)]
pub struct LoginOutcome {
    /// Stored account.
    pub info: AccountInfo,
    /// True if a file for this `sub` already existed and was replaced.
    pub replaced: bool,
    /// True if a hint was given and the account chosen has another email.
    pub hint_mismatch: bool,
    /// getProfile.
    pub profile: Profile,
    /// Scopes per tokeninfo (POST).
    pub tokeninfo_scopes: Vec<String>,
    /// Read-only verdict.
    pub verdict: ScopeVerdict,
}

/// Open the browser, wait for the redirect (cancel / timeout aware), exchange, store.
/// `on_redirect` runs once the redirect arrived (the UI brings its window to the front there).
pub async fn complete_login(
    root: &Path,
    p: Pending,
    opts: &LoginOpts,
    cancel: &Notify,
    on_redirect: impl FnOnce(),
) -> Result<LoginOutcome, Error> {
    open_browser(&p.url)?;
    let redirect = wait_for_redirect(&p.listener, &p.state, opts.timeout, cancel).await?;
    on_redirect();
    drop(p.listener);

    let tr = token_post(&[
        ("code", &redirect.code),
        ("client_id", &p.client.client_id),
        ("client_secret", &p.client.client_secret),
        ("code_verifier", &p.verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", &p.redirect_uri),
    ])
    .await?;

    if !oauth::has_gmail_readonly(&tr.scope) {
        let probe = if opts.probe_on_missing_scope {
            Some(match labels_probe(Some(&tr.access_token)).await {
                Ok(pr) => pr.describe(),
                Err(e) => format!("probe failed: {e}"),
            })
        } else {
            None
        };
        return Err(Error::MissingScope { granted: tr.scope, probe });
    }
    let refresh_token = tr.refresh_token.filter(|t| !t.is_empty()).ok_or(Error::NoRefreshToken)?;
    let id_token = tr.id_token.ok_or_else(|| Error::InvalidIdToken("missing".into()))?;
    let claims = oauth::decode_id_token(&id_token)?;
    let profile = fetch_profile(&tr.access_token).await?;
    let ti = tokeninfo(&tr.access_token).await?;
    let email = claims.email.unwrap_or_else(|| profile.email_address.clone());
    let replaced = account_path(root, &claims.sub)?.exists();
    let hint_mismatch = p.hint.as_deref().is_some_and(|h| !h.eq_ignore_ascii_case(&email));
    let stored =
        Stored { email, sub: claims.sub, refresh_token, scopes: oauth::split_scopes(&tr.scope), created_at: now_secs() };
    save_account(root, &stored)?;
    Ok(LoginOutcome {
        info: stored.info(),
        replaced,
        hint_mismatch,
        profile,
        verdict: judge_scopes(&ti.scopes),
        tokeninfo_scopes: ti.scopes,
    })
}

/// `ShellExecuteW` "open".
pub fn open_browser(url: &str) -> Result<(), Error> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let url = HSTRING::from(url);
    // SAFETY: valid NUL-terminated wide strings for the duration of the call.
    let r = unsafe { ShellExecuteW(None, w!("open"), &url, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if r.0 as isize > 32 { Ok(()) } else { Err(Error::BrowserOpen(format!("ShellExecute returned {}", r.0 as isize))) }
}

/// The consent URL for the embedded-WebView2 refusal test. Uses a dead loopback redirect and no
/// listener: nothing is ever exchanged. Contains no secrets.
pub fn embedded_probe_url() -> Result<String, Error> {
    let client = load_client()?;
    let verifier = oauth::pkce_verifier()?;
    Ok(oauth::auth_url(&oauth::AuthUrlParams {
        client_id: &client.client_id,
        redirect_uri: "http://127.0.0.1:9",
        code_challenge: &oauth::pkce_challenge(&verifier),
        state: &oauth::random_b64(24)?,
        login_hint: None,
        prompt: "consent select_account",
    }))
}

/// Reduce a URL to scheme + host + path + query KEY names, so it can be shown or logged without
/// risking a code, state or challenge value. A few safe `error*` values are kept.
pub fn redact_url(url: &str) -> String {
    let Ok(u) = reqwest::Url::parse(url) else { return "(unparseable url)".into() };
    let mut keys = Vec::new();
    for (k, v) in u.query_pairs() {
        let keep = matches!(k.as_ref(), "error" | "error_subcode" | "flowName" | "flowEntry");
        keys.push(if keep { format!("{k}={v}") } else { k.into_owned() });
    }
    let host = u.host_str().unwrap_or("");
    let q = if keys.is_empty() { String::new() } else { format!("?{}", keys.join("&")) };
    format!("{}://{}{}{}", u.scheme(), host, u.path(), q)
}

/// Accept connections until a usable redirect arrives, the timeout elapses, or `cancel` fires.
pub(crate) async fn wait_for_redirect(
    listener: &TcpListener,
    state: &str,
    timeout: Duration,
    cancel: &Notify,
) -> Result<oauth::Redirect, Error> {
    let work = async {
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
            let (status, msg) = page_for(&result);
            respond(&mut sock, status, msg).await;
            return result;
        }
    };
    tokio::select! {
        r = work => r,
        _ = tokio::time::sleep(timeout) => Err(Error::Timeout(timeout.as_secs())),
        _ = cancel.notified() => Err(Error::Cancelled),
    }
}

fn page_for(result: &Result<oauth::Redirect, Error>) -> (&'static str, &'static str) {
    match result {
        Ok(r) if r.scope.as_deref().is_some_and(|s| !oauth::has_gmail_readonly(s)) => (
            "200 OK",
            "The Gmail permission was not granted. Go back to the app and try again, and this time tick \"Read your email\". You can close this tab.",
        ),
        Ok(_) => ("200 OK", "Signed in. You can close this tab and return to the app."),
        Err(Error::AccessDenied) => ("200 OK", "Sign-in was cancelled. You can close this tab."),
        Err(Error::StateMismatch) => ("400 Bad Request", "Sign-in rejected (state mismatch). You can close this tab."),
        Err(_) => ("200 OK", "Sign-in failed. You can close this tab and see the app for details."),
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

/// A shareable cancel handle.
pub fn new_cancel() -> Arc<Notify> {
    Arc::new(Notify::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, path: String) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn bound() -> (TcpListener, u16) {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    #[test]
    fn redaction_keeps_no_values() {
        let r = redact_url(
            "https://accounts.google.com/signin/oauth/error?authError=SECRETISH&code=abc&state=zzz&flowName=GeneralOAuthFlow",
        );
        assert_eq!(r, "https://accounts.google.com/signin/oauth/error?authError&code&state&flowName=GeneralOAuthFlow");
        assert!(!r.contains("SECRETISH") && !r.contains("abc") && !r.contains("zzz"));
    }

    #[tokio::test]
    async fn listener_ignores_noise_then_accepts() {
        let (l, port) = bound().await;
        let cancel = Arc::new(Notify::new());
        let c2 = cancel.clone();
        let t = tokio::spawn(async move { wait_for_redirect(&l, "ST", Duration::from_secs(10), &c2).await });
        assert!(get(port, "/favicon.ico".into()).await.starts_with("HTTP/1.1 404"));
        let q = "/?code=abc&state=ST&scope=openid+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.readonly";
        let ok = get(port, q.into()).await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("Signed in"));
        assert_eq!(t.await.unwrap().unwrap().code, "abc");
    }

    #[tokio::test]
    async fn missing_gmail_scope_page_tells_user_to_retry() {
        let (l, port) = bound().await;
        let cancel = Notify::new();
        let (r, page) = tokio::join!(
            wait_for_redirect(&l, "ST", Duration::from_secs(10), &cancel),
            get(port, "/?code=abc&state=ST&scope=openid+email".into())
        );
        assert!(r.is_ok());
        assert!(page.contains("not granted") && page.contains("tick"));
    }

    #[tokio::test]
    async fn access_denied_and_state_mismatch() {
        for (q, want) in [("error=access_denied&state=ST", "cancelled"), ("code=abc&state=EVIL", "state mismatch")] {
            let (l, port) = bound().await;
            let cancel = Notify::new();
            let (r, page) = tokio::join!(
                wait_for_redirect(&l, "ST", Duration::from_secs(10), &cancel),
                get(port, format!("/?{q}"))
            );
            assert!(r.is_err());
            assert!(page.contains(want), "{page}");
        }
    }

    #[tokio::test]
    async fn times_out_when_tab_is_closed() {
        let (l, _) = bound().await;
        let cancel = Notify::new();
        let r = wait_for_redirect(&l, "ST", Duration::from_millis(150), &cancel).await;
        assert!(matches!(r, Err(Error::Timeout(0))));
    }

    #[tokio::test]
    async fn cancel_wins_even_if_signalled_early() {
        let (l, _) = bound().await;
        let cancel = Notify::new();
        cancel.notify_one(); // stored permit: cancel pressed before the wait began
        let r = wait_for_redirect(&l, "ST", Duration::from_secs(30), &cancel).await;
        assert!(matches!(r, Err(Error::Cancelled)));
    }
}
