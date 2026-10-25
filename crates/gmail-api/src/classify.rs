//! HTTP error classification (PLAN §3.4), ported from spike S4's `api.rs`.

use mail_model::{ApiError, AuthError};
use serde_json::Value;

/// Map a non-success response to an [`ApiError`].
///
/// - 403 with reason `rateLimitExceeded` / `userRateLimitExceeded`, or 429 -> `RateLimited{retry_after_s}`
/// - 404 on a history call (`is_history`) -> `HistoryExpired`; other 404 -> `NotFound`
/// - 401, or 403 `PERMISSION_DENIED` / `insufficientPermissions` -> `Auth(NeedsReauth)` (the 401 retry-once
///   happens in the client *before* this classification)
/// - everything else -> `Status{code, reason}`; `reason` is Google's short `errors[0].reason` / `status`, never
///   the human message.
pub fn classify(status: u16, body: &str, retry_after_s: Option<u64>, is_history: bool) -> ApiError {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let first_reason = json["error"]["errors"][0]["reason"].as_str().map(str::to_owned);
    let status_str = json["error"]["status"].as_str().map(str::to_owned);
    // Rate-limit 403s can carry `status: PERMISSION_DENIED`, so look at every reason before the status.
    let any_reason = |f: &dyn Fn(&str) -> bool| {
        json["error"]["errors"].as_array().is_some_and(|a| a.iter().filter_map(|e| e["reason"].as_str()).any(f))
    };
    let reason = first_reason.clone().or(status_str.clone()).unwrap_or_default();

    match status {
        429 => ApiError::RateLimited { retry_after_s },
        403 if any_reason(&|r| r == "rateLimitExceeded" || r == "userRateLimitExceeded") => {
            ApiError::RateLimited { retry_after_s }
        }
        404 if is_history => ApiError::HistoryExpired,
        404 => ApiError::NotFound,
        401 => ApiError::Auth(AuthError::NeedsReauth { reason: "unauthorized".into() }),
        403 if status_str.as_deref() == Some("PERMISSION_DENIED") || any_reason(&|r| r == "insufficientPermissions") => {
            ApiError::Auth(AuthError::NeedsReauth { reason: if reason.is_empty() { "permission_denied".into() } else { reason } })
        }
        _ => ApiError::Status { code: status, reason },
    }
}
