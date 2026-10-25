//! Pure OAuth helpers (no I/O): PKCE, auth URL, redirect parsing, scope check, JWT payload.

use crate::error::Error;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

pub(crate) const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const SCOPE_GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub(crate) const REQUESTED_SCOPES: &str = "openid email https://www.googleapis.com/auth/gmail.readonly";

/// `len` random bytes from the OS CSPRNG, base64url (no padding).
pub(crate) fn random_b64(len: usize) -> Result<String, Error> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| Error::Io(format!("OS random failed: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// PKCE code_challenge (S256) for a verifier: `base64url(sha256(verifier))`.
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// New PKCE verifier: 48 random bytes -> 64 base64url chars (RFC 7636 allows 43..=128).
pub(crate) fn pkce_verifier() -> Result<String, Error> {
    random_b64(48)
}

pub(crate) struct AuthUrlParams<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub state: &'a str,
    pub login_hint: Option<&'a str>,
    pub prompt: &'a str,
}

/// Build the consent URL. Encoding is done by `url`'s query serializer.
pub(crate) fn auth_url(p: &AuthUrlParams<'_>) -> String {
    let mut url = reqwest::Url::parse(AUTH_ENDPOINT).expect("static URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", p.client_id)
            .append_pair("redirect_uri", p.redirect_uri)
            .append_pair("scope", REQUESTED_SCOPES)
            .append_pair("code_challenge", p.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", p.state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", p.prompt);
        if let Some(h) = p.login_hint {
            q.append_pair("login_hint", h);
        }
    }
    url.into()
}

/// Split the first line of an HTTP request into (path, query). Only GET is accepted.
pub(crate) fn parse_request_line(head: &str) -> Option<(String, String)> {
    let line = head.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "GET" {
        return None;
    }
    let target = it.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some((path.to_string(), query.to_string()))
}

/// What a good redirect carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Redirect {
    pub code: String,
    /// The `scope` query parameter Google adds to the redirect (may be absent).
    pub scope: Option<String>,
}

/// Interpret the redirect query. Checks `state` first, then `error`, then `code`.
pub(crate) fn interpret_query(query: &str, expected_state: &str) -> Result<Redirect, Error> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1/?{query}")).map_err(|_| Error::StateMismatch)?;
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
        return Err(Error::StateMismatch);
    }
    if let Some(e) = error {
        return Err(if e == "access_denied" { Error::AccessDenied } else { Error::AuthRedirect(e) });
    }
    let code = code.filter(|c| !c.is_empty()).ok_or_else(|| Error::AuthRedirect("missing code".into()))?;
    Ok(Redirect { code, scope })
}

/// True if a space-separated scope string contains `gmail.readonly`.
pub(crate) fn has_gmail_readonly(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == SCOPE_GMAIL_READONLY)
}

/// Split a scope string into a list.
pub(crate) fn split_scopes(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_string).collect()
}

#[derive(serde::Deserialize)]
pub(crate) struct IdClaims {
    pub sub: String,
    pub email: Option<String>,
}

