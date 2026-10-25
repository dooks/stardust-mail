//! Header block, sender check, attachment list and list-row helpers.

use mail_model::{Addr, AttachmentInfo, AuthSummary, MessageMeta, MessageRow};

use crate::parse::Parsed;

/// The addresses and subject for the header area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderBlock {
    /// First `From` mailbox (`addr` empty if the header is missing).
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    /// Only when it differs from `from`.
    pub reply_to: Option<Addr>,
    pub subject: String,
    /// `Date` header in milliseconds since the epoch; `None` if absent or unparseable (the caller falls back to
    /// `internalDate`).
    pub date_ms: Option<i64>,
}

/// Extract the header area from a parsed message.
pub fn header_block(parsed: &Parsed) -> HeaderBlock {
    todo!()
}

/// The sender check from the first `Authentication-Results` header whose authserv-id is `*.google.com`:
/// `SPF pass · DKIM pass (domain) · DMARC pass` with a good / mixed / bad level; `none` when absent.
pub fn auth_summary(parsed: &Parsed) -> AuthSummary {
    todo!()
}

/// Attachments to list: every part that is not referenced inline by `cid:` (name falls back to `attachment`).
pub fn attachments(parsed: &Parsed) -> Vec<AttachmentInfo> {
    todo!()
}

/// Decode HTML character references in a Gmail snippet (`&#39;`, `&amp;`, ...).
pub fn snippet_text(raw: &str) -> String {
    todo!()
}

/// Parse the first mailbox of a `From`-style header value (RFC 2047 words decoded, quotes removed). An empty or
/// unparseable value gives `Addr { name: None, addr: <trimmed input> }`.
pub fn parse_from(raw: &str) -> Addr {
    todo!()
}

/// Convert cached metadata to a list row: `parse_from(from_raw)`, `snippet_text(snippet_raw)`, `unread` =
/// UNREAD label, `starred` = STARRED, `important` = IMPORTANT, `attachment_hint` = `mime_type ==
/// "multipart/mixed"`.
pub fn message_row(meta: &MessageMeta) -> MessageRow {
    todo!()
}
