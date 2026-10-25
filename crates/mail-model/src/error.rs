//! Error types shared across crates. Messages never contain tokens, addresses, subjects or bodies.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sign-in / token failures (PLAN §1 Auth errors).
///
/// `Config`, `Network`, `Store` and `Protocol` go beyond the list in PLAN §3.1: `TokenSource` can only return
/// `AuthError`, so a refresh that fails offline (`Network`) must be expressible without becoming `NeedsReauth`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No stored token for this account.
    #[error("not signed in")]
    NotSignedIn,
    /// The refresh token is dead (`invalid_grant`) or the API said `PERMISSION_DENIED`: the user must sign in again.
    #[error("sign-in expired: {reason}")]
    NeedsReauth { reason: String },
    /// Google did not grant `gmail.readonly` (the box was left unticked). Nothing is stored.
    #[error("Gmail access was not granted")]
    MissingScope { granted: Vec<String> },
    /// The user cancelled the pending sign-in.
    #[error("sign-in cancelled")]
    Cancelled,
    /// The pending sign-in timed out (5 minutes).
    #[error("sign-in timed out")]
    Timeout,
    /// The user pressed "Cancel/Deny" on Google's page (`error=access_denied`).
    #[error("access denied on the consent page")]
    Denied,
    /// Another sign-in is already pending (one per app).
    #[error("a sign-in is already in progress")]
    AlreadyPending,
    /// The redirect's `state` did not match (possible forged redirect).
    #[error("sign-in state mismatch")]
    StateMismatch,
    /// The system browser could not be opened.
    #[error("could not open the browser: {0}")]
    Browser(String),
    /// The OAuth client file is missing or invalid.
    #[error("OAuth client configuration: {0}")]
    Config(String),
    /// Transport failure talking to Google's OAuth endpoints (offline). Does not change the account state.
    #[error("network: {0}")]
    Network(String),
    /// Local token-store I/O or decryption failure.
    #[error("token store: {0}")]
    Store(String),
    /// Unexpected response from an OAuth endpoint.
    #[error("unexpected OAuth response: {0}")]
    Protocol(String),
}

/// Any failure talking to the Gmail API.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApiError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// 403 `rateLimitExceeded` / `userRateLimitExceeded` or 429.
    #[error("rate limited")]
    RateLimited { retry_after_s: Option<u64> },
    /// `history.list` 404: the cursor is too old; resync.
    #[error("history expired")]
    HistoryExpired,
    #[error("not found")]
    NotFound,
    /// Transport failure (no network, DNS, TLS, timeout).
    #[error("offline: {0}")]
    Offline(String),
    /// Any other HTTP status (`reason` is Google's short error reason, never message content).
    #[error("HTTP {code}: {reason}")]
    Status { code: u16, reason: String },
    /// A response that could not be decoded.
    #[error("decode: {0}")]
    Decode(String),
}

/// Local cache failure. Strings only, so this crate needs no rusqlite.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheError {
    #[error("database: {0}")]
    Db(String),
    #[error("io: {0}")]
    Io(String),
    #[error("crypto: {0}")]
    Crypto(String),
    /// The account's cache is open elsewhere (e.g. deleting while a connection is open).
    #[error("cache is locked")]
    Locked,
}

/// MIME parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RenderError {
    #[error("parse: {0}")]
    Parse(String),
}

/// Coarse class of a command failure, serialized as `auth|offline|throttled|not_found|internal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmdErrorCode {
    Auth,
    Offline,
    Throttled,
    NotFound,
    Internal,
}