/// Decode the payload of a JWT without verifying the signature (it came from Google's token endpoint over TLS).
pub(crate) fn decode_id_token(jwt: &str) -> Result<IdClaims, Error> {
    let payload = jwt.split('.').nth(1).ok_or_else(|| Error::InvalidIdToken("not a JWT".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| Error::InvalidIdToken("payload is not base64url".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidIdToken("payload is not the expected JSON".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_appendix_b() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(pkce_challenge(v), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_length_and_charset() {
        let v = pkce_verifier().unwrap();
        assert!((43..=128).contains(&v.len()));
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(v, pkce_verifier().unwrap());
    }

    #[test]
    fn auth_url_has_all_params_encoded() {
        let u = auth_url(&AuthUrlParams {
            client_id: "cid.apps.googleusercontent.com",
            redirect_uri: "http://127.0.0.1:5555",
            code_challenge: "CHAL",
            state: "ST",
            login_hint: Some("a+b@gmail.com"),
            prompt: "consent select_account",
        });
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        // Raw string checks for encoding.
        assert!(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555"));
        assert!(u.contains("login_hint=a%2Bb%40gmail.com"));
        assert!(!u.contains(' '));
        // Semantic checks.
        let parsed = reqwest::Url::parse(&u).unwrap();
        let get = |k: &str| parsed.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        assert_eq!(get("response_type").as_deref(), Some("code"));
        assert_eq!(get("client_id").as_deref(), Some("cid.apps.googleusercontent.com"));
        assert_eq!(get("redirect_uri").as_deref(), Some("http://127.0.0.1:5555"));
        assert_eq!(get("scope").as_deref(), Some("openid email https://www.googleapis.com/auth/gmail.readonly"));
        assert_eq!(get("code_challenge").as_deref(), Some("CHAL"));
        assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
        assert_eq!(get("state").as_deref(), Some("ST"));
        assert_eq!(get("access_type").as_deref(), Some("offline"));
        assert_eq!(get("prompt").as_deref(), Some("consent select_account"));
        assert_eq!(get("login_hint").as_deref(), Some("a+b@gmail.com"));
    }

    #[test]
    fn auth_url_without_hint() {
        let u = auth_url(&AuthUrlParams {
            client_id: "c",
            redirect_uri: "http://127.0.0.1:1",
            code_challenge: "x",
            state: "s",
            login_hint: None,
            prompt: "consent",
        });
        assert!(!u.contains("login_hint"));
    }

    #[test]
    fn request_line_parsing() {
        assert_eq!(
            parse_request_line("GET /?code=a&state=b HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("/".into(), "code=a&state=b".into()))
        );
        assert_eq!(parse_request_line("GET /favicon.ico HTTP/1.1\r\n"), Some(("/favicon.ico".into(), String::new())));
        assert_eq!(parse_request_line("POST / HTTP/1.1\r\n"), None);
        assert_eq!(parse_request_line(""), None);
    }

    #[test]
    fn query_code_and_state() {
        let r = interpret_query("state=S1&code=4%2Fabc&scope=x+y", "S1").unwrap();
        assert_eq!((r.code.as_str(), r.scope.as_deref()), ("4/abc", Some("x y")));
    }

    #[test]
    fn query_access_denied() {
        assert!(matches!(interpret_query("error=access_denied&state=S1", "S1"), Err(Error::AccessDenied)));
    }

    #[test]
    fn query_other_error() {
        match interpret_query("error=server_error&state=S1", "S1") {
            Err(Error::AuthRedirect(e)) => assert_eq!(e, "server_error"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn query_missing_state() {
        assert!(matches!(interpret_query("code=abc", "S1"), Err(Error::StateMismatch)));
    }

    #[test]
    fn query_state_mismatch() {
        assert!(matches!(interpret_query("code=abc&state=EVIL", "S1"), Err(Error::StateMismatch)));
    }

    #[test]
    fn query_missing_code() {
        assert!(matches!(interpret_query("state=S1", "S1"), Err(Error::AuthRedirect(_))));
    }

    #[test]
    fn scope_check() {
        assert!(has_gmail_readonly(
            "openid https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/gmail.readonly"
        ));
        assert!(!has_gmail_readonly("openid email https://www.googleapis.com/auth/gmail.readonly.extra"));
        assert!(!has_gmail_readonly("openid email"));
        assert_eq!(split_scopes("a  b"), vec!["a", "b"]);
    }

    #[test]
    fn id_token_decode() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"iss":"x","sub":"1234567890","email":"test@example.com"}"#);
        let jwt = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let c = decode_id_token(&jwt).unwrap();
        assert_eq!(c.sub, "1234567890");
        assert_eq!(c.email.as_deref(), Some("test@example.com"));
        assert!(decode_id_token("nope").is_err());
        assert!(decode_id_token("a.!!!.c").is_err());
    }
}
