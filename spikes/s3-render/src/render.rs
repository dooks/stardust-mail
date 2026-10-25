//! Parsed message -> (metadata view, sandboxed page for `<iframe srcdoc>`).

use std::sync::Arc;

use serde::Serialize;

use crate::parse::{self, AttachmentInfo, BodyItem, BodyKind, Parsed};
use crate::sample::Meta;
use crate::sanitize::{self, ASSET_HOST, CidResolver, ImagePolicy, Report};
use crate::text;

/// CSP for the iframe page. Remote images only in the per-message "images allowed" variant.
pub fn csp(images: ImagePolicy) -> String {
    let img = match images {
        ImagePolicy::Block => format!("img-src http://{ASSET_HOST} data:"),
        ImagePolicy::Allow => format!("img-src http://{ASSET_HOST} data: https: http:"),
    };
    format!("default-src 'none'; style-src 'unsafe-inline'; {img}; form-action 'none'; base-uri 'none'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DarkPolicy {
    /// Everything on white.
    Light,
    /// Our generated plain-text view follows the dark theme; email HTML stays on white (recommended).
    HtmlWhite,
    /// Plain text dark, and email HTML inverted with `filter: invert(1) hue-rotate(180deg)` (images re-inverted).
    HtmlInvert,
}

#[derive(Debug, Clone)]
pub struct RenderOpts {
    pub account: String,
    pub msg_id: String,
    pub images: ImagePolicy,
    pub dark: DarkPolicy,
}

#[derive(Debug, Clone)]
pub struct Rendered {
    pub page: String,
    pub body: BodyKind,
    pub report: Report,
}

// ---------------------------------------------------------------------------------------------
// Asset URLs
// ---------------------------------------------------------------------------------------------

fn enc_seg(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b'+') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub fn asset_url(account: &str, msg_id: &str, part_id: u32) -> String {
    format!("http://{ASSET_HOST}/{}/{}/{part_id}", enc_seg(account), enc_seg(msg_id))
}

/// Parse the path of an asset request (`/<account>/<msgId>/<partId>`). Strict charset; nothing is taken from mail content.
pub fn parse_asset_path(path: &str) -> Option<(String, String, u32)> {
    let mut it = path.trim_start_matches('/').split('/');
    let (a, m, p) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let ok = |s: &str, max: usize| {
        !s.is_empty()
            && s.len() <= max
            && !s.starts_with('.')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b'+'))
    };
    // Decode the only escapes we generate ('%40' never occurs because '@' is kept); reject anything else.
    if !ok(a, 128) || !ok(m, 64) || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((a.to_string(), m.to_string(), p.parse().ok()?))
}

/// Mime types that may be shown inline by the asset protocol; everything else is served as a download.
pub fn inline_safe_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" | "image/avif" | "image/x-icon" | "image/vnd.microsoft.icon")
}

// ---------------------------------------------------------------------------------------------
// Page wrapper
// ---------------------------------------------------------------------------------------------