/// What a Tauri command returns on failure: `{code, message, detail?}`.
///
/// `detail` is a stable machine-readable sub-code beyond PLAN §3.1 (e.g. `missing_scope`, `cancelled`,
/// `timeout`, `denied`, `already_pending`, `needs_reauth`, `state_mismatch`, `history_expired`) so the UI can pick
/// the right message (the unticked-Gmail retry flow) without parsing `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{message}")]
pub struct CmdError {
    pub code: CmdErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl CmdError {
    pub fn new(code: CmdErrorCode, message: impl Into<String>) -> Self {
        CmdError { code, message: message.into(), detail: None }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(CmdErrorCode::Internal, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(CmdErrorCode::NotFound, message)
    }

    pub fn offline(message: impl Into<String>) -> Self {
        Self::new(CmdErrorCode::Offline, message)
    }
}

impl From<AuthError> for CmdError {
    fn from(e: AuthError) -> Self {
        let (code, detail) = match &e {
            AuthError::NotSignedIn => (CmdErrorCode::Auth, "not_signed_in"),
            AuthError::NeedsReauth { .. } => (CmdErrorCode::Auth, "needs_reauth"),
            AuthError::MissingScope { .. } => (CmdErrorCode::Auth, "missing_scope"),
            AuthError::Cancelled => (CmdErrorCode::Auth, "cancelled"),
            AuthError::Timeout => (CmdErrorCode::Auth, "timeout"),
            AuthError::Denied => (CmdErrorCode::Auth, "denied"),
            AuthError::AlreadyPending => (CmdErrorCode::Auth, "already_pending"),
            AuthError::StateMismatch => (CmdErrorCode::Auth, "state_mismatch"),
            AuthError::Browser(_) => (CmdErrorCode::Internal, "browser"),
            AuthError::Config(_) => (CmdErrorCode::Internal, "config"),
            AuthError::Network(_) => (CmdErrorCode::Offline, "network"),
            AuthError::Store(_) | AuthError::Protocol(_) => (CmdErrorCode::Internal, "internal"),
        };
        CmdError::new(code, e.to_string()).with_detail(detail)
    }
}

impl From<ApiError> for CmdError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Auth(a) => a.into(),
            ApiError::RateLimited { .. } => CmdError::new(CmdErrorCode::Throttled, e.to_string()),
            ApiError::NotFound => CmdError::not_found(e.to_string()),
            ApiError::Offline(_) => CmdError::offline(e.to_string()),
            ApiError::HistoryExpired => CmdError::internal(e.to_string()).with_detail("history_expired"),
            ApiError::Status { .. } | ApiError::Decode(_) => CmdError::internal(e.to_string()),
        }
    }
}

impl From<CacheError> for CmdError {
    fn from(e: CacheError) -> Self {
        CmdError::internal(e.to_string())
    }
}

impl From<RenderError> for CmdError {
    fn from(e: RenderError) -> Self {
        CmdError::internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_maps_to_codes() {
        let e: CmdError = AuthError::NeedsReauth { reason: "invalid_grant".into() }.into();
        assert_eq!(e.code, CmdErrorCode::Auth);
        assert_eq!(e.detail.as_deref(), Some("needs_reauth"));
        let e: CmdError = AuthError::MissingScope { granted: vec!["openid".into()] }.into();
        assert_eq!(e.detail.as_deref(), Some("missing_scope"));
        let e: CmdError = AuthError::Network("dns".into()).into();
        assert_eq!(e.code, CmdErrorCode::Offline);
    }

    #[test]
    fn api_maps_to_codes() {
        assert_eq!(CmdError::from(ApiError::RateLimited { retry_after_s: None }).code, CmdErrorCode::Throttled);
        assert_eq!(CmdError::from(ApiError::Offline("x".into())).code, CmdErrorCode::Offline);
        assert_eq!(CmdError::from(ApiError::NotFound).code, CmdErrorCode::NotFound);
        assert_eq!(CmdError::from(ApiError::Status { code: 500, reason: "backendError".into() }).code, CmdErrorCode::Internal);
        let e: CmdError = ApiError::Auth(AuthError::NotSignedIn).into();
        assert_eq!(e.code, CmdErrorCode::Auth);
        assert_eq!(e.message, "not signed in");
    }

    #[test]
    fn cache_and_render_are_internal() {
        assert_eq!(CmdError::from(CacheError::Locked).code, CmdErrorCode::Internal);
        assert_eq!(CmdError::from(RenderError::Parse("x".into())).code, CmdErrorCode::Internal);
    }

    #[test]
    fn json_shape() {
        let e = CmdError::not_found("gone");
        assert_eq!(serde_json::to_value(&e).unwrap(), serde_json::json!({"code": "not_found", "message": "gone"}));
        let e = CmdError::from(AuthError::Cancelled);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "auth");
        assert_eq!(v["detail"], "cancelled");
    }
}
