//! httpmock tests for `GmailClient`, batch framing and error classification. Fixtures are synthetic.

use std::sync::Mutex;

use gmail_api::{GmailClient, Url, boundary_of, build_batch_body, classify, parse_batch_response};
use httpmock::prelude::*;
use mail_model::{
    ApiError, AuthError, GmailApi, HistoryEvent, HistoryId, LabelCounts, LabelId, LabelKind, MessageId, TokenSource,
};
use serde_json::{Value, json};

/// Hands out "t0", then "t1" once a refresh was forced; records every call.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<bool>>,
    dead: bool,
}

impl TokenSource for Fake {
    async fn access_token(&self, force_refresh: bool) -> Result<String, AuthError> {
        self.calls.lock().unwrap().push(force_refresh);
        if self.dead && force_refresh {
            return Err(AuthError::NeedsReauth { reason: "invalid_grant".into() });
        }
        Ok(if force_refresh { "t1" } else { "t0" }.to_string())
    }
}

fn client(server: &MockServer) -> GmailClient<Fake> {
    GmailClient::new(Fake::default(), Some(Url::parse(&server.base_url()).unwrap()))
}

fn mid(s: &str) -> MessageId {
    MessageId::new(s)
}

fn msg_json(id: &str) -> Value {
    json!({
        "id": id, "threadId": format!("th{id}"), "labelIds": ["INBOX", "UNREAD"],
        "snippet": "Your code is 123 &amp; more", "historyId": "9001", "internalDate": "1700000000000",
        "sizeEstimate": 4321,
        "payload": {"mimeType": "multipart/mixed", "headers": [
            {"name": "from", "value": "Bob <bob@example.test>"},
            {"name": "To", "value": "me@example.test"},
            {"name": "Subject", "value": "Hello"},
            {"name": "Date", "value": "Tue, 14 Nov 2023 22:13:20 +0000"}
        ]}
    })
}

fn batch_reply(parts: &[(u16, Value)]) -> String {
    let mut s = String::new();
    for (i, (status, body)) in parts.iter().enumerate() {
        s.push_str(&format!(
            "--rb\r\nContent-Type: application/http\r\nContent-ID: <response-item{i}>\r\n\r\nHTTP/1.1 {status} X\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{body}\r\n"
        ));
    }
    s.push_str("--rb--");
    s
}

// ---------------------------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn profile_parses_and_sends_bearer() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile").header("authorization", "Bearer t0");
            then.status(200).json_body(json!({"emailAddress": "a@example.test", "messagesTotal": 12, "threadsTotal": 3, "historyId": "555"}));
        })
        .await;
    let p = client(&server).profile().await.unwrap();
    assert_eq!(p.email, "a@example.test");
    assert_eq!(p.messages_total, 12);
    assert_eq!(p.history_id, HistoryId(555));
    m.assert_async().await;
}

#[tokio::test]
async fn labels_and_counts() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/labels");
            then.status(200).json_body(json!({"labels": [
                {"id": "INBOX", "name": "INBOX", "type": "system", "labelListVisibility": "labelHide"},
                {"id": "Label_12", "name": "Work/Sub", "type": "user"}
            ]}));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/labels/INBOX");
            then.status(200).json_body(json!({"id": "INBOX", "messagesTotal": 40, "messagesUnread": 3}));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/labels/Label_12");
            then.status(200).json_body(json!({"id": "Label_12"}));
        })
        .await;
    let c = client(&server);
    let labels = c.labels().await.unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0].kind, LabelKind::System);
    assert_eq!(labels[1].kind, LabelKind::User);
    assert_eq!(labels[1].name, "Work/Sub");
    assert_eq!(c.label_counts(&LabelId::new("INBOX")).await.unwrap(), LabelCounts { total: Some(40), unread: Some(3) });
    assert_eq!(c.label_counts(&LabelId::new("Label_12")).await.unwrap(), LabelCounts::default());
}

