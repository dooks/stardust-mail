//! Header block, sender check, attachment list and list-row helpers.

use base64::Engine;
use mail_model::{Addr, AttachmentInfo, AuthLevel, AuthSummary, MessageMeta, MessageRow};

use crate::auth;
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
    let from = parsed.from.first().cloned().unwrap_or(Addr { name: None, addr: String::new() });
    let reply_to = parsed.reply_to.first().filter(|r| !r.addr.eq_ignore_ascii_case(&from.addr)).cloned();
    HeaderBlock {
        from,
        to: parsed.to.clone(),
        cc: parsed.cc.clone(),
        reply_to,
        subject: parsed.subject.clone().unwrap_or_default(),
        date_ms: parsed.date_ts.and_then(|s| s.checked_mul(1000)),
    }
}

/// The sender check from the first `Authentication-Results` header whose authserv-id is `*.google.com`:
/// `SPF pass · DKIM pass (domain) · DMARC pass` with a good / mixed / bad level; `none` when absent.
pub fn auth_summary(parsed: &Parsed) -> AuthSummary {
    match auth::summarize(&parsed.authentication_results) {
        Some(s) => AuthSummary {
            line: s.line(),
            level: match s.level() {
                "good" => AuthLevel::Good,
                "bad" => AuthLevel::Bad,
                "mixed" => AuthLevel::Mixed,
                _ => AuthLevel::None,
            },
            mailed_by: s.mailed_by,
            signed_by: s.signed_by,
        },
        None => AuthSummary {
            line: "no authentication results".to_string(),
            level: AuthLevel::None,
            mailed_by: None,
            signed_by: None,
        },
    }
}

/// Attachments to list: every part that is not referenced inline by `cid:` (name falls back to `attachment`).
pub fn attachments(parsed: &Parsed) -> Vec<AttachmentInfo> {
    parsed
        .parts
        .iter()
        .filter(|p| !p.inline_referenced)
        .map(|p| AttachmentInfo {
            part_id: p.id,
            name: p.file_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "attachment".to_string()),
            mime: p.mime.clone(),
            size: p.data.len() as u64,
        })
        .collect()
}

/// Value of an HTML character reference body (between `&` and `;`).
fn entity_char(body: &str) -> Option<char> {
    if let Some(num) = body.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(n).filter(|c| *c != '\0');
    }
    Some(match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "hellip" => '\u{2026}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "euro" => '\u{20ac}',
        "bull" => '\u{2022}',
        "middot" => '\u{b7}',
        _ => return None,
    })
}

