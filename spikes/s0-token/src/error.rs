//! Error type. Messages never contain tokens, codes or the client secret.

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `oauth-client.json` missing or malformed (message names the problem, never the content).
    #[error("OAuth client file problem: {0}")]
    ClientFile(String),
    /// Filesystem error.
    #[error("I/O error: {0}")]
    Io(String),
    /// DPAPI protect/unprotect failed.
    #[error("DPAPI error: {0}")]
    Dpapi(String),
    /// JSON (de)serialisation failed.
    #[error("JSON error: {0}")]
    Json(String),
    /// Network / TLS / timeout error (URL stripped).
    #[error("HTTP transport error: {0}")]
    Transport(String),
    /// Could not open the system browser (the URL was still printed to stderr).
    #[error("could not open the system browser: {0}")]
    BrowserOpen(String),
    /// The user clicked Cancel / denied access on the consent screen.
    #[error("sign-in was cancelled (access_denied)")]
    AccessDenied,
    /// Google redirected back with some other `error` value.
    #[error("authorization failed: {0}")]
    AuthRedirect(String),
    /// `state` missing or different from the one we sent (possible CSRF).
    #[error("OAuth state mismatch; sign-in rejected")]
    StateMismatch,
    /// No redirect arrived within 5 minutes.
    #[error("timed out waiting for the browser sign-in (5 minutes)")]
    Timeout,
    /// Granular consent: the user did not tick the Gmail read permission.
    #[error("the gmail.readonly permission was not granted (granted: {granted}); sign in again and tick the Gmail checkbox")]
    MissingScope {
        /// Space-separated scopes Google reported as granted.
        granted: String,
    },
    /// Token response had no refresh token.
    #[error("Google returned no refresh token")]
    NoRefreshToken,
    /// id_token missing or undecodable.
    #[error("invalid id_token: {0}")]
    InvalidIdToken(String),
    /// Refresh grant rejected with `invalid_grant`: access was revoked or the token expired
    /// (7 days in Testing mode). The user must sign in again.
    #[error("access for {0} was revoked or expired (invalid_grant); sign in again")]
    Revoked(String),
    /// Token endpoint returned an error (status + Google's `error` field only).
    #[error("token endpoint error: HTTP {status}, error={error}")]
    OAuth {
        /// HTTP status.
        status: u16,
        /// Google's `error` field.
        error: String,
    },
    /// Gmail API returned an error (status + Google's error status/reason only).
    #[error("Gmail API error: HTTP {status}, error={error}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Google's error status string (e.g. `PERMISSION_DENIED`) if any.
        error: String,
    },
    /// No stored account with this email.
    #[error("no signed-in account for {0}")]
    UnknownAccount(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        // serde_json messages describe position/type, not content values.
        Error::Json(e.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e.without_url().to_string())
    }
}