#[tokio::test]
async fn list_with_label_and_page_token() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/gmail/v1/users/me/messages")
                .query_param("labelIds", "INBOX")
                .query_param("pageToken", "tok")
                .query_param("maxResults", "50")
                .query_param_missing("includeSpamTrash");
            then.status(200).json_body(json!({"messages": [{"id": "a", "threadId": "ta"}, {"id": "b", "threadId": "tb"}], "nextPageToken": "n2"}));
        })
        .await;
    let page = client(&server).list(Some(&LabelId::new("INBOX")), Some("tok"), 50).await.unwrap();
    assert_eq!(page.ids.len(), 2);
    assert_eq!(page.ids[1].0, mid("b"));
    assert_eq!(page.ids[1].1.as_str(), "tb");
    assert_eq!(page.next_page_token.as_deref(), Some("n2"));
    m.assert_async().await;
}

#[tokio::test]
async fn list_spam_and_trash_include_spam_trash() {
    let server = MockServer::start_async().await;
    for label in ["SPAM", "TRASH"] {
        let m = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/gmail/v1/users/me/messages")
                    .query_param("labelIds", label)
                    .query_param("includeSpamTrash", "true");
                then.status(200).json_body(json!({"resultSizeEstimate": 0}));
            })
            .await;
        let page = client(&server).list(Some(&LabelId::new(label)), None, 20).await.unwrap();
        assert!(page.ids.is_empty());
        assert_eq!(page.next_page_token, None);
        m.assert_async().await;
    }
}

#[tokio::test]
async fn list_all_mail_has_no_label_filter() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/gmail/v1/users/me/messages")
                .query_param_missing("labelIds")
                .query_param_missing("includeSpamTrash")
                .query_param("maxResults", "10");
            then.status(200).json_body(json!({"messages": [{"id": "z", "threadId": "tz"}]}));
        })
        .await;
    let page = client(&server).list(None, None, 10).await.unwrap();
    assert_eq!(page.ids.len(), 1);
    m.assert_async().await;
}

#[tokio::test]
async fn raw_decodes_base64url() {
    let server = MockServer::start_async().await;
    // "Subject: hi\r\n\r\n??>>" has bytes that differ between base64 and base64url.
    let bytes = b"Subject: hi\r\n\r\n\xfb\xff\xfe?>>";
    let enc = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE.encode(bytes) // padded: must be accepted too
    };
    let m = server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/messages/m1").query_param("format", "raw");
            then.status(200).json_body(json!({"id": "m1", "raw": enc}));
        })
        .await;
    assert_eq!(client(&server).raw(&mid("m1")).await.unwrap(), bytes.to_vec());
    m.assert_async().await;
}

#[tokio::test]
async fn history_maps_events_in_record_order() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/gmail/v1/users/me/history")
                .query_param("startHistoryId", "100")
                .query_param("maxResults", "500")
                .query_param("historyTypes", "messageAdded")
                .query_param("historyTypes", "messageDeleted")
                .query_param("historyTypes", "labelAdded")
                .query_param("historyTypes", "labelRemoved")
                .query_param("pageToken", "p2");
            then.status(200).json_body(json!({
                "history": [
                    {"id": "101",
                     "labelsRemoved": [{"message": {"id": "c", "threadId": "tc"}, "labelIds": ["UNREAD"]}],
                     "messagesAdded": [{"message": {"id": "a", "threadId": "ta", "labelIds": ["INBOX"]}}],
                     "messagesDeleted": [{"message": {"id": "b", "threadId": "tb"}}],
                     "labelsAdded": [{"message": {"id": "c", "threadId": "tc"}, "labelIds": ["STARRED", "INBOX"]}]},
                    {"id": "102", "messagesAdded": [{"message": {"id": "d", "threadId": "td"}}]}
                ],
                "nextPageToken": "p3", "historyId": "777"
            }));
        })
        .await;
    let page = client(&server).history(HistoryId(100), Some("p2")).await.unwrap();
    m.assert_async().await;
    assert_eq!(page.history_id, HistoryId(777));
    assert_eq!(page.next_page_token.as_deref(), Some("p3"));
    assert_eq!(page.events.len(), 5);
    assert!(matches!(&page.events[0], HistoryEvent::Added { id, label_ids, .. } if id == &mid("a") && label_ids == &[LabelId::new("INBOX")]));
    assert!(matches!(&page.events[1], HistoryEvent::Deleted { id } if id == &mid("b")));
    assert!(matches!(&page.events[2], HistoryEvent::LabelsAdded { label_ids, .. } if label_ids.len() == 2));
    assert!(matches!(&page.events[3], HistoryEvent::LabelsRemoved { id, label_ids } if id == &mid("c") && label_ids[0].as_str() == "UNREAD"));
    assert!(matches!(&page.events[4], HistoryEvent::Added { id, label_ids, .. } if id == &mid("d") && label_ids.is_empty()));
}

