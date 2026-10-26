//! The OAuth client (desktop-app client id + secret) and the endpoints it talks to.

use std::fmt;
use std::path::{Path, PathBuf};

use mail_model::AuthError;
use serde::Deserialize;

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
        f.debug_struct("Client")
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// `%LOCALAPPDATA%\com.ziggy.starmail` (falls back to the current directory if the variable is unset).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.ziggy.starmail")
}

/// `<data_dir>\oauth-client.json`.
pub fn default_client_path() -> PathBuf {
    data_dir().join("oauth-client.json")
}

impl Client {
    /// Read the client file: either Google's `client_secret*.json` download (`{"installed": {...}}`) or a flat
    /// `{"client_id", "client_secret"}`. Errors are `AuthError::Config` and never include file content.
    pub fn load(path: &Path) -> Result<Client, AuthError> {
        #[derive(Deserialize)]
        struct Creds {
            client_id: String,
            client_secret: String,
        }
        #[derive(Deserialize)]
        struct File {
            installed: Option<Creds>,
            client_id: Option<String>,
            client_secret: Option<String>,
        }
        let bytes = std::fs::read(path).map_err(|e| {
            AuthError::Config(format!(
                "cannot read the OAuth client file ({:?})",
                e.kind()
            ))
        })?;
        let f: File = serde_json::from_slice(&bytes).map_err(|_| {
            AuthError::Config(
                "the OAuth client file is not valid JSON of the expected shape".into(),
            )
        })?;
        let (client_id, client_secret) = match (f.installed, f.client_id, f.client_secret) {
            (Some(c), _, _) => (c.client_id, c.client_secret),
            (None, Some(id), Some(secret)) => (id, secret),
            _ => {
                return Err(AuthError::Config(
                    "the OAuth client file has no client_id / client_secret".into(),
                ));
            }
        };
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Err(AuthError::Config(
                "client_id or client_secret is empty".into(),
            ));
        }
        Ok(Client {
            client_id,
            client_secret,
            endpoints: Endpoints::default(),
        })
    }

    /// Same client against other endpoints (tests).
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Client {
        self.endpoints = endpoints;
        self
    }
}

/// Shared HTTP client for the OAuth endpoints (rustls, 30 s timeout).
pub(crate) fn http() -> reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("starmail/0.1")
            .build()
            .expect("reqwest client")
    })
    .clone()
}

/// Transport error text without the URL.
pub(crate) fn transport_text(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// Google's `error` field of a failed OAuth response (`"unknown"` if absent or not JSON).
pub(crate) async fn error_field(resp: reqwest::Response) -> String {
    #[derive(Deserialize)]
    struct E {
        error: Option<String>,
    }
    resp.json::<E>()
        .await
        .ok()
        .and_then(|e| e.error)
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gauth-client-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_installed_and_flat_and_hides_secret() {
        let d = tmp("load");
        let p = d.join("c.json");
        std::fs::write(
            &p,
            r#"{"installed":{"client_id":"id1","client_secret":"SYNTHETIC-SECRET"}}"#,
        )
        .unwrap();
        let c = Client::load(&p).unwrap();
        assert_eq!(c.client_id, "id1");
        assert!(!format!("{c:?}").contains("SYNTHETIC-SECRET"));
        std::fs::write(&p, r#"{"client_id":"id2","client_secret":"s"}"#).unwrap();
        assert_eq!(Client::load(&p).unwrap().client_id, "id2");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_files_are_config_errors_without_content() {
        let d = tmp("bad");
        let p = d.join("c.json");
        for body in [
            r#"{"web":{}}"#,
            "not json SYNTHETIC-SECRET",
            r#"{"installed":{"client_id":"","client_secret":"x"}}"#,
        ] {
            std::fs::write(&p, body).unwrap();
            match Client::load(&p) {
                Err(AuthError::Config(m)) => assert!(!m.contains("SYNTHETIC-SECRET")),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(matches!(
            Client::load(&d.join("missing.json")),
            Err(AuthError::Config(_))
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn default_paths() {
        let p = default_client_path();
        assert_eq!(p.file_name().unwrap(), "oauth-client.json");
        assert_eq!(p.parent().unwrap(), data_dir());
    }
}
