//! HTTP error classification (PLAN §3.4), ported from spike S4's `api.rs`.

use mail_model::ApiError;

/// Map a non-success response to an [`ApiError`].
///
/// - 403 with reason `rateLimitExceeded` / `userRateLimitExceeded`, or 429 -> `RateLimited{retry_after_s}`
/// - 404 on a history call (`is_history`) -> `HistoryExpired`; other 404 -> `NotFound`
/// - 401, or 403 `PERMISSION_DENIED` / `insufficientPermissions` -> `Auth(NeedsReauth)` (the 401 retry-once
///   happens in the client *before* this classification)
/// - everything else -> `Status{code, reason}`; `reason` is Google's short `errors[0].reason` / `status`, never
///   the human message.
pub fn classify(status: u16, body: &str, retry_after_s: Option<u64>, is_history: bool) -> ApiError {
    todo!()
}
