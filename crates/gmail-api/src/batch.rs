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
    let mut s = String::new();
    for (i, g) in gets.iter().enumerate() {
        s.push_str(&format!(
            "--{boundary}\r\nContent-Type: application/http\r\nContent-ID: <item{i}>\r\n\r\nGET {g}\r\n\r\n"
        ));
    }
    s.push_str(&format!("--{boundary}--\r\n"));
    s
}

/// Extract `boundary=` from a Content-Type value.
pub fn boundary_of(content_type: &str) -> Option<String> {
    let i = content_type.find("boundary=")?;
    let v = content_type[i + 9..].split(';').next()?.trim().trim_matches('"');
    if v.is_empty() { None } else { Some(v.to_string()) }
}

/// Split a batch reply into sub-responses, in order.
pub fn parse_batch_response(body: &str, boundary: &str) -> Vec<BatchPart> {
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    for part in body.split(&delim) {
        let Some(h) = part.find("HTTP/1.") else { continue };
        let inner = &part[h..];
        let status = inner.get(9..12).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
        let json = inner.find("\r\n\r\n").map(|p| inner[p + 4..].trim()).filter(|t| t.starts_with('{'));
        out.push(BatchPart { status, body: json.and_then(|t| serde_json::from_str(t).ok()) });
    }
    out
}
