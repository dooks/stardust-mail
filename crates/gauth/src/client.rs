//! The OAuth client (desktop-app client id + secret) and the endpoints it talks to.

use std::fmt;
use std::path::{Path, PathBuf};

use mail_model::AuthError;

/// OAuth endpoint URLs. `Default` is Google's; tests point them at an `httpmock` server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// `https://accounts.google.com/o/oauth2/v2/auth`
    pub auth: String,
    /// `https://oauth2.googleapis.com/token`
    pub token: String,
    /// `https://oauth2.googleapis.com/revoke`
    pub revoke: String,
    /// `https://oauth2.googleapis.com/tokeninfo`
    pub tokeninfo: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            revoke: "https://oauth2.googleapis.com/revoke".into(),
            tokeninfo: "https://oauth2.googleapis.com/tokeninfo".into(),
        }
    }
}

/// The Google Cloud "Desktop app" OAuth client. Never printed: `Debug` redacts the secret.
#[derive(Clone)]
pub struct Client {
    pub client_id: String,
    pub client_secret: String,
    pub endpoints: Endpoints,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("client_id", &"<redacted>").field("client_secret", &"<redacted>").finish()
    }
}

/// `%LOCALAPPDATA%\com.ziggy.gmail-ro` (falls back to the current directory if the variable is unset).
pub fn data_dir() -> PathBuf {
    todo!()
}

/// `<data_dir>\oauth-client.json`.
pub fn default_client_path() -> PathBuf {
    todo!()
}

impl Client {
    /// Read the client file: either Google's `client_secret*.json` download (`{"installed": {...}}`) or a flat
    /// `{"client_id", "client_secret"}`. Errors are `AuthError::Config` and never include file content.
    pub fn load(path: &Path) -> Result<Client, AuthError> {
        todo!()
    }

    /// Same client against other endpoints (tests).
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Client {
        self.endpoints = endpoints;
        self
    }
}
