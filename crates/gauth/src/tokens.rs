//! Per-account access-token cache implementing `TokenSource`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mail_model::{AccountId, AuthError, TokenSource};
use serde::Deserialize;

use crate::client::{Client, error_field, http, transport_text};
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
    cached: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Tokens {
    /// `account` must come from `Store::list` / `complete_login`.
    pub fn new(client: Client, store: Store, account: &StoredAccount) -> Tokens {
        Tokens {
            inner: Arc::new(TokensInner {
                client,
                store,
                id: account.id(),
                email: account.email.clone(),
                refresh_token: account.refresh_token.clone(),
                cached: tokio::sync::Mutex::new(None),
            }),
        }
    }

    pub fn account_id(&self) -> &AccountId {
        &self.inner.id
    }

    pub fn email(&self) -> &str {
        &self.inner.email
    }

    /// Drop the cached access token so the next call refreshes (after a Gmail 401).
    pub fn invalidate(&self) {
        // If a refresh holds the lock right now, it is about to store a token newer than the rejected one.
        if let Ok(mut g) = self.inner.cached.try_lock() {
            *g = None;
        }
    }

    /// The refresh token, for sign-out's revoke.
    pub fn refresh_token(&self) -> &str {
        &self.inner.refresh_token
    }

    async fn refresh(&self) -> Result<(String, Instant), AuthError> {
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            #[serde(default)]
            expires_in: u64,
        }
        let i = &self.inner;
        let resp = http()
            .post(&i.client.endpoints.token)
            .form(&[
                ("client_id", i.client.client_id.as_str()),
                ("client_secret", i.client.client_secret.as_str()),
                ("refresh_token", i.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| AuthError::Network(transport_text(e)))?;
        let status = resp.status();
        if status.is_success() {
            let r: Resp = resp
                .json()
                .await
                .map_err(|_| AuthError::Protocol("unexpected token response shape".into()))?;
            let ttl = Duration::from_secs(r.expires_in.saturating_sub(60));
            return Ok((r.access_token, Instant::now() + ttl));
        }
        let code = status.as_u16();
        let err = error_field(resp).await;
        if err == "invalid_grant" {
            if let Err(e) = i.store.set_needs_reauth(&i.id, true) {
                tracing::warn!(error = %e, "could not persist needs_reauth");
            }
            return Err(AuthError::NeedsReauth {
                reason: "invalid_grant".into(),
            });
        }
        if code >= 500 {
            return Err(AuthError::Network(format!("token endpoint HTTP {code}")));
        }
        Err(AuthError::Protocol(format!(
            "token endpoint HTTP {code}: {err}"
        )))
    }
}

impl TokenSource for Tokens {
    async fn access_token(&self, force_refresh: bool) -> Result<String, AuthError> {
        // The lock is held across the refresh: callers for one account queue behind a single refresh.
        let mut cached = self.inner.cached.lock().await;
        if force_refresh {
            *cached = None;
        }
        if let Some((token, expiry)) = cached.as_ref()
            && Instant::now() < *expiry
        {
            return Ok(token.clone());
        }
        *cached = None;
        let fresh = self.refresh().await?;
        let token = fresh.0.clone();
        *cached = Some(fresh);
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Endpoints;
    use crate::store::tests::{sample, tmp};
    use httpmock::prelude::*;

    fn setup(name: &str, server: &MockServer) -> (Tokens, Store, std::path::PathBuf) {
        let d = tmp(name);
        let store = Store::open(&d).unwrap();
        let account = sample("111", "a@example.com");
        store.save(&account).unwrap();
        let client = Client {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            endpoints: Endpoints {
                token: server.url("/token"),
                ..Endpoints::default()
            },
        };
        (Tokens::new(client, store.clone(), &account), store, d)
    }

    fn ok_body(token: &str, expires_in: u64) -> serde_json::Value {
        serde_json::json!({"access_token": token, "expires_in": expires_in, "token_type": "Bearer"})
    }

    #[tokio::test]
    async fn caches_until_expiry_then_invalidate_forces_refresh() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/token")
                    .form_urlencoded_tuple("grant_type", "refresh_token")
                    .form_urlencoded_tuple("refresh_token", "synthetic-refresh")
                    .form_urlencoded_tuple("client_id", "cid");
                then.status(200).json_body(ok_body("at-1", 3600));
            })
            .await;
        let (t, _store, d) = setup("tok-cache", &server);
        assert_eq!(t.access_token(false).await.unwrap(), "at-1");
        assert_eq!(t.access_token(false).await.unwrap(), "at-1");
        m.assert_calls_async(1).await;
        t.invalidate();
        t.access_token(false).await.unwrap();
        m.assert_calls_async(2).await;
        t.access_token(true).await.unwrap();
        m.assert_calls_async(3).await;
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn short_lived_tokens_are_not_cached() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(200).json_body(ok_body("at", 50)); // 50 - 60 s margin => already stale
            })
            .await;
        let (t, _store, d) = setup("tok-short", &server);
        t.access_token(false).await.unwrap();
        t.access_token(false).await.unwrap();
        m.assert_calls_async(2).await;
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(200)
                    .delay(Duration::from_millis(200))
                    .json_body(ok_body("at", 3600));
            })
            .await;
        let (t, _store, d) = setup("tok-conc", &server);
        let t2 = t.clone();
        let (a, b, c) = tokio::join!(
            t.access_token(false),
            t2.access_token(false),
            t.access_token(false)
        );
        assert_eq!(
            (a.unwrap(), b.unwrap(), c.unwrap()),
            ("at".to_string(), "at".to_string(), "at".to_string())
        );
        m.assert_calls_async(1).await;
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn invalid_grant_is_needs_reauth_and_persisted() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(400)
                    .json_body(serde_json::json!({"error": "invalid_grant"}));
            })
            .await;
        let (t, store, d) = setup("tok-grant", &server);
        assert!(matches!(
            t.access_token(false).await,
            Err(AuthError::NeedsReauth { .. })
        ));
        assert!(store.list().0[0].needs_reauth);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn offline_refresh_is_network_and_changes_nothing() {
        let server = MockServer::start_async().await;
        let (_t, store, d) = setup("tok-offline", &server);
        // Rebuild with an unreachable token endpoint.
        let account = store.list().0.remove(0);
        let client = Client {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            endpoints: Endpoints {
                token: "http://127.0.0.1:9/token".into(),
                ..Endpoints::default()
            },
        };
        let t = Tokens::new(client, store.clone(), &account);
        assert!(matches!(
            t.access_token(false).await,
            Err(AuthError::Network(_))
        ));
        assert!(!store.list().0[0].needs_reauth);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn server_error_is_network_other_errors_protocol() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(503);
            })
            .await;
        let (t, store, d) = setup("tok-5xx", &server);
        assert!(matches!(
            t.access_token(false).await,
            Err(AuthError::Network(_))
        ));
        m.delete_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/token");
                then.status(401)
                    .json_body(serde_json::json!({"error": "invalid_client"}));
            })
            .await;
        assert!(matches!(
            t.access_token(false).await,
            Err(AuthError::Protocol(_))
        ));
        assert!(!store.list().0[0].needs_reauth);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn accessors() {
        let server = MockServer::start_async().await;
        let (t, _s, d) = setup("tok-acc", &server);
        assert_eq!(t.account_id(), &AccountId::new("111"));
        assert_eq!(t.email(), "a@example.com");
        assert_eq!(t.refresh_token(), "synthetic-refresh");
        let _ = std::fs::remove_dir_all(&d);
    }
}
