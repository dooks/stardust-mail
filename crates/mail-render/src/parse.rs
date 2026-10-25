//! `format=raw` bytes -> owned structured data via `mail-parser` 1.0 (port of spike S3 `parse.rs`).

use std::collections::HashMap;

use mail_model::{Addr, PartId, RenderError};
use mail_parser::{MessageParser, MessagePart, PartKind, PartRole};

use crate::sanitize::percent_decode;

/// Which body the renderer will show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Html,
    /// Plain text converted to HTML.
    Text,
    None,
}

/// One piece of a plain-text body in reading order: text, or an image embedded between text parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyItem {
    Text(String),
    Image(PartId),
}

/// A non-body MIME part (attachment or inline resource), with its decoded bytes. `id` is the `mail-parser` part
/// index, stable for the same raw bytes; it is what `gmail-asset` URLs and `save_attachment` carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub id: PartId,
    pub mime: String,
    pub file_name: Option<String>,
    /// Normalised `Content-ID` (no angle brackets, lower-cased), if any.
    pub content_id: Option<String>,
    /// Transfer-decoded bytes.
    pub data: Vec<u8>,
    /// Referenced by `cid:` from the chosen HTML body (shown inline, so hidden from the attachment list).
    pub inline_referenced: bool,
}

/// A parsed message. Plain owned data: no `mail-parser` type escapes this crate.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub from: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub subject: Option<String>,
    /// `Date` header, seconds since the epoch.
    pub date_ts: Option<i64>,
    pub message_id: Option<String>,
    /// Chosen HTML body (C1/windows-1252 repaired), if any.
    pub html: Option<String>,
    /// Chosen plain-text body, if any.
    pub text: Option<String>,
    /// Plain-text body split in reading order with inline images (`text_items`).
    pub text_items: Vec<BodyItem>,
    pub has_html_part: bool,
    pub has_text_part: bool,
    /// Every non-body part by index (attachments and inline resources).
    pub parts: Vec<Part>,
    /// Normalised content-id -> part index.
    pub cids: HashMap<String, PartId>,
    /// Raw values of every `Authentication-Results` header, in order.
    pub authentication_results: Vec<String>,
    /// Charset decoding problems encountered (count only).
    pub charset_problems: usize,
    pub part_count: usize,
}

impl Parsed {
    /// HTML if present, otherwise text, otherwise none.
    pub fn body_kind(&self) -> BodyKind {
        if self.html.is_some() {
            BodyKind::Html
        } else if self.text.is_some() {
            BodyKind::Text
        } else {
            BodyKind::None
        }
    }
}

fn addrs(list: Option<mail_parser::AddressList<'_>>) -> Vec<Addr> {
    list.into_iter()
        .flat_map(|l| l.mailboxes())
        .filter_map(|m| {
            let addr = m.address()?.trim().to_string();
            if addr.is_empty() {
                return None;
            }
            let name = m.name().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            Some(Addr { name, addr })
        })
        .collect()
}

fn mime_of(p: &MessagePart<'_>) -> String {
    match p.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            None => ct.ctype().to_ascii_lowercase(),
        },
        None => "text/plain".to_string(),
    }
}

