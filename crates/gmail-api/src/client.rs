//! `GmailClient`: the `GmailApi` implementation over `reqwest`.

use std::time::Duration;

use base64::Engine;
use futures_util::stream::{self, StreamExt};
use mail_model::{
    ApiError, AuthError, GmailApi, HistoryEvent, HistoryId, HistoryPage, LabelCounts, LabelId, LabelInfo, LabelKind,
    ListPage, MessageId, MessageMeta, Profile, ThreadId, TokenSource,
};
use reqwest::Url;
use serde_json::Value;

use crate::batch::{BatchPart, boundary_of, build_batch_body, parse_batch_response};
use crate::classify::classify;
use crate::{BATCH_PATH, DEFAULT_BASE, FALLBACK_CONCURRENCY};

/// Subject etc. requested for every metadata fetch.
const META_HEADERS: [&str; 4] = ["From", "To", "Subject", "Date"];
const BATCH_BOUNDARY: &str = "stardust_mail_batch_7f3a9c21";

/// Gmail client for ONE account, generic over where its bearer tokens come from (`gauth::Tokens` in the app, a
/// fake in tests).
///
/// On a 401 it calls `access_token(force_refresh = true)` once and retries once. Transport errors are
/// `ApiError::Offline`. `metadata_batch` posts one multipart batch of GET sub-requests and falls back to
/// concurrent GETs (concurrency [`crate::FALLBACK_CONCURRENCY`]) if the batch call itself fails.
pub struct GmailClient<T> {
    tokens: T,
    http: reqwest::Client,
    base: Url,
}

/// A successful (2xx) response.
struct Reply {
    body: Vec<u8>,
    content_type: Option<String>,
}

impl<T: TokenSource + Send + Sync> GmailClient<T> {
    /// `base` overrides [`crate::DEFAULT_BASE`] (httpmock in tests). HTTP/2, rustls, request timeouts set here.
    pub fn new(tokens: T, base: Option<Url>) -> Self {
        let base = base.unwrap_or_else(|| Url::parse(DEFAULT_BASE).expect("DEFAULT_BASE is a valid URL"));
        // HTTP/2 is negotiated through ALPN over TLS (the `http2` reqwest feature).
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        GmailClient { tokens, http, base }
    }

    /// `<base>/gmail/v1/users/me/<segments..>?<query>`.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut u = self.base.clone();
        {
            let mut p = u.path_segments_mut().expect("http base URL");
            p.pop_if_empty().extend(["gmail", "v1", "users", "me"]).extend(segments);
        }
        if !query.is_empty() {
            u.query_pairs_mut().extend_pairs(query);
        }
        u
    }

    /// Send one request, with the bearer token; on 401 force-refresh the token once and retry once.
    /// Non-2xx answers are classified; transport failures are `Offline`.
    async fn send(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
        is_history: bool,
    ) -> Result<Reply, ApiError> {
        for attempt in 0..2 {
            let token = self.tokens.access_token(attempt > 0).await?;
            let resp = build(&self.http)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| ApiError::Offline(e.without_url().to_string()))?;
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            let content_type =
                resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let body = resp.bytes().await.map_err(|e| ApiError::Offline(e.without_url().to_string()))?;
            if (200..300).contains(&status) {
                tracing::debug!(status, bytes = body.len(), "gmail response");
                return Ok(Reply { body: body.to_vec(), content_type });
            }
            tracing::debug!(status, "gmail error response");
            if status == 401 && attempt == 0 {
                continue;
            }
            return Err(classify(status, &String::from_utf8_lossy(&body), retry_after, is_history));
        }
        // Unreachable: the second attempt always returns above.
        Err(ApiError::Auth(AuthError::NeedsReauth { reason: "unauthorized".into() }))
    }

    async fn get_json(&self, url: Url, is_history: bool) -> Result<Value, ApiError> {
        let reply = self.send(|c| c.get(url.clone()), is_history).await?;
        serde_json::from_slice(&reply.body).map_err(|_| ApiError::Decode("response is not JSON".into()))
    }

    fn meta_query<'a>() -> Vec<(&'a str, &'a str)> {
        let mut q = vec![("format", "metadata")];
        q.extend(META_HEADERS.iter().map(|h| ("metadataHeaders", *h)));
        q
    }

    async fn get_meta(&self, id: &MessageId) -> Result<MessageMeta, ApiError> {
        let url = self.url(&["messages", id.as_str()], &Self::meta_query());
        parse_meta(&self.get_json(url, false).await?)
    }

    /// One batch POST. `Err` means the batch call as a whole failed (not a sub-response).
    async fn batch_once(&self, ids: &[MessageId]) -> Result<Vec<BatchPart>, ApiError> {
        let gets: Vec<String> = ids
            .iter()
            .map(|id| {
                let u = self.url(&["messages", id.as_str()], &Self::meta_query());
                match u.query() {
                    Some(q) => format!("{}?{}", u.path(), q),
                    None => u.path().to_string(),
                }
            })
            .collect();
        let body = build_batch_body(BATCH_BOUNDARY, &gets);
        let mut url = self.base.clone();
        url.path_segments_mut().expect("http base URL").pop_if_empty().extend(BATCH_PATH.split('/'));
        let ctype = format!("multipart/mixed; boundary={BATCH_BOUNDARY}");
        let reply = self
            .send(|c| c.post(url.clone()).header("Content-Type", ctype.clone()).body(body.clone()), false)
            .await?;
        let boundary = reply
            .content_type
            .as_deref()
            .and_then(boundary_of)
            .ok_or_else(|| ApiError::Decode("batch response has no boundary".into()))?;
        let parts = parse_batch_response(&String::from_utf8_lossy(&reply.body), &boundary);
        if parts.len() != ids.len() {
            return Err(ApiError::Decode("batch response part count mismatch".into()));
        }
        Ok(parts)
    }
}