const BASE_CSS: &str = r#"
html { background: #fff; color: #202124; color-scheme: light; }
body { margin: 0; padding: 12px 16px; font: 14px/1.5 system-ui, "Segoe UI", Roboto, Arial, sans-serif; overflow-wrap: break-word; }
img { max-width: 100%; height: auto; }
img:not([src]) { display: inline-block; min-width: 12px; min-height: 12px; outline: 1px dashed #c4c7c5; color: #80868b; font-size: 12px; }
a { color: #1a0dab; }
table { border-collapse: separate; }
.plain { white-space: pre-wrap; overflow-wrap: anywhere; }
.plain .q { border-left: 3px solid #dadce0; padding-left: 8px; margin: 4px 0; color: #5f6368; }
"#;

const DARK_TEXT_CSS: &str = r#"
html.dark-text { background: #202124; color: #e8eaed; color-scheme: dark; }
html.dark-text a { color: #8ab4f8; }
html.dark-text .plain .q { border-left-color: #5f6368; color: #9aa0a6; }
"#;

const INVERT_CSS: &str = r#"
html.invert { background: #fff; filter: invert(1) hue-rotate(180deg); }
html.invert img { filter: invert(1) hue-rotate(180deg); }
"#;

/// Wrap a sanitized fragment in a complete document: CSP first, no scripts, own `<base target=_blank>`.
pub fn page(fragment: &str, images: ImagePolicy, html_class: &str) -> String {
    let mut s = String::with_capacity(fragment.len() + 2048);
    s.push_str("<!doctype html>\n<html");
    if !html_class.is_empty() {
        s.push_str(&format!(" class=\"{html_class}\""));
    }
    s.push_str("><head><meta charset=\"utf-8\">\n");
    s.push_str(&format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">\n", csp(images)));
    s.push_str("<meta name=\"referrer\" content=\"no-referrer\">\n");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<base target=\"_blank\">\n<style>");
    s.push_str(BASE_CSS);
    s.push_str(DARK_TEXT_CSS);
    s.push_str(INVERT_CSS);
    s.push_str("</style></head>\n<body>\n");
    s.push_str(fragment);
    s.push_str("\n</body></html>\n");
    s
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// How `cid:` images are served.
#[derive(Clone)]
pub enum CidMode {
    /// `http://gmail-asset.localhost/<account>/<msgId>/<partId>` (production).
    Asset,
    /// `data:` URIs (for opening a page in a plain browser). Images above ~1.5 MB are skipped.
    Inline(Arc<Vec<u8>>),
}

pub fn render(p: &Parsed, opts: &RenderOpts, cid_mode: &CidMode) -> Rendered {
    let cids = p.cids.clone();
    let (account, msg_id) = (opts.account.clone(), opts.msg_id.clone());
    let mode = cid_mode.clone();
    let part_url: Arc<dyn Fn(u32) -> Option<String> + Send + Sync> = Arc::new(move |part: u32| {
        match &mode {
            CidMode::Asset => Some(asset_url(&account, &msg_id, part)),
            CidMode::Inline(raw) => {
                let (mime, bytes) = parse::part_bytes(raw, part)?;
                if !inline_safe_mime(&mime) || bytes.len() > 1_500_000 {
                    return None;
                }
                use base64::Engine;
                Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
            }
        }
    });
    let pu = part_url.clone();
    let resolver: CidResolver = Arc::new(move |id: &str| pu(*cids.get(id)?));

    let (fragment, report, body, class) = match (&p.html, &p.text) {
        (Some(h), _) => {
            let (f, r) = sanitize::sanitize(h, opts.images, resolver);
            let class = if opts.dark == DarkPolicy::HtmlInvert { "invert" } else { "" };
            (f, r, BodyKind::Html, class)
        }
        (None, Some(t)) => {
            let class = if opts.dark == DarkPolicy::Light { "" } else { "dark-text" };
            let _ = t;
            let mut f = String::new();
            for item in &p.text_items {
                match item {
                    BodyItem::Text(t) => f.push_str(&text::text_to_html(t)),
                    BodyItem::Image(id) => {
                        if let Some(u) = part_url(*id) {
                            f.push_str(&format!("<div class=\"plain\"><img src=\"{}\" alt=\"\"></div>", text::escape(&u)));
                        }
                    }
                }
            }
            (f, Report::default(), BodyKind::Text, class)
        }
        (None, None) => ("<div class=\"plain\">(no displayable body)</div>".to_string(), Report::default(), BodyKind::None, ""),
    };
    Rendered { page: page(&fragment, opts.images, class), body, report }
}

// ---------------------------------------------------------------------------------------------
// Metadata view
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AuthView {
    pub line: String,
    /// good | bad | mixed | unknown
    pub level: String,
    pub mailed_by: Option<String>,
    pub signed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttView {
    pub part_id: u32,
    pub name: String,
    pub mime: String,
    pub size: usize,
    pub size_human: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetaView {
    pub subject: String,
    pub from: Vec<String>,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub reply_to: Vec<String>,
    /// Shown only when Reply-To differs from From.
    pub reply_to_differs: bool,
    pub date_local: String,
    pub date_relative: String,
    pub date_source: String,
    pub labels: Vec<String>,
    pub auth: Option<AuthView>,
    pub attachments: Vec<AttView>,
}

pub fn human_size(n: usize) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else {
        format!("{:.1} MB", f / (K * K))
    }
}

pub fn relative(now_ts: i64, ts: i64) -> String {
    let d = now_ts - ts;
    if d < 0 {
        return "in the future".into();
    }
    match d {
        0..=44 => "just now".into(),
        45..=3_599 => format!("{} min ago", (d / 60).max(1)),
        3_600..=86_399 => format!("{} h ago", d / 3_600),
        86_400..=2_591_999 => format!("{} d ago", d / 86_400),
        _ => format!("{} mo ago", d / 2_592_000),
    }
}

pub fn local_date(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%a, %b %-d, %Y, %-I:%M %p").to_string()).unwrap_or_default()
}

fn label_name(id: &str) -> String {
    match id {
        "INBOX" => "Inbox".into(),
        "SENT" => "Sent".into(),
        "DRAFT" => "Drafts".into(),
        "SPAM" => "Spam".into(),
        "TRASH" => "Trash".into(),
        "STARRED" => "Starred".into(),
        "IMPORTANT" => "Important".into(),
        "UNREAD" => "Unread".into(),
        "CHAT" => "Chat".into(),
        x if x.starts_with("CATEGORY_") => {
            let c = x["CATEGORY_".len()..].to_ascii_lowercase();
            format!("{}{}", c[..1].to_ascii_uppercase(), &c[1..])
        }
        // User labels come as opaque ids; names need `labels.list` (S2).
        other => other.to_string(),
    }
}

pub fn meta_view(p: &Parsed, meta: &Meta, account: &str, now_ts: i64) -> MetaView {
    let (ts, source) = match (p.date_ts, meta.internal_date_ms) {
        (Some(t), _) => (Some(t), "Date header"),
        (None, ms) if ms > 0 => (Some((ms / 1000) as i64), "internalDate"),
        _ => (None, "none"),
    };
    let reply_to_differs = !p.reply_to.is_empty()
        && p.reply_to.iter().map(|a| a.address.to_ascii_lowercase()).collect::<Vec<_>>()
            != p.from.iter().map(|a| a.address.to_ascii_lowercase()).collect::<Vec<_>>();
    MetaView {
        subject: p.subject.clone().unwrap_or_default(),
        from: p.from.iter().map(|a| a.display()).collect(),
        from_name: p.from.first().and_then(|a| a.name.clone()),
        from_addr: p.from.first().map(|a| a.address.clone()),
        to: p.to.iter().map(|a| a.display()).collect(),
        cc: p.cc.iter().map(|a| a.display()).collect(),
        reply_to: p.reply_to.iter().map(|a| a.display()).collect(),
        reply_to_differs,
        date_local: ts.map(local_date).unwrap_or_default(),
        date_relative: ts.map(|t| relative(now_ts, t)).unwrap_or_default(),
        date_source: source.to_string(),
        labels: meta.label_ids.iter().map(|l| label_name(l)).collect(),
        auth: p.auth.as_ref().map(|a| AuthView {
            line: a.line(),
            level: a.level().to_string(),
            mailed_by: a.mailed_by.clone(),
            signed_by: a.signed_by.clone(),
        }),
        attachments: p
            .attachments
            .iter()
            .filter(|a| !a.inline_referenced)
            .map(|a| att_view(a, account, &meta.id))
            .collect(),
    }
}

fn att_view(a: &AttachmentInfo, account: &str, msg_id: &str) -> AttView {
    AttView {
        part_id: a.part_id,
        name: a.name.clone().unwrap_or_else(|| "(unnamed)".to_string()),
        mime: a.mime.clone(),
        size: a.size,
        size_human: human_size(a.size),
        url: asset_url(account, msg_id, a.part_id),
    }
}

/// Header block for the standalone `.view.html` page (all text escaped).
pub fn header_html(m: &MetaView) -> String {
    let e = text::escape;
    let list = |label: &str, v: &[String]| {
        if v.is_empty() { String::new() } else { format!("<div><b>{label}</b> {}</div>", e(&v.join(", "))) }
    };
    let mut s = format!("<h2>{}</h2>", e(&m.subject));
    s.push_str(&list("From", &m.from));
    s.push_str(&list("To", &m.to));
    s.push_str(&list("Cc", &m.cc));
    if m.reply_to_differs {
        s.push_str(&list("Reply-To", &m.reply_to));
    }
    s.push_str(&format!("<div><b>Date</b> {} ({}; from {})</div>", e(&m.date_local), e(&m.date_relative), e(&m.date_source)));
    s.push_str(&list("Labels", &m.labels));
    if let Some(a) = &m.auth {
        s.push_str(&format!(
            "<div class=\"auth {}\"><b>Sender</b> {}{}{}</div>",
            e(&a.level),
            e(&a.line),
            a.mailed_by.as_ref().map(|d| format!(" \u{b7} mailed-by {}", e(d))).unwrap_or_default(),
            a.signed_by.as_ref().map(|d| format!(" \u{b7} signed-by {}", e(d))).unwrap_or_default(),
        ));
    } else {
        s.push_str("<div class=\"auth unknown\"><b>Sender</b> no Authentication-Results from Gmail</div>");
    }
    if !m.attachments.is_empty() {
        let items: Vec<String> =
            m.attachments.iter().map(|a| format!("{} ({}, {})", e(&a.name), e(&a.mime), e(&a.size_human))).collect();
        s.push_str(&format!("<div><b>Attachments</b> {}</div>", items.join("; ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_strings() {
        assert_eq!(
            csp(ImagePolicy::Block),
            "default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action 'none'; base-uri 'none'"
        );
        assert!(csp(ImagePolicy::Allow).contains("img-src http://gmail-asset.localhost data: https: http:"));
    }

    #[test]
    fn asset_urls_roundtrip_and_reject_junk() {
        let u = asset_url("a1", "18f3a0c1d2e3f405", 7);
        assert_eq!(u, "http://gmail-asset.localhost/a1/18f3a0c1d2e3f405/7");
        assert_eq!(parse_asset_path("/a1/18f3a0c1d2e3f405/7"), Some(("a1".into(), "18f3a0c1d2e3f405".into(), 7)));
        for bad in ["/a1/m/7/x", "/a1/m", "/../m/7", "/a1/m/-1", "/a1/m/x", "/a1/%2e%2e/1", "/a1/m/1234567890", "//m/1", "/a1/.hidden/1"] {
            assert!(parse_asset_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative(1000, 990), "just now");
        assert_eq!(relative(10_000, 10_000 - 300), "5 min ago");
        assert_eq!(relative(100_000, 100_000 - 7_300), "2 h ago");
        assert_eq!(relative(1_000_000, 1_000_000 - 200_000), "2 d ago");
    }

    #[test]
    fn only_raster_images_are_inline_safe() {
        assert!(inline_safe_mime("image/png"));
        assert!(!inline_safe_mime("image/svg+xml"));
        assert!(!inline_safe_mime("text/html"));
    }
}
