//! Sign-out helpers: revoke and tokeninfo. These two are the only non-GET calls in the app besides the Gmail batch.

use mail_model::AuthError;

/// What Google said to a revoke. A revoke kills the whole grant for the account (every token of this client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// HTTP 200.
    Revoked,
    /// 400 `invalid_token`: it was already gone.
    AlreadyInvalid,
    /// Offline or any other failure (message: status / Google's `error` field / transport text; no token).
    /// The caller still deletes everything locally and tells the user to remove access at Google.
    Failed(String),
}

/// `POST https://oauth2.googleapis.com/revoke` with the token in a form body.
pub async fn revoke(refresh_token: &str) -> RevokeOutcome {
    todo!()
}

/// [`revoke`] against another URL (tests).
pub async fn revoke_at(url: &str, refresh_token: &str) -> RevokeOutcome {
    todo!()
}

/// Scopes the access token actually carries (`POST https://oauth2.googleapis.com/tokeninfo`, form body).
pub async fn tokeninfo_scopes(access_token: &str) -> Result<Vec<String>, AuthError> {
    todo!()
}

/// [`tokeninfo_scopes`] against another URL (tests).
pub async fn tokeninfo_scopes_at(url: &str, access_token: &str) -> Result<Vec<String>, AuthError> {
    todo!()
}
