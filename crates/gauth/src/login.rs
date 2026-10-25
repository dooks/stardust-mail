//! Loopback + PKCE sign-in in the system browser. One pending sign-in per app (global slot).

use std::sync::Arc;
use std::time::Duration;

use mail_model::AuthError;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::client::Client;
use crate::store::StoredAccount;

/// A prepared sign-in: loopback listener bound on `127.0.0.1:<ephemeral>`, consent URL built, and the global
/// "one pending login" slot held until this value is dropped or completed. The browser has NOT been opened yet.
pub struct PendingLogin {
    url: String,
    redirect_uri: String,
    hint: Option<String>,
    listener: TcpListener,
    state: String,
    verifier: String,
    client: Client,
    cancel: Arc<Notify>,
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
        todo!()
    }

    /// Cancel: the task awaiting [`complete_login`] returns `AuthError::Cancelled`. The permit is stored, so
    /// cancelling just before `complete_login` starts waiting still works.
    pub fn cancel(&self) {
        todo!()
    }

    /// A cloneable handle for reopen / cancel from other tasks.
    pub fn handle(&self) -> LoginHandle {
        LoginHandle { url: self.url.clone(), cancel: self.cancel.clone() }
    }
}

impl LoginHandle {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Same as [`PendingLogin::reopen_browser`].
    pub fn reopen_browser(&self) -> Result<(), AuthError> {
        todo!()
    }

    /// Same as [`PendingLogin::cancel`].
    pub fn cancel(&self) {
        todo!()
    }
}

/// Bind the loopback listener, build the consent URL (`access_type=offline`, `prompt=consent select_account`,
/// PKCE S256, `state`, optional `login_hint`, scopes = `gmail.readonly openid email`) and take the global slot.
/// `AuthError::AlreadyPending` if another login is pending. Does not open the browser (the UI shows the
/// "tick Read all your email" instruction first, then calls `reopen_browser`).
pub async fn begin_login(client: &Client, hint: Option<&str>) -> Result<PendingLogin, AuthError> {
    todo!()
}

/// Wait for the redirect (404 for stray requests), verify `state`, exchange the code, decode the id token, check
/// the granted scope contains `gmail.readonly` (otherwise `MissingScope{granted}` and NOTHING is stored), and
/// return the account. The caller saves it with [`crate::Store::save`] (so a re-sign-in replaces the old file and
/// clears `needs_reauth`). Errors: `Cancelled`, `Timeout`, `Denied`, `StateMismatch`, `Network`, `Protocol`.
pub async fn complete_login(pending: PendingLogin, timeout: Duration) -> Result<StoredAccount, AuthError> {
    todo!()
}
