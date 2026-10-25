//! Error type. Messages never contain tokens, codes or the client secret.

/// Everything that can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `oauth-client.json` missing or malformed.
    #[error("OAuth client file problem: {0}")]
    ClientFile(String),
    /// Filesystem error.
    #[error("I/O error: {0}")]
    Io(String),
    /// DPAPI failure.
    #[error("DPAPI error: {0}")]
    Dpapi(String),
    /// JSON failure.
    #[error("JSON error: {0}")]
    Json(String),
    /// Network / TLS / timeout (URL stripped).
    #[error("network error: {0}")]
    Transport(String),
    /// Could not open the system browser.
    #[error("could not open the system browser: {0}")]
    BrowserOpen(String),
    /// User pressed Cancel / Deny on the consent screen.
    #[error("sign-in was cancelled on the Google page (access_denied)")]
    AccessDenied,
    /// Redirect carried another `error` value.
    #[error("authorization failed: {0}")]
    AuthRedirect(String),
    /// `state` missing or wrong.
    #[error("OAuth state mismatch; sign-in rejected")]
    StateMismatch,
    /// No redirect before the timeout (closed tab, walked away).
    #[error("no answer from the browser within {0} seconds; the sign-in was abandoned")]
    Timeout(u64),
    /// The user cancelled from inside the app.
    #[error("sign-in cancelled")]
    Cancelled,
    /// Granular consent: Gmail box not ticked.
    #[error("The Gmail permission was not granted. On Google's page tick \"Read your email\" (View your email messages and settings), then try again.")]
    MissingScope {
        /// Scopes Google reported (space separated).
        granted: String,
        /// Result of the optional diagnostic read with the scope-reduced token.
        probe: Option<String>,
    },
    /// No refresh token in the response.
    #[error("Google returned no refresh token")]
    NoRefreshToken,
    /// id_token problems.
    #[error("invalid id_token: {0}")]
    InvalidIdToken(String),
    /// `invalid_grant` on refresh: revoked, expired (Testing: 7 days), or password changed.
    #[error("access for {0} was removed or expired (invalid_grant); sign in again")]
    Revoked(String),
    /// Token / revoke / tokeninfo endpoint error (status + `error` only).
    #[error("Google endpoint error: HTTP {status}, error={error}")]
    OAuth {
        /// HTTP status.
        status: u16,
        /// Google's `error` field.
        error: String,
    },
    /// Gmail API error (status + error.status only).
    #[error("Gmail API error: HTTP {status}, error={error}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Google's `error.status` string.
        error: String,
    },
    /// Unknown account.
    #[error("no signed-in account {0}")]
    UnknownAccount(String),
}

impl Error {
    /// Short machine-readable kind for the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::MissingScope { .. } => "missing_scope",
            Error::AccessDenied => "access_denied",
            Error::Timeout(_) => "timeout",
            Error::Cancelled => "cancelled",
            Error::Revoked(_) => "revoked",
            Error::Transport(_) => "offline",
            Error::StateMismatch => "state_mismatch",
            _ => "error",
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e.without_url().to_string())
    }
}