#[tokio::test]
async fn history_with_no_changes() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/history");
            then.status(200).json_body(json!({"historyId": "42"}));
        })
        .await;
    let page = client(&server).history(HistoryId(40), None).await.unwrap();
    assert!(page.events.is_empty());
    assert_eq!(page.history_id, HistoryId(42));
    assert_eq!(page.next_page_token, None);
}

// ---------------------------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn batch_posts_framed_gets_and_reports_failed_sub_response() {
    let server = MockServer::start_async().await;
    let reply = batch_reply(&[
        (200, msg_json("a")),
        (404, json!({"error": {"code": 404, "message": "Requested entity was not found.", "errors": [{"reason": "notFound"}], "status": "NOT_FOUND"}})),
        (200, msg_json("c")),
    ]);
    let m = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/batch/gmail/v1")
                .header("authorization", "Bearer t0")
                .header_includes("content-type", "multipart/mixed; boundary=")
                .body_includes("GET /gmail/v1/users/me/messages/a?format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Subject&metadataHeaders=Date\r\n")
                .body_includes("Content-ID: <item2>")
                .body_includes("GET /gmail/v1/users/me/messages/c?");
            then.status(200).header("content-type", "multipart/mixed; boundary=rb").body(reply);
        })
        .await;
    let out = client(&server).metadata_batch(&[mid("a"), mid("b"), mid("c")]).await.unwrap();
    m.assert_async().await;
    assert_eq!(out.len(), 3);
    let a = out[0].as_ref().unwrap();
    assert_eq!(a.id, mid("a"));
    assert_eq!(a.thread_id.as_str(), "tha");
    assert_eq!(a.label_ids, vec![LabelId::new("INBOX"), LabelId::new("UNREAD")]);
    assert_eq!(a.internal_date_ms, 1_700_000_000_000);
    assert_eq!(a.size_estimate, 4321);
    assert_eq!(a.history_id, HistoryId(9001));
    assert_eq!(a.mime_type, "multipart/mixed");
    assert_eq!(a.from_raw, "Bob <bob@example.test>");
    assert_eq!(a.to_raw, "me@example.test");
    assert_eq!(a.subject, "Hello");
    assert_eq!(a.date_raw, "Tue, 14 Nov 2023 22:13:20 +0000");
    assert_eq!(a.snippet_raw, "Your code is 123 &amp; more", "snippet is passed through undecoded");
    assert_eq!(out[1], Err(ApiError::NotFound));
    assert_eq!(out[2].as_ref().unwrap().id, mid("c"));
}

#[tokio::test]
async fn batch_sub_response_rate_limit_is_per_message() {
    let server = MockServer::start_async().await;
    let reply = batch_reply(&[(403, json!({"error": {"errors": [{"reason": "userRateLimitExceeded"}], "status": "PERMISSION_DENIED"}}))]);
    server
        .mock_async(|when, then| {
            when.method(POST).path("/batch/gmail/v1");
            then.status(200).header("content-type", "multipart/mixed; boundary=rb").body(reply);
        })
        .await;
    let out = client(&server).metadata_batch(&[mid("a")]).await.unwrap();
    assert_eq!(out[0], Err(ApiError::RateLimited { retry_after_s: None }));
}

#[tokio::test]
async fn failed_batch_falls_back_to_concurrent_gets() {
    let server = MockServer::start_async().await;
    let batch = server
        .mock_async(|when, then| {
            when.method(POST).path("/batch/gmail/v1");
            then.status(500).json_body(json!({"error": {"errors": [{"reason": "backendError"}]}}));
        })
        .await;
    for id in ["a", "c"] {
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path(format!("/gmail/v1/users/me/messages/{id}"))
                    .query_param("format", "metadata")
                    .query_param("metadataHeaders", "Subject");
                then.status(200).json_body(msg_json(id));
            })
            .await;
    }
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/messages/b");
            then.status(404).json_body(json!({"error": {"errors": [{"reason": "notFound"}]}}));
        })
        .await;
    let out = client(&server).metadata_batch(&[mid("a"), mid("b"), mid("c")]).await.unwrap();
    batch.assert_async().await;
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].as_ref().unwrap().id, mid("a"));
    assert_eq!(out[1], Err(ApiError::NotFound));
    assert_eq!(out[2].as_ref().unwrap().id, mid("c"));
}