impl<T: TokenSource + Send + Sync> GmailApi for GmailClient<T> {
    async fn profile(&self) -> Result<Profile, ApiError> {
        let v = self.get_json(self.url(&["profile"], &[]), false).await?;
        Ok(Profile {
            email: str_field(&v, "emailAddress")?.to_string(),
            messages_total: v["messagesTotal"].as_u64().unwrap_or(0),
            history_id: history_id_of(&v["historyId"])?,
        })
    }

    async fn labels(&self) -> Result<Vec<LabelInfo>, ApiError> {
        let v = self.get_json(self.url(&["labels"], &[]), false).await?;
        let mut out = Vec::new();
        for l in v["labels"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let kind = if l["type"].as_str() == Some("system") { LabelKind::System } else { LabelKind::User };
            out.push(LabelInfo {
                id: LabelId::new(str_field(l, "id")?),
                name: l["name"].as_str().unwrap_or_default().to_string(),
                kind,
            });
        }
        Ok(out)
    }

    async fn label_counts(&self, id: &LabelId) -> Result<LabelCounts, ApiError> {
        let v = self.get_json(self.url(&["labels", id.as_str()], &[]), false).await?;
        let n = |k: &str| v[k].as_u64().and_then(|n| u32::try_from(n).ok());
        Ok(LabelCounts { total: n("messagesTotal"), unread: n("messagesUnread") })
    }

    /// Sets `includeSpamTrash=true` for `SPAM` / `TRASH`.
    async fn list(&self, label: Option<&LabelId>, page_token: Option<&str>, max: u32) -> Result<ListPage, ApiError> {
        let max = max.to_string();
        let mut q: Vec<(&str, &str)> = vec![("maxResults", &max)];
        if let Some(l) = label {
            q.push(("labelIds", l.as_str()));
            if l.as_str() == mail_model::label::SPAM || l.as_str() == mail_model::label::TRASH {
                q.push(("includeSpamTrash", "true"));
            }
        }
        if let Some(t) = page_token {
            q.push(("pageToken", t));
        }
        let v = self.get_json(self.url(&["messages"], &q), false).await?;
        let mut ids = Vec::new();
        for m in v["messages"].as_array().map(Vec::as_slice).unwrap_or_default() {
            ids.push((MessageId::new(str_field(m, "id")?), ThreadId::new(m["threadId"].as_str().unwrap_or_default())));
        }
        Ok(ListPage { ids, next_page_token: v["nextPageToken"].as_str().map(str::to_owned) })
    }

