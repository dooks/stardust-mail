//! GET-only Gmail calls with outcome classification. Never logs tokens or bodies.

use std::time::Instant;

use serde_json::Value;

pub const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Ok,
    Unauthorized,
    /// 429, or 403 with a rate-limit reason. Carries `Retry-After` seconds if present.
    RateLimited(Option<u64>),
    /// 404 (expired/invalid history id, or message gone).
    Gone,
    /// Transport failure: no network, DNS, TLS, timeout.
    Offline,
    Server,
    Other,
}

#[derive(Debug)]
pub struct Reply {
    pub kind: Kind,
    pub status: u16,
    pub reason: String,
    pub bytes: usize,
    pub ms: u128,
    pub body: Value,
}

pub fn classify(status: u16, reason: &str, retry_after: Option<u64>) -> Kind {
    match status {
        200..=299 => Kind::Ok,
        401 => Kind::Unauthorized,
        429 => Kind::RateLimited(retry_after),
        403 if reason.contains("ateLimit") || reason.contains("quota") => Kind::RateLimited(retry_after),
        404 => Kind::Gone,
        500..=599 => Kind::Server,
        _ => Kind::Other,
    }
}

/// One GET. `path` is relative to `base` (leading slash). Repeated query keys are allowed.
pub async fn get_with(base: &str, token: &str, path: &str, query: &[(&str, &str)]) -> Reply {
    let t0 = Instant::now();
    let res = gtoken::http().get(format!("{base}{path}")).bearer_auth(token).query(query).send().await;
    let resp = match res {
        Ok(r) => r,
        Err(_) => {
            return Reply { kind: Kind::Offline, status: 0, reason: "transport".into(), bytes: 0, ms: t0.elapsed().as_millis(), body: Value::Null };
        }
    };
    let status = resp.status().as_u16();
    let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok());
    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return Reply { kind: Kind::Offline, status, reason: "body".into(), bytes: 0, ms: t0.elapsed().as_millis(), body: Value::Null };
        }
    };
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let reason = body["error"]["errors"][0]["reason"].as_str().or(body["error"]["status"].as_str()).unwrap_or("").to_string();
    Reply { kind: classify(status, &reason, retry_after), status, reason, bytes: raw.len(), ms: t0.elapsed().as_millis(), body }
}

/// GET using the account's cached access token; on 401 retries once with another `access_token` call.
pub async fn get(email: &str, path: &str, query: &[(&str, &str)]) -> Result<Reply, gtoken::Error> {
    let tok = gtoken::access_token(email).await?;
    let r = get_with(BASE, &tok, path, query).await;
    if r.kind == Kind::Unauthorized {
        let tok = gtoken::access_token(email).await?;
        return Ok(get_with(BASE, &tok, path, query).await);
    }
    Ok(r)
}

pub const META_HEADERS: [(&str, &str); 3] = [("metadataHeaders", "From"), ("metadataHeaders", "Subject"), ("metadataHeaders", "Date")];

pub async fn get_metadata(email: &str, id: &str) -> Result<Reply, gtoken::Error> {
    let mut q = vec![("format", "metadata")];
    q.extend_from_slice(&META_HEADERS);
    get(email, &format!("/messages/{id}"), &q).await
}

pub async fn get_raw(email: &str, id: &str) -> Result<Reply, gtoken::Error> {
    get(email, &format!("/messages/{id}"), &[("format", "raw")]).await
}

pub async fn profile(email: &str) -> Result<Reply, gtoken::Error> {
    get(email, "/profile", &[]).await
}

pub async fn list_inbox(email: &str, n: usize, page: Option<&str>) -> Result<Reply, gtoken::Error> {
    let n = n.to_string();
    let mut q = vec![("labelIds", "INBOX"), ("maxResults", n.as_str())];
    if let Some(p) = page {
        q.push(("pageToken", p));
    }
    get(email, "/messages", &q).await
}

pub fn history_query<'a>(start: &'a str, page: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut q = vec![
        ("startHistoryId", start),
        ("maxResults", "500"),
        ("historyTypes", "messageAdded"),
        ("historyTypes", "messageDeleted"),
        ("historyTypes", "labelAdded"),
        ("historyTypes", "labelRemoved"),
    ];
    if let Some(p) = page {
        q.push(("pageToken", p));
    }
    q
}

pub async fn history(email: &str, start: &str, page: Option<&str>) -> Result<Reply, gtoken::Error> {
    get(email, "/history", &history_query(start, page)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify(200, "", None), Kind::Ok);
        assert_eq!(classify(401, "", None), Kind::Unauthorized);
        assert_eq!(classify(429, "", Some(7)), Kind::RateLimited(Some(7)));
        assert_eq!(classify(403, "userRateLimitExceeded", None), Kind::RateLimited(None));
        assert_eq!(classify(403, "insufficientPermissions", None), Kind::Other);
        assert_eq!(classify(404, "notFound", None), Kind::Gone);
        assert_eq!(classify(503, "", None), Kind::Server);
    }
}
