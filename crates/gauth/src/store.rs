//! The DPAPI token store: one `<sub>.dpapi` file per account.

use std::fmt;
use std::path::{Path, PathBuf};

use mail_model::{AccountId, AuthError};
use serde::{Deserialize, Serialize};

/// What is persisted per account (DPAPI-encrypted JSON, entropy [`crate::TOKEN_ENTROPY`]).
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
    /// Google's stable `sub` claim; also the file name stem and the [`AccountId`].
    pub sub: String,
    pub refresh_token: String,
    /// Scopes Google reported as granted.
    pub scopes: Vec<String>,
    /// Seconds since the epoch.
    pub created_at: u64,
    /// Set when the refresh token died (`invalid_grant`) or Gmail refused the scope; cleared by a new sign-in.
    #[serde(default)]
    pub needs_reauth: bool,
}

impl StoredAccount {
    /// The account id (the `sub`).
    pub fn id(&self) -> AccountId {
        AccountId(self.sub.clone())
    }
}

impl fmt::Debug for StoredAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAccount")
            .field("sub", &self.sub)
            .field("refresh_token", &"<redacted>")
            .field("scopes", &self.scopes)
            .field("needs_reauth", &self.needs_reauth)
            .finish()
    }
}

/// Directory of `<sub>.dpapi` files (`%LOCALAPPDATA%\com.ziggy.gmail-ro\accounts`). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Use `dir` (created if missing).
    pub fn open(dir: &Path) -> Result<Store, AuthError> {
        todo!()
    }

    /// All readable accounts, and how many files were skipped (undecryptable or malformed: skipped, never fatal).
    pub fn list(&self) -> (Vec<StoredAccount>, usize) {
        todo!()
    }

    /// Write via tmp file + rename (atomic). Replaces an existing account with the same `sub`.
    pub fn save(&self, account: &StoredAccount) -> Result<(), AuthError> {
        todo!()
    }

    /// Persist the `needs_reauth` flag of one account.
    pub fn set_needs_reauth(&self, id: &AccountId, needs_reauth: bool) -> Result<(), AuthError> {
        todo!()
    }

    /// Delete the token file. `Ok(true)` if a file was removed.
    pub fn delete(&self, id: &AccountId) -> Result<bool, AuthError> {
        todo!()
    }
}