/// Decode HTML character references in a Gmail snippet (`&#39;`, `&amp;`, ...).
pub fn snippet_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let decoded = after
            .find(';')
            .filter(|&e| (1..=10).contains(&e))
            .and_then(|e| entity_char(&after[..e]).map(|c| (c, e)));
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn q_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = h {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'=');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn decode_charset(cs: &str, bytes: &[u8]) -> String {
    match encoding_rs::Encoding::for_label(cs.as_bytes()) {
        Some(enc) => enc.decode_without_bom_handling(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn parse_encoded_word(after: &str) -> Option<(String, &str)> {
    let c_end = after.find('?')?;
    let charset = &after[..c_end];
    let r = &after[c_end + 1..];
    let e_end = r.find('?')?;
    let enc = &r[..e_end];
    let r2 = &r[e_end + 1..];
    let t_end = r2.find("?=")?;
    let text = &r2[..t_end];
    if enc.len() != 1 || charset.is_empty() || charset.contains(char::is_whitespace) || text.contains(char::is_whitespace) {
        return None;
    }
    let bytes = match enc {
        "B" | "b" => base64::engine::general_purpose::STANDARD.decode(text.as_bytes()).ok()?,
        "Q" | "q" => q_decode(text),
        _ => return None,
    };
    let charset = charset.split('*').next().unwrap_or(charset);
    Some((decode_charset(charset, &bytes), &r2[t_end + 2..]))
}

/// Decode RFC 2047 encoded words (B and Q). Whitespace between adjacent encoded words is dropped.
fn decode_rfc2047(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    let mut prev_encoded = false;
    while let Some(start) = rest.find("=?") {
        let before = &rest[..start];
        let after = &rest[start + 2..];
        match parse_encoded_word(after) {
            Some((decoded, tail)) => {
                if !(prev_encoded && before.chars().all(char::is_whitespace)) {
                    out.push_str(before);
                }
                out.push_str(&decoded);
                prev_encoded = true;
                rest = tail;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                prev_encoded = false;
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let mut out = String::new();
        let mut esc = false;
        for c in t[1..t.len() - 1].chars() {
            if esc {
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        t.to_string()
    }
}

/// Parse the first mailbox of a `From`-style header value (RFC 2047 words decoded, quotes removed). An empty or
/// unparseable value gives `Addr { name: None, addr: <trimmed input> }`.
pub fn parse_from(raw: &str) -> Addr {
    let s = raw.trim();
    let fallback = || Addr { name: None, addr: s.to_string() };
    if s.is_empty() {
        return fallback();
    }
    let (mut in_q, mut esc) = (false, false);
    let (mut lt, mut gt) = (None, None);
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_q => esc = true,
            '"' => in_q = !in_q,
            '<' if !in_q && lt.is_none() => lt = Some(i),
            '>' if !in_q && lt.is_some() && gt.is_none() => gt = Some(i),
            ',' if !in_q && lt.is_none() => break,
            _ => {}
        }
    }
    if let (Some(l), Some(g)) = (lt, gt) {
        let addr = s[l + 1..g].trim().to_string();
        let name = decode_rfc2047(&unquote(s[..l].trim())).trim().to_string();
        return Addr { name: (!name.is_empty()).then_some(name), addr };
    }
    let first = s.split(',').next().unwrap_or(s).trim();
    Addr { name: None, addr: first.to_string() }
}

/// Convert cached metadata to a list row: `parse_from(from_raw)`, `snippet_text(snippet_raw)`, `unread` =
/// UNREAD label, `starred` = STARRED, `important` = IMPORTANT, `attachment_hint` = `mime_type ==
/// "multipart/mixed"`.
pub fn message_row(meta: &MessageMeta) -> MessageRow {
    let from = parse_from(&meta.from_raw);
    let has = |l: &str| meta.label_ids.iter().any(|x| x.as_str() == l);
    MessageRow {
        id: meta.id.clone(),
        internal_date: meta.internal_date_ms,
        from_name: from.name.unwrap_or_default(),
        from_addr: from.addr,
        subject: meta.subject.clone(),
        snippet: snippet_text(&meta.snippet_raw),
        unread: has("UNREAD"),
        starred: has("STARRED"),
        important: has("IMPORTANT"),
        attachment_hint: meta.mime_type == "multipart/mixed",
        size: meta.size_estimate,
    }
}

#[cfg(test)]
mod tests {
    use mail_model::{HistoryId, LabelId, MessageId, ThreadId};

    use super::*;
    use crate::parse::{Part, Parsed};

    #[test]
    fn snippet_entities() {
        assert_eq!(snippet_text("it&#39;s &amp; that &lt;b&gt; &quot;x&quot; &#x41; &nbsp;"), "it's & that <b> \"x\" A \u{a0}");
        assert_eq!(snippet_text("a & b; c &unknown; &#; &"), "a & b; c &unknown; &#; &");
        assert_eq!(snippet_text("plain"), "plain");
        assert_eq!(snippet_text("&amp;amp;"), "&amp;");
        assert_eq!(snippet_text("caf\u{e9} &hellip;"), "caf\u{e9} \u{2026}");
    }

    #[test]
    fn from_shapes() {
        let a = parse_from("\"Doe, Jane\" <jane@example.com>, other@example.com");
        assert_eq!(a, Addr { name: Some("Doe, Jane".into()), addr: "jane@example.com".into() });
        let b = parse_from("=?UTF-8?B?w6RiYw==?= <abc@example.com>");
        assert_eq!(b.name.as_deref(), Some("\u{e4}bc"));
        assert_eq!(parse_from("bare@example.com"), Addr { name: None, addr: "bare@example.com".into() });
        assert_eq!(parse_from("<only@example.com>"), Addr { name: None, addr: "only@example.com".into() });
        let e = parse_from("\"=?UTF-8?Q?caf=C3=A9?=\" <x@example.com>");
        assert_eq!(e.name.as_deref(), Some("caf\u{e9}"));
        let l = parse_from("=?iso-8859-1?Q?caf=E9?= <x@example.com>");
        assert_eq!(l.name.as_deref(), Some("caf\u{e9}"));
        assert_eq!(parse_from("Plain Name <p@example.com>").name.as_deref(), Some("Plain Name"));
        assert_eq!(parse_from("  "), Addr { name: None, addr: String::new() });
        assert_eq!(parse_from(" weird text "), Addr { name: None, addr: "weird text".into() });
    }

    fn meta(labels: &[&str], mime: &str) -> MessageMeta {
        MessageMeta {
            id: MessageId::new("m1"),
            thread_id: ThreadId::new("t1"),
            label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
            internal_date_ms: 1_700_000_000_000,
            size_estimate: 1234,
            history_id: HistoryId(5),
            mime_type: mime.into(),
            from_raw: "Alice <alice@example.com>".into(),
            to_raw: "me@example.org".into(),
            subject: "Hello".into(),
            date_raw: "Tue, 14 Nov 2023 22:13:20 +0000".into(),
            snippet_raw: "Don&#39;t panic".into(),
        }
    }

    #[test]
    fn row_flags_and_fields() {
        let r = message_row(&meta(&["INBOX", "UNREAD", "IMPORTANT"], "multipart/mixed"));
        assert!(r.unread && r.important && !r.starred && r.attachment_hint);
        assert_eq!((r.from_name.as_str(), r.from_addr.as_str()), ("Alice", "alice@example.com"));
        assert_eq!(r.snippet, "Don't panic");
        assert_eq!((r.internal_date, r.size, r.subject.as_str()), (1_700_000_000_000, 1234, "Hello"));
        let r = message_row(&meta(&["STARRED"], "multipart/alternative"));
        assert!(r.starred && !r.unread && !r.important && !r.attachment_hint);
        let mut m = meta(&[], "text/plain");
        m.from_raw = "no-name@example.com".into();
        let r = message_row(&m);
        assert_eq!((r.from_name.as_str(), r.from_addr.as_str()), ("", "no-name@example.com"));
    }

    #[test]
    fn auth_levels_and_trust() {
        let mut p = Parsed::default();
        assert_eq!(auth_summary(&p).level, AuthLevel::None);
        p.authentication_results = vec![
            "evil.example.net; dkim=pass header.d=bank.com; spf=pass smtp.mailfrom=bank.com; dmarc=pass".into(),
            "mx.google.com; spf=softfail smtp.mailfrom=x@c.net; dkim=pass header.d=b.org; dmarc=fail".into(),
        ];
        let a = auth_summary(&p);
        assert_eq!(a.level, AuthLevel::Bad);
        assert_eq!(a.signed_by.as_deref(), Some("b.org"));
        p.authentication_results = vec!["mx.google.com; spf=pass smtp.mailfrom=a@b.c; dkim=none".into()];
        assert_eq!(auth_summary(&p).level, AuthLevel::Mixed);
        p.authentication_results = vec!["evil.example.net; spf=pass".into()];
        assert_eq!(auth_summary(&p).level, AuthLevel::None);
    }

    #[test]
    fn header_block_defaults_and_attachment_names() {
        let h = header_block(&Parsed::default());
        assert_eq!(h.from.addr, "");
        assert_eq!((h.subject.as_str(), h.date_ms, h.reply_to), ("", None, None));
        let a = |s: &str| Addr { name: None, addr: s.into() };
        let p = Parsed { from: vec![a("A@x.com")], reply_to: vec![a("a@x.com")], ..Default::default() };
        assert!(header_block(&p).reply_to.is_none());
        let part = |id: u32, name: Option<&str>, inline: bool| Part {
            id: mail_model::PartId(id),
            mime: "application/octet-stream".into(),
            file_name: name.map(str::to_string),
            content_id: None,
            data: vec![0; 3],
            inline_referenced: inline,
        };
        let p = Parsed { parts: vec![part(2, None, false), part(3, Some("x.bin"), false), part(4, Some("i.png"), true)], ..Default::default() };
        let l = attachments(&p);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].name.as_str(), l[0].size), ("attachment", 3));
        assert_eq!(l[1].name, "x.bin");
    }
}
