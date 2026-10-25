//! `format=raw` bytes -> structured message, via `mail-parser` 1.0.
//!
//! Output is plain owned data so the renderer and the Tauri harness never touch the parser types.

use std::collections::HashMap;

use mail_parser::{MessageParser, MessagePart, PartKind, PartRole};

use crate::auth::{self, AuthSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub name: Option<String>,
    pub address: String,
}

impl Addr {
    /// `Name <addr>` or just `addr`.
    pub fn display(&self) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => format!("{n} <{}>", self.address),
            _ => self.address.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    /// `mail-parser` part id; stable for the same raw bytes. Used in `gmail-asset` URLs.
    pub part_id: u32,
    pub name: Option<String>,
    pub mime: String,
    /// Transfer-decoded size in bytes.
    pub size: usize,
    pub content_id: Option<String>,
    /// Referenced by `cid:` from the chosen HTML body (shown inline, so hidden from the attachment list).
    pub inline_referenced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Html,
    /// Plain text converted to HTML.
    Text,
    None,
}

/// One piece of a plain-text body, in reading order: text, or an image embedded between text parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyItem {
    Text(String),
    Image(u32),
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub from: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub subject: Option<String>,
    /// `Date` header as a UTC unix timestamp.
    pub date_ts: Option<i64>,
    pub message_id: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    /// The text body with embedded images (multipart/mixed with an image between text parts).
    pub text_items: Vec<BodyItem>,
    pub has_html_part: bool,
    pub has_text_part: bool,
    pub attachments: Vec<AttachmentInfo>,
    /// content-id (without `<>`, lower-cased) -> part id, for every part that has one.
    pub cids: HashMap<String, u32>,
    pub auth: Option<AuthSummary>,
    /// Number of `Authentication-Results` headers (a spoofable one may sit below Gmail's).
    pub auth_header_count: usize,
    pub charsets: Vec<String>,
    /// Text parts whose charset was unknown or whose bytes were malformed in that charset.
    pub charset_problems: usize,
    pub part_count: usize,
    pub structure: String,
    /// Subject/From/To/Cc contained RFC 2047 encoded words in the raw header (decoded by the parser).
    pub had_encoded_words: bool,
}

impl Parsed {
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
            let address = m.address()?.trim().to_string();
            if address.is_empty() {
                return None;
            }
            let name = m.name().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            Some(Addr { name, address })
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

/// Content shape of the MIME tree, e.g. `multipart/mixed(multipart/alternative(text/plain,text/html),application/pdf)`.
/// Only MIME types: content-free.
fn shape(p: &MessagePart<'_>) -> String {
    let mime = mime_of(p);
    let kids: Vec<String> = p.children().map(|c| shape(&c)).collect();
    if kids.is_empty() { mime } else { format!("{mime}({})", kids.join(",")) }
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

/// Strip `<>` and lower-case a Content-ID / `cid:` reference.
pub fn norm_cid(s: &str) -> String {
    s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
}

pub fn parse(raw: &[u8]) -> Option<Parsed> {
    let msg = MessageParser::new().parse(raw)?;
    let root = msg.root();

    // Bodies. `html_body()` holds genuine HTML parts and text parts standing in for missing HTML.
    let html_parts: Vec<_> = msg.html_body().filter(|p| matches!(p.kind(), PartKind::Html)).collect();
    let text_parts: Vec<_> = msg.text_body().filter(|p| matches!(p.kind(), PartKind::Text)).collect();
    let html = (!html_parts.is_empty()).then(|| {
        html_parts.iter().filter_map(part_text).collect::<Vec<_>>().join("
")
    });
    let text = (!text_parts.is_empty()).then(|| {
        text_parts.iter().filter_map(part_text).collect::<Vec<_>>().join("

")
    });

    let text_items: Vec<BodyItem> = msg
        .text_body()
        .filter_map(|p| match p.kind() {
            PartKind::Text => part_text(&p).map(BodyItem::Text),
            _ if p.role() == PartRole::MediaBody && mime_of(&p).starts_with("image/") => Some(BodyItem::Image(p.id())),
            _ => None,
        })
        .collect();

    let mut charsets = Vec::new();
    let mut charset_problems = 0;
    let mut cids = HashMap::new();
    let mut part_count = 0;
    for p in msg.parts() {
        part_count += 1;
        if let Some(cid) = p.content_id() {
            cids.insert(norm_cid(cid), p.id());
        }
        if p.is_text() {
            if let Some(cs) = p.content_type().and_then(|c| c.attribute("charset")) {
                charsets.push(cs.to_ascii_lowercase());
            }
            if p.text_checked().is_some_and(|(_, problems)| !problems.is_empty()) {
                charset_problems += 1;
            }
        }
    }

    let referenced: Vec<String> = html
        .as_deref()
        .map(|h| {
            let lower = h.to_ascii_lowercase();
            cids.keys().filter(|k| lower.contains(&format!("cid:{k}"))).cloned().collect()
        })
        .unwrap_or_default();

    let mut attachments: Vec<AttachmentInfo> = msg
        .attachments()
        .map(|p| {
            let content_id = p.content_id().map(norm_cid);
            let inline_referenced = content_id.as_ref().is_some_and(|c| referenced.contains(c));
            AttachmentInfo {
                part_id: p.id(),
                name: p.attachment_name().map(str::to_string),
                mime: if p.is_message() { "message/rfc822".to_string() } else { mime_of(&p) },
                size: p.decoded_len(),
                content_id,
                inline_referenced,
            }
        })
        .collect();
    // Inline media that is part of the body flow but would not be displayed (the chosen body is HTML and does not
    // reference it) must still be offered, otherwise it is invisible.
    if html.is_some() {
        for p in msg.parts().filter(|p| p.role() == PartRole::MediaBody) {
            let cid = p.content_id().map(norm_cid);
            if cid.as_ref().is_some_and(|c| referenced.contains(c)) || attachments.iter().any(|a| a.part_id == p.id()) {
                continue;
            }
            attachments.push(AttachmentInfo {
                part_id: p.id(),
                name: p.attachment_name().map(str::to_string),
                mime: mime_of(&p),
                size: p.decoded_len(),
                content_id: cid,
                inline_referenced: false,
            });
        }
    }

    let headers = msg.headers();
    let auth_values: Vec<String> = headers
        .all("authentication-results")
        .map(|h| String::from_utf8_lossy(h.raw_value()).into_owned())
        .collect();

    let had_encoded_words = ["subject", "from", "to", "cc"]
        .iter()
        .any(|n| headers.all(*n).any(|h| String::from_utf8_lossy(h.raw_value()).contains("=?")));

    Some(Parsed {
        had_encoded_words,
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
        attachments,
        cids,
        auth: auth::summarize(&auth_values),
        auth_header_count: auth_values.len(),
        charsets,
        charset_problems,
        part_count,
        structure: shape(&root.root_part()),
    })
}

/// Decoded bytes and MIME type of one part, for the asset protocol and "save attachment".
pub fn part_bytes(raw: &[u8], part_id: u32) -> Option<(String, Vec<u8>)> {
    let msg = MessageParser::new().parse(raw)?;
    let p = msg.part(part_id)?;
    if p.is_multipart() {
        return None;
    }
    let mime = if p.is_message() { "message/rfc822".to_string() } else { mime_of(&p) };
    let data = if p.is_message() { p.raw().to_vec() } else { p.decoded().into_owned() };
    Some((mime, data))
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
