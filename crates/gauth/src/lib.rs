//! Google OAuth for gmail-ro (PLAN §3.2, WP-A): loopback + PKCE sign-in in the system browser, a DPAPI token
//! store, a per-account access-token cache, revoke and tokeninfo.
//!
//! Wave 0 stubs: every function body is `todo!()`; signatures are final.
//!
//! Privacy: never log tokens, authorization codes, `state`, verifiers or Authorization headers.

#![allow(unused_variables, dead_code)] // Wave 0 stubs: remove when the file is implemented.

mod client;
mod login;
mod revoke;
mod store;
mod tokens;

pub use client::{Client, Endpoints, data_dir, default_client_path};
pub use login::{LoginHandle, PendingLogin, begin_login, complete_login};
pub use revoke::{RevokeOutcome, revoke, revoke_at, tokeninfo_scopes, tokeninfo_scopes_at};
pub use store::{Store, StoredAccount};
pub use tokens::Tokens;

/// The only Gmail scope ever requested.
pub const SCOPE_GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
/// Identity scopes requested alongside it (to learn the account's `sub` and e-mail).
pub const SCOPES_IDENTITY: [&str; 2] = ["openid", "email"];
/// How long a pending sign-in waits for the redirect (5 minutes).
pub const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// DPAPI entropy of token files.
pub const TOKEN_ENTROPY: &[u8] = b"gmail-ro-token";
