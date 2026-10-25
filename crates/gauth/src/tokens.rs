//! Per-account access-token cache implementing `TokenSource`.

use std::sync::Arc;

use mail_model::{AccountId, AuthError, TokenSource};

use crate::client::Client;
use crate::store::{Store, StoredAccount};

/// Access tokens for one account. Cheap to clone (shared state). Tokens live in memory only, are cached until
/// `expires_in - 60 s`, and refreshes are serialized (one at a time per account). `invalid_grant` becomes
/// `AuthError::NeedsReauth` and the flag is persisted through the [`Store`]; transport failures are
/// `AuthError::Network` and change nothing.
#[derive(Clone)]
pub struct Tokens {
    inner: Arc<TokensInner>,
}

struct TokensInner {
    client: Client,
    store: Store,
    id: AccountId,
    email: String,
    refresh_token: String,
    cached: tokio::sync::Mutex<Option<(String, std::time::Instant)>>,
}

impl Tokens {
    /// `account` must come from `Store::list` / `complete_login`.
    pub fn new(client: Client, store: Store, account: &StoredAccount) -> Tokens {
        todo!()
    }

    pub fn account_id(&self) -> &AccountId {
        &self.inner.id
    }

    pub fn email(&self) -> &str {
        &self.inner.email
    }

    /// Drop the cached access token so the next call refreshes (after a Gmail 401).
    pub fn invalidate(&self) {
        todo!()
    }

    /// The refresh token, for sign-out's revoke.
    pub fn refresh_token(&self) -> &str {
        &self.inner.refresh_token
    }
}

impl TokenSource for Tokens {
    async fn access_token(&self, force_refresh: bool) -> Result<String, AuthError> {
        todo!()
    }
}
