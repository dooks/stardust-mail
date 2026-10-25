//! multipart/mixed batch framing (GET sub-requests only). Ported from spike S2 (`build_batch_body`,
//! `boundary_of`, `parse_batch_response`).

/// One sub-response of a batch reply, in request order.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchPart {
    /// HTTP status of the sub-response (0 if unparseable).
    pub status: u16,
    /// Parsed JSON body, if the part had one.
    pub body: Option<serde_json::Value>,
}

/// Build a multipart/mixed body containing only `GET <path>` sub-requests with `Content-ID: <itemN>`.
/// `gets` are paths like `/gmail/v1/users/me/messages/ID?format=metadata`.
pub fn build_batch_body(boundary: &str, gets: &[String]) -> String {
    todo!()
}

/// Extract `boundary=` from a Content-Type value.
pub fn boundary_of(content_type: &str) -> Option<String> {
    todo!()
}

/// Split a batch reply into sub-responses, in order.
pub fn parse_batch_response(body: &str, boundary: &str) -> Vec<BatchPart> {
    todo!()
}
