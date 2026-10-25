//! `format=full` payload tree helpers: what you would have to do yourself if you did not use the raw MIME.
//! Used only by `stats` to compare against `mail-parser`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FullAtt {
    pub filename: String,
    pub mime: String,
    pub size: u64,
    pub attachment_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct FullView {
    pub html: Vec<String>,
    pub text: Vec<String>,
    /// The same parts decoded as UTF-8 regardless of the declared charset (Gmail re-encodes bodies to UTF-8).
    pub html_u8: Vec<String>,
    pub text_u8: Vec<String>,
    pub attachments: Vec<FullAtt>,
    /// Body-type parts that only carry an `attachmentId` (their text is NOT in the message response).
    pub body_parts_by_attachment_id: usize,
    pub part_count: usize,
    /// Top-level headers (name lower-cased -> raw undecoded value).
    pub headers: Vec<(String, String)>,
    /// Text parts that declared a charset that `encoding_rs` does not know.
    pub unknown_charsets: usize,
}

fn header<'a>(part: &'a Value, name: &str) -> Option<&'a str> {
    part["headers"].as_array()?.iter().find_map(|h| {
        (h["name"].as_str()?.eq_ignore_ascii_case(name)).then(|| h["value"].as_str()).flatten()
    })
}

fn charset_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let p = lower.find("charset=")? + 8;
    Some(lower[p..].trim_start_matches('"').split(['"', ';', ' ']).next()?.to_string())
}

fn decode_u8(part: &Value) -> Option<String> {
    let data = part["body"]["data"].as_str()?;
    let bytes = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn decode_body(part: &Value, unknown: &mut usize) -> Option<String> {
    let data = part["body"]["data"].as_str()?;
    let bytes = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')).ok()?;
    let cs = header(part, "content-type").and_then(charset_of);
    let enc = match cs.as_deref() {
        Some(label) => match encoding_rs::Encoding::for_label(label.as_bytes()) {
            Some(e) => e,
            None => {
                *unknown += 1;
                encoding_rs::UTF_8
            }
        },
        None => encoding_rs::UTF_8,
    };
    Some(enc.decode(&bytes).0.into_owned())
}

fn walk(part: &Value, out: &mut FullView) {
    out.part_count += 1;
    let mime = part["mimeType"].as_str().unwrap_or("").to_ascii_lowercase();
    if let Some(kids) = part["parts"].as_array() {
        for k in kids {
            walk(k, out);
        }
        return;
    }
    let filename = part["filename"].as_str().unwrap_or("").to_string();
    let disp_attachment = header(part, "content-disposition").is_some_and(|d| d.to_ascii_lowercase().starts_with("attachment"));
    let aid = part["body"]["attachmentId"].as_str().map(str::to_string);
    let is_body_type = mime == "text/html" || mime == "text/plain";
    if is_body_type && filename.is_empty() && !disp_attachment {
        match decode_body(part, &mut out.unknown_charsets) {
            Some(t) if mime == "text/html" => {
                out.html.push(t);
                out.html_u8.extend(decode_u8(part));
            }
            Some(t) => {
                out.text.push(t);
                out.text_u8.extend(decode_u8(part));
            }
            None => {
                if aid.is_some() {
                    out.body_parts_by_attachment_id += 1;
                }
            }
        }
    } else {
        out.attachments.push(FullAtt { filename, mime, size: part["body"]["size"].as_u64().unwrap_or(0), attachment_id: aid });
    }
}

pub fn view(full: &Value) -> FullView {
    let mut v = FullView::default();
    walk(&full["payload"], &mut v);
    v.headers = full["payload"]["headers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| Some((h["name"].as_str()?.to_ascii_lowercase(), h["value"].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    v
}

/// Normalise for comparing two decodings of the same body: line endings, `<meta>` tags (mail-parser rewrites the
/// charset one), surrounding whitespace.
pub fn norm_body(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(a) = lower[pos..].find("<meta").map(|p| p + pos) {
        out.push_str(&s[pos..a]);
        pos = lower[a..].find('>').map_or(s.len(), |e| a + e + 1);
    }
    out.push_str(&s[pos..]);
    // Whitespace runs differ legitimately (quoted-printable trailing-space rules), so compare modulo whitespace.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
