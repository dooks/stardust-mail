//! `format=raw` bytes -> owned structured data via `mail-parser` 1.0 (port of spike S3 `parse.rs`).

use std::collections::HashMap;

use mail_model::{Addr, PartId, RenderError};

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
        todo!()
    }
}

/// Repair C1 control characters that really are windows-1252 punctuation (mislabelled charsets).
pub fn fix_c1(s: &str) -> String {
    todo!()
}

/// Normalise a `Content-ID` / `cid:` reference (strip `<>` and `cid:`, percent-decode, lower-case).
pub fn norm_cid(s: &str) -> String {
    todo!()
}

/// Parse raw RFC 822 bytes. `RenderError::Parse` only if nothing usable can be extracted.
pub fn parse(raw: &[u8]) -> Result<Parsed, RenderError> {
    todo!()
}