/// Repair C1 control characters (U+0080..U+009F). They appear when a sender labels windows-1252 bytes as UTF-8 or
/// Latin-1; browsers show nothing useful for them, but the bytes were meant as windows-1252 (en dash, curly quotes...).
pub fn fix_c1(s: &str) -> String {
    let is_c1 = |c: char| ('\u{80}'..='\u{9f}').contains(&c);
    if !s.chars().any(is_c1) {
        return s.to_string();
    }
    s.chars()
        .map(|c| {
            if is_c1(c) {
                let byte = [c as u8];
                let (d, _, _) = encoding_rs::WINDOWS_1252.decode(&byte);
                d.chars().next().filter(|r| *r != '\u{fffd}').unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Normalise a `Content-ID` / `cid:` reference (strip `<>` and `cid:`, percent-decode, lower-case).
pub fn norm_cid(s: &str) -> String {
    let t = s.trim();
    let t = if t.len() >= 4 && t.is_char_boundary(4) && t[..4].eq_ignore_ascii_case("cid:") { &t[4..] } else { t };
    let decoded = percent_decode(t);
    decoded.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
}

/// Parse raw RFC 822 bytes. `RenderError::Parse` only if nothing usable can be extracted.
pub fn parse(raw: &[u8]) -> Result<Parsed, RenderError> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Err(RenderError::Parse("empty message".into()));
    }
    let msg = MessageParser::new().parse(raw).ok_or_else(|| RenderError::Parse("unparseable message".into()))?;

    // Bodies. `html_body()` holds genuine HTML parts and text parts standing in for missing HTML.
    let html_parts: Vec<_> = msg.html_body().filter(|p| matches!(p.kind(), PartKind::Html)).collect();
    let text_parts: Vec<_> = msg.text_body().filter(|p| matches!(p.kind(), PartKind::Text)).collect();
    let html = (!html_parts.is_empty()).then(|| html_parts.iter().filter_map(part_text).collect::<Vec<_>>().join("\n"));
    let text = (!text_parts.is_empty()).then(|| text_parts.iter().filter_map(part_text).collect::<Vec<_>>().join("\n\n"));

    let text_items: Vec<BodyItem> = msg
        .text_body()
        .filter_map(|p| match p.kind() {
            PartKind::Text => part_text(&p).map(BodyItem::Text),
            _ if p.role() == PartRole::MediaBody && mime_of(&p).starts_with("image/") => {
                Some(BodyItem::Image(PartId(p.id())))
            }
            _ => None,
        })
        .collect();

    // Parts that are the chosen/alternative bodies are not listed as parts.
    let body_ids: Vec<u32> = msg
        .html_body()
        .chain(msg.text_body())
        .filter(|p| matches!(p.kind(), PartKind::Html | PartKind::Text))
        .map(|p| p.id())
        .collect();

    let mut charset_problems = 0;
    let mut cids = HashMap::new();
    let mut part_count = 0;
    for p in msg.parts() {
        part_count += 1;
        if let Some(cid) = p.content_id() {
            cids.insert(norm_cid(cid), PartId(p.id()));
        }
        if p.is_text() && p.text_checked().is_some_and(|(_, problems)| !problems.is_empty()) {
            charset_problems += 1;
        }
    }

    let referenced: Vec<String> = html
        .as_deref()
        .map(|h| {
            let lower = h.to_ascii_lowercase();
            cids.keys().filter(|k| lower.contains(&format!("cid:{k}"))).cloned().collect()
        })
        .unwrap_or_default();
    // With no HTML body, images between text parts are shown inline too.
    let inline_in_text: Vec<PartId> = if html.is_none() {
        text_items.iter().filter_map(|i| if let BodyItem::Image(id) = i { Some(*id) } else { None }).collect()
    } else {
        Vec::new()
    };

    // Attachments, plus inline media of the body flow (which must still be servable and, when not displayed,
    // offered). Parts of a nested `message/rfc822` are not listed: the message is one part.
    let mut listed: Vec<u32> = msg.attachments().map(|p| p.id()).collect();
    listed.extend(
        msg.parts()
            .filter(|p| p.role() == PartRole::MediaBody && !p.is_multipart() && !body_ids.contains(&p.id()))
            .map(|p| p.id()),
    );
    listed.sort_unstable();
    listed.dedup();

    let parts: Vec<Part> = listed
        .iter()
        .filter_map(|id| msg.part(*id))
        .filter(|p| !p.is_multipart() && !body_ids.contains(&p.id()))
        .map(|p| {
            let id = PartId(p.id());
            let content_id = p.content_id().map(norm_cid);
            let inline_referenced =
                content_id.as_ref().is_some_and(|c| referenced.contains(c)) || inline_in_text.contains(&id);
            let (mime, data) = if p.is_message() {
                ("message/rfc822".to_string(), p.raw().to_vec())
            } else {
                (mime_of(&p), p.decoded().into_owned())
            };
            Part { id, mime, file_name: p.attachment_name().map(str::to_string), content_id, data, inline_referenced }
        })
        .collect();

    let authentication_results: Vec<String> = msg
        .headers()
        .all("authentication-results")
        .map(|h| String::from_utf8_lossy(h.raw_value()).into_owned())
        .collect();

    Ok(Parsed {
        from: addrs(msg.from()),
        to: addrs(msg.to()),
        cc: addrs(msg.cc()),
        reply_to: addrs(msg.reply_to()),
        subject: msg.subject().map(str::to_string),
        date_ts: msg.date().filter(|d| d.is_valid()).map(|d| d.to_timestamp()),
        message_id: msg.message_id().map(str::to_string),
        has_html_part: !html_parts.is_empty(),
        has_text_part: !text_parts.is_empty(),
        html,
        text,
        text_items,
        parts,
        cids,
        authentication_results,
        charset_problems,
        part_count,
    })
}

/// Decode UTF-8, mapping each invalid byte through windows-1252 (what browsers effectively do for mislabeled mail).
fn utf8_with_cp1252_fallback(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    loop {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (good, rest) = bytes.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                let bad = e.error_len().unwrap_or(rest.len()).max(1);
                let (d, _, _) = encoding_rs::WINDOWS_1252.decode(&rest[..bad]);
                out.push_str(&d);
                bytes = &rest[bad..];
            }
        }
    }
}

/// Decoded text of a text/html part, repaired for the common "bytes are windows-1252 but labelled UTF-8" mistake.
/// `<meta charset>` is not rewritten: the sanitizer drops `<meta>` and the page declares UTF-8 itself.
fn part_text(p: &MessagePart<'_>) -> Option<String> {
    let t = p.text()?;
    if t.contains('\u{fffd}') {
        let cs = p.content_type().and_then(|c| c.attribute("charset")).map(str::to_ascii_lowercase);
        if matches!(cs.as_deref(), None | Some("utf-8" | "utf8" | "us-ascii" | "ascii")) {
            let bytes = p.decoded();
            if std::str::from_utf8(&bytes).is_err() {
                return Some(utf8_with_cp1252_fallback(&bytes));
            }
        }
    }
    Some(fix_c1(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c1_repair() {
        assert_eq!(fix_c1("\u{93}hi\u{94}"), "\u{201c}hi\u{201d}");
        assert_eq!(fix_c1("plain"), "plain");
    }

    #[test]
    fn cid_normalisation() {
        assert_eq!(norm_cid("<Logo@X>"), "logo@x");
        assert_eq!(norm_cid("cid:%3CLogo%40X%3E"), "logo@x");
        assert_eq!(norm_cid(" CID:abc "), "abc");
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(parse(b"").is_err());
        assert!(parse(b"  \r\n").is_err());
    }
}