    async fn metadata_batch(&self, ids: &[MessageId]) -> Result<Vec<Result<MessageMeta, ApiError>>, ApiError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        match self.batch_once(ids).await {
            Ok(parts) => Ok(parts.into_iter().map(part_to_meta).collect()),
            // Offline, auth and throttling failures would hit the fallback's GETs just the same: report them.
            Err(e @ (ApiError::Offline(_) | ApiError::Auth(_) | ApiError::RateLimited { .. })) => Err(e),
            Err(e) => {
                tracing::debug!(error = %e, count = ids.len(), "batch failed; falling back to concurrent GETs");
                let results = stream::iter(ids.iter().cloned().map(|id| async move { self.get_meta(&id).await }))
                    .buffered(FALLBACK_CONCURRENCY)
                    .collect::<Vec<_>>()
                    .await;
                // Whole-call failure only if every GET failed for the same transport/auth/throttle reason.
                if let Some(Err(first)) = results.first()
                    && matches!(first, ApiError::Offline(_) | ApiError::Auth(_) | ApiError::RateLimited { .. })
                    && results.iter().all(|r| r.is_err())
                {
                    return Err(first.clone());
                }
                Ok(results)
            }
        }
    }

    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>, ApiError> {
        let v = self.get_json(self.url(&["messages", id.as_str()], &[("format", "raw")]), false).await?;
        let raw = str_field(&v, "raw")?.trim_end_matches('=');
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| ApiError::Decode("raw body is not base64url".into()))
    }

    /// `historyTypes=messageAdded,messageDeleted,labelAdded,labelRemoved&maxResults=500`; 404 -> `HistoryExpired`.
    async fn history(&self, start: HistoryId, page_token: Option<&str>) -> Result<HistoryPage, ApiError> {
        let start = start.to_string();
        let mut q: Vec<(&str, &str)> = vec![
            ("startHistoryId", &start),
            ("maxResults", "500"),
            ("historyTypes", "messageAdded"),
            ("historyTypes", "messageDeleted"),
            ("historyTypes", "labelAdded"),
            ("historyTypes", "labelRemoved"),
        ];
        if let Some(t) = page_token {
            q.push(("pageToken", t));
        }
        let v = self.get_json(self.url(&["history"], &q), true).await?;
        let mut events = Vec::new();
        for rec in v["history"].as_array().map(Vec::as_slice).unwrap_or_default() {
            for e in list_of(rec, "messagesAdded") {
                let m = &e["message"];
                events.push(HistoryEvent::Added {
                    id: MessageId::new(str_field(m, "id")?),
                    thread_id: ThreadId::new(m["threadId"].as_str().unwrap_or_default()),
                    label_ids: labels_of(&m["labelIds"]),
                });
            }
            for e in list_of(rec, "messagesDeleted") {
                events.push(HistoryEvent::Deleted { id: MessageId::new(str_field(&e["message"], "id")?) });
            }
            for e in list_of(rec, "labelsAdded") {
                events.push(HistoryEvent::LabelsAdded {
                    id: MessageId::new(str_field(&e["message"], "id")?),
                    label_ids: labels_of(&e["labelIds"]),
                });
            }
            for e in list_of(rec, "labelsRemoved") {
                events.push(HistoryEvent::LabelsRemoved {
                    id: MessageId::new(str_field(&e["message"], "id")?),
                    label_ids: labels_of(&e["labelIds"]),
                });
            }
        }
        Ok(HistoryPage {
            events,
            history_id: history_id_of(&v["historyId"])?,
            next_page_token: v["nextPageToken"].as_str().map(str::to_owned),
        })
    }
}

fn list_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn labels_of(v: &Value) -> Vec<LabelId> {
    v.as_array().map(|a| a.iter().filter_map(|l| l.as_str()).map(LabelId::new).collect()).unwrap_or_default()
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    v[key].as_str().ok_or_else(|| ApiError::Decode(format!("missing field `{key}`")))
}

/// Gmail sends history ids as decimal strings.
fn history_id_of(v: &Value) -> Result<HistoryId, ApiError> {
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .map(HistoryId)
        .ok_or_else(|| ApiError::Decode("missing or invalid historyId".into()))
}

fn part_to_meta(p: BatchPart) -> Result<MessageMeta, ApiError> {
    if (200..300).contains(&p.status) {
        match p.body {
            Some(v) => parse_meta(&v),
            None => Err(ApiError::Decode("empty batch sub-response".into())),
        }
    } else {
        let body = p.body.map(|v| v.to_string()).unwrap_or_default();
        Err(classify(p.status, &body, None, false))
    }
}

/// Parse a `messages.get?format=metadata` JSON object.
fn parse_meta(v: &Value) -> Result<MessageMeta, ApiError> {
    let header = |name: &str| -> String {
        v["payload"]["headers"]
            .as_array()
            .and_then(|hs| hs.iter().find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))))
            .and_then(|h| h["value"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let num = |k: &str| v[k].as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v[k].as_i64()).unwrap_or(0);
    Ok(MessageMeta {
        id: MessageId::new(str_field(v, "id")?),
        thread_id: ThreadId::new(v["threadId"].as_str().unwrap_or_default()),
        label_ids: labels_of(&v["labelIds"]),
        internal_date_ms: num("internalDate"),
        size_estimate: u64::try_from(num("sizeEstimate")).unwrap_or(0),
        history_id: history_id_of(&v["historyId"])?,
        mime_type: v["payload"]["mimeType"].as_str().unwrap_or_default().to_string(),
        from_raw: header("From"),
        to_raw: header("To"),
        subject: header("Subject"),
        date_raw: header("Date"),
        snippet_raw: v["snippet"].as_str().unwrap_or_default().to_string(),
    })
}