#[tokio::test]
async fn batch_with_wrong_part_count_falls_back() {
    let server = MockServer::start_async().await;
    let reply = batch_reply(&[(200, msg_json("a"))]);
    server
        .mock_async(|when, then| {
            when.method(POST).path("/batch/gmail/v1");
            then.status(200).header("content-type", "multipart/mixed; boundary=rb").body(reply);
        })
        .await;
    for id in ["a", "b"] {
        server
            .mock_async(|when, then| {
                when.method(GET).path(format!("/gmail/v1/users/me/messages/{id}"));
                then.status(200).json_body(msg_json(id));
            })
            .await;
    }
    let out = client(&server).metadata_batch(&[mid("a"), mid("b")]).await.unwrap();
    assert_eq!(out.iter().filter(|r| r.is_ok()).count(), 2);
}

#[tokio::test]
async fn batch_empty_makes_no_call() {
    let server = MockServer::start_async().await;
    let c = client(&server);
    assert!(c.metadata_batch(&[]).await.unwrap().is_empty());
}

#[test]
fn batch_framing_roundtrip() {
    let body = build_batch_body("bb", &["/gmail/v1/users/me/messages/a?format=metadata".into(), "/gmail/v1/users/me/messages/b".into()]);
    assert!(body.starts_with("--bb\r\nContent-Type: application/http\r\nContent-ID: <item0>\r\n\r\nGET /gmail/v1/users/me/messages/a?format=metadata\r\n"));
    assert!(body.contains("Content-ID: <item1>"));
    assert_eq!(body.matches("--bb\r\n").count(), 2);
    assert!(body.ends_with("--bb--\r\n"));
    assert_eq!(body.matches("GET ").count(), 2);
    assert_eq!(boundary_of("multipart/mixed; boundary=batch_xyz"), Some("batch_xyz".into()));
    assert_eq!(boundary_of("multipart/mixed; boundary=\"q q\"; charset=x"), Some("q q".into()));
    assert_eq!(boundary_of("multipart/mixed"), None);
    let parts = parse_batch_response(&batch_reply(&[(200, json!({"id": "a"})), (404, json!({"error": {}})), (500, json!("x"))]), "rb");
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].status, 200);
    assert_eq!(parts[0].body.as_ref().unwrap()["id"], "a");
    assert_eq!(parts[1].status, 404);
    assert_eq!(parts[2].status, 500);
    assert!(parts[2].body.is_none());
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_refreshes_once_and_retries() {
    let server = MockServer::start_async().await;
    let first = server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile").header("authorization", "Bearer t0");
            then.status(401).json_body(json!({"error": {"code": 401, "status": "UNAUTHENTICATED"}}));
        })
        .await;
    let second = server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile").header("authorization", "Bearer t1");
            then.status(200).json_body(json!({"emailAddress": "a@example.test", "messagesTotal": 1, "historyId": "5"}));
        })
        .await;
    let c = client(&server);
    assert_eq!(c.profile().await.unwrap().history_id, HistoryId(5));
    first.assert_calls_async(1).await;
    second.assert_calls_async(1).await;
}

#[tokio::test]
async fn second_401_is_needs_reauth_without_a_third_try() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile");
            then.status(401).json_body(json!({"error": {"status": "UNAUTHENTICATED"}}));
        })
        .await;
    let err = client(&server).profile().await.unwrap_err();
    assert!(matches!(err, ApiError::Auth(AuthError::NeedsReauth { .. })));
    m.assert_calls_async(2).await;
}

#[tokio::test]
async fn token_failure_on_forced_refresh_surfaces() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile");
            then.status(401);
        })
        .await;
    let c = GmailClient::new(Fake { dead: true, ..Default::default() }, Some(Url::parse(&server.base_url()).unwrap()));
    let err = c.profile().await.unwrap_err();
    assert_eq!(err, ApiError::Auth(AuthError::NeedsReauth { reason: "invalid_grant".into() }));
}

