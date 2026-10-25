//! Sign-out helpers: revoke and tokeninfo. These two are the only non-GET calls in the app besides the Gmail batch.

use std::time::Duration;

use mail_model::AuthError;
use serde::Deserialize;

use crate::client::{Endpoints, error_field, http, transport_text};

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
    revoke_at(&Endpoints::default().revoke, refresh_token).await
}

/// [`revoke`] against another URL (tests).
pub async fn revoke_at(url: &str, refresh_token: &str) -> RevokeOutcome {
    let req = http()
        .post(url)
        .timeout(Duration::from_secs(10))
        .form(&[("token", refresh_token)]);
    match req.send().await {
        Err(e) => RevokeOutcome::Failed(format!("network: {}", transport_text(e))),
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 200 {
                return RevokeOutcome::Revoked;
            }
            let err = error_field(resp).await;
            if status == 400 && err == "invalid_token" {
                RevokeOutcome::AlreadyInvalid
            } else {
                RevokeOutcome::Failed(format!("HTTP {status}: {err}"))
            }
        }
    }
}

/// Scopes the access token actually carries (`POST https://oauth2.googleapis.com/tokeninfo`, form body).
pub async fn tokeninfo_scopes(access_token: &str) -> Result<Vec<String>, AuthError> {
    tokeninfo_scopes_at(&Endpoints::default().tokeninfo, access_token).await
}

/// [`tokeninfo_scopes`] against another URL (tests).
pub async fn tokeninfo_scopes_at(url: &str, access_token: &str) -> Result<Vec<String>, AuthError> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(default)]
        scope: String,
    }
    let resp = http()
        .post(url)
        .form(&[("access_token", access_token)])
        .send()
        .await
        .map_err(|e| AuthError::Network(transport_text(e)))?;
    let status = resp.status();
    if !status.is_success() {
        let err = error_field(resp).await;
        return Err(AuthError::Protocol(format!(
            "tokeninfo HTTP {}: {err}",
            status.as_u16()
        )));
    }
    let info: Info = resp
        .json()
        .await
        .map_err(|_| AuthError::Protocol("unexpected tokeninfo response".into()))?;
    Ok(info.scope.split_whitespace().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn revoke_200_is_revoked_and_token_goes_in_form_body() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/revoke")
                    .form_urlencoded_tuple("token", "synthetic-rt");
                then.status(200);
            })
            .await;
        assert_eq!(
            revoke_at(&server.url("/revoke"), "synthetic-rt").await,
            RevokeOutcome::Revoked
        );
        m.assert_async().await;
    }

    #[tokio::test]
    async fn revoke_400_invalid_token_is_already_invalid() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/revoke");
                then.status(400)
                    .json_body(serde_json::json!({"error": "invalid_token"}));
            })
            .await;
        assert_eq!(
            revoke_at(&server.url("/revoke"), "x").await,
            RevokeOutcome::AlreadyInvalid
        );
    }

    #[tokio::test]
    async fn revoke_other_errors_and_offline_are_failed_without_the_token() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/revoke");
                then.status(500);
            })
            .await;
        match revoke_at(&server.url("/revoke"), "SECRET-RT").await {
            RevokeOutcome::Failed(m) => assert!(m.contains("500") && !m.contains("SECRET-RT")),
            other => panic!("unexpected {other:?}"),
        }
        match revoke_at("http://127.0.0.1:9/revoke", "SECRET-RT").await {
            RevokeOutcome::Failed(m) => assert!(!m.contains("SECRET-RT")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn tokeninfo_posts_form_body_and_splits_scopes() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/tokeninfo")
                    .form_urlencoded_tuple("access_token", "synthetic-at");
                then.status(200).json_body(serde_json::json!({
                    "scope": "openid https://www.googleapis.com/auth/gmail.readonly",
                    "expires_in": "3000"
                }));
            })
            .await;
        let scopes = tokeninfo_scopes_at(&server.url("/tokeninfo"), "synthetic-at")
            .await
            .unwrap();
        assert_eq!(
            scopes,
            vec!["openid", "https://www.googleapis.com/auth/gmail.readonly"]
        );
        m.assert_async().await;
    }

    #[tokio::test]
    async fn tokeninfo_errors_map_to_protocol_and_network() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/tokeninfo");
                then.status(400)
                    .json_body(serde_json::json!({"error": "invalid_token"}));
            })
            .await;
        assert!(matches!(
            tokeninfo_scopes_at(&server.url("/tokeninfo"), "x").await,
            Err(AuthError::Protocol(_))
        ));
        assert!(matches!(
            tokeninfo_scopes_at("http://127.0.0.1:9/t", "x").await,
            Err(AuthError::Network(_))
        ));
    }
}