#[tokio::test]
async fn http_errors_are_classified() {
    let server = MockServer::start_async().await;
    let cases: [(&str, u16, Option<&str>, Value); 6] = [
        ("/gmail/v1/users/me/labels", 403, None, json!({"error": {"errors": [{"reason": "rateLimitExceeded"}], "status": "PERMISSION_DENIED"}})),
        ("/gmail/v1/users/me/profile", 429, Some("7"), json!({})),
        ("/gmail/v1/users/me/history", 404, None, json!({"error": {"errors": [{"reason": "notFound"}]}})),
        ("/gmail/v1/users/me/messages/m", 404, None, json!({"error": {"errors": [{"reason": "notFound"}]}})),
        ("/gmail/v1/users/me/labels/X", 403, None, json!({"error": {"errors": [{"reason": "insufficientPermissions"}], "status": "PERMISSION_DENIED"}})),
        ("/gmail/v1/users/me/messages", 503, None, json!({"error": {"errors": [{"reason": "backendError", "message": "secret text"}]}})),
    ];
    for (path, status, retry, body) in cases {
        server
            .mock_async(|when, then| {
                when.method(GET).path(path);
                let t = then.status(status).json_body(body);
                if let Some(r) = retry {
                    t.header("retry-after", r);
                }
            })
            .await;
    }
    let c = client(&server);
    assert_eq!(c.labels().await.unwrap_err(), ApiError::RateLimited { retry_after_s: None });
    assert_eq!(c.profile().await.unwrap_err(), ApiError::RateLimited { retry_after_s: Some(7) });
    assert_eq!(c.history(HistoryId(1), None).await.unwrap_err(), ApiError::HistoryExpired);
    assert_eq!(c.raw(&mid("m")).await.unwrap_err(), ApiError::NotFound);
    assert!(matches!(c.label_counts(&LabelId::new("X")).await.unwrap_err(), ApiError::Auth(AuthError::NeedsReauth { .. })));
    assert_eq!(c.list(None, None, 5).await.unwrap_err(), ApiError::Status { code: 503, reason: "backendError".into() });
}

#[tokio::test]
async fn transport_failure_is_offline() {
    // Port 1 on loopback refuses connections.
    let c = GmailClient::new(Fake::default(), Some(Url::parse("http://127.0.0.1:1/").unwrap()));
    assert!(matches!(c.profile().await.unwrap_err(), ApiError::Offline(_)));
    // A whole-call batch failure caused by being offline is reported, not turned into 50 GETs.
    assert!(matches!(c.metadata_batch(&[mid("a")]).await.unwrap_err(), ApiError::Offline(_)));
}

#[tokio::test]
async fn non_json_success_is_decode_error() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/gmail/v1/users/me/profile");
            then.status(200).body("<html>");
        })
        .await;
    assert!(matches!(client(&server).profile().await.unwrap_err(), ApiError::Decode(_)));
}

#[test]
fn classify_unit() {
    assert_eq!(classify(429, "", Some(3), false), ApiError::RateLimited { retry_after_s: Some(3) });
    assert_eq!(classify(403, r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#, None, false), ApiError::RateLimited { retry_after_s: None });
    assert_eq!(classify(404, "", None, true), ApiError::HistoryExpired);
    assert_eq!(classify(404, "", None, false), ApiError::NotFound);
    assert!(matches!(classify(401, "", None, false), ApiError::Auth(AuthError::NeedsReauth { .. })));
    assert!(matches!(classify(403, r#"{"error":{"status":"PERMISSION_DENIED"}}"#, None, false), ApiError::Auth(AuthError::NeedsReauth { .. })));
    // Reason is the short code from status when there is no errors[], never the message.
    assert_eq!(classify(400, r#"{"error":{"status":"INVALID_ARGUMENT","message":"private"}}"#, None, false), ApiError::Status { code: 400, reason: "INVALID_ARGUMENT".into() });
    assert_eq!(classify(500, "not json", None, false), ApiError::Status { code: 500, reason: String::new() });
}
