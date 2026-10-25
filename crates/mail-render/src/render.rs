//! Sanitize + page wrap + asset helpers (ports of spike S3 `sanitize.rs`, `text.rs`, `render.rs`).

use std::sync::Arc;

use mail_model::{AccountId, MessageId, PartId};

use crate::parse::{BodyItem, Parsed};
use crate::sanitize::{self, ASSET_HOST, CidResolver};
use crate::text;

/// What happens to remote images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePolicy {
    /// Remote image URLs (and CSS background URLs) are removed in the sanitizer: nothing is requested.
    Block,
    /// Remote http(s) images are kept and the page CSP gains `img-src ... https: http:` ("Show images").
    Allow,
}

/// A finished document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Complete HTML for `<iframe srcdoc>`: white background, dark text (also for plain text), the CSP `<meta>`
    /// (`default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action
    /// 'none'; base-uri 'none'`, widened by `Allow`), `<base target=_blank>` and `referrer no-referrer`.
    pub page: String,
    /// Number of remote images / CSS URLs removed because of `Block`.
    pub remote_images_blocked: u32,
}

/// CSP for the iframe page. Remote images only in the per-message "images allowed" variant.
fn csp(images: ImagePolicy) -> String {
    let img = match images {
        ImagePolicy::Block => format!("img-src http://{ASSET_HOST} data:"),
        ImagePolicy::Allow => format!("img-src http://{ASSET_HOST} data: https: http:"),
    };
    format!("default-src 'none'; style-src 'unsafe-inline'; {img}; form-action 'none'; base-uri 'none'")
}

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

/// Wrap a sanitized fragment in a complete document: CSP first, no scripts, own `<base target=_blank>`.
fn page(fragment: &str, images: ImagePolicy) -> String {
    let mut s = String::with_capacity(fragment.len() + 2048);
    s.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
    s.push_str(&format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">\n", csp(images)));
    s.push_str("<meta name=\"referrer\" content=\"no-referrer\">\n");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<base target=\"_blank\">\n<style>");
    s.push_str(BASE_CSS);
    s.push_str("</style></head>\n<body>\n");
    s.push_str(fragment);
    s.push_str("\n</body></html>\n");
    s
}

/// Sanitize (`ammonia` with S3's exact config) and wrap. `cid:` images become
/// `http://gmail-asset.localhost/<account>/<msg>/<part>`. Never fails: an unparseable body renders as empty.
pub fn render(parsed: &Parsed, account: &AccountId, msg: &MessageId, images: ImagePolicy) -> Rendered {
    let url_of = {
        let (account, msg) = (account.clone(), msg.clone());
        move |part: PartId| asset_url(&account, &msg, part)
    };
    let cids = parsed.cids.clone();
    let resolver: CidResolver = {
        let url_of = url_of.clone();
        Arc::new(move |id: &str| cids.get(id).map(|p| url_of(*p)))
    };

    let (fragment, blocked) = match (&parsed.html, &parsed.text) {
        (Some(h), _) => {
            let (f, r) = sanitize::sanitize(h, images, resolver);
            let blocked = if images == ImagePolicy::Block { r.remote_images + r.remote_css_urls } else { 0 };
            (f, blocked)
        }
        (None, Some(_)) => {
            let mut f = String::new();
            for item in &parsed.text_items {
                match item {
                    BodyItem::Text(t) => f.push_str(&text::text_to_html(t)),
                    BodyItem::Image(id) => {
                        f.push_str(&format!("<div class=\"plain\"><img src=\"{}\" alt=\"\"></div>", text::escape(&url_of(*id))));
                    }
                }
            }
            (f, 0)
        }
        (None, None) => ("<div class=\"plain\">(no displayable body)</div>".to_string(), 0),
    };
    Rendered { page: page(&fragment, images), remote_images_blocked: u32::try_from(blocked).unwrap_or(u32::MAX) }
}

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

/// `http://gmail-asset.localhost/<account>/<msg>/<part>` with each segment restricted to `[A-Za-z0-9_.@+-]`.
pub fn asset_url(account: &AccountId, msg: &MessageId, part: PartId) -> String {
    format!("http://{ASSET_HOST}/{}/{}/{}", enc_seg(account.as_str()), enc_seg(msg.as_str()), part.0)
}

/// Parse the path of a `gmail-asset` request (`/<account>/<msg>/<part>`). Strict charset and length limits;
/// `None` for anything else (dot-leading segments, extra segments, non-digit part).
pub fn parse_asset_path(path: &str) -> Option<(AccountId, MessageId, PartId)> {
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
    if !ok(a, 128) || !ok(m, 64) || p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((AccountId::new(a), MessageId::new(m), PartId(p.parse().ok()?)))
}

/// Mime types that may be shown inline: png, jpeg, gif, webp, bmp, avif, x-icon.
pub fn inline_safe_mime(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/avif"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
    )
}

/// One part for serving or saving: `(bytes, mime, file_name)`.
pub fn part(parsed: &Parsed, id: PartId) -> Option<(Vec<u8>, String, Option<String>)> {
    parsed.parts.iter().find(|p| p.id == id).map(|p| (p.data.clone(), p.mime.clone(), p.file_name.clone()))
}

/// Response headers for a served asset: inline-safe raster images get their `Content-Type`; anything else is
/// served as `application/octet-stream` with `Content-Disposition: attachment`. Always `X-Content-Type-Options:
/// nosniff` and a `Content-Security-Policy: sandbox` header.
pub fn asset_headers(mime: &str) -> Vec<(&'static str, String)> {
    let mut h = Vec::with_capacity(4);
    if inline_safe_mime(mime) {
        h.push(("Content-Type", mime.trim().to_ascii_lowercase()));
    } else {
        h.push(("Content-Type", "application/octet-stream".to_string()));
        h.push(("Content-Disposition", "attachment".to_string()));
    }
    h.push(("X-Content-Type-Options", "nosniff".to_string()));
    h.push(("Content-Security-Policy", "default-src 'none'; sandbox".to_string()));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> AccountId {
        AccountId::new("a1")
    }
    fn m() -> MessageId {
        MessageId::new("18f3a0c1d2e3f405")
    }

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
        let u = asset_url(&a(), &m(), PartId(7));
        assert_eq!(u, "http://gmail-asset.localhost/a1/18f3a0c1d2e3f405/7");
        assert_eq!(parse_asset_path("/a1/18f3a0c1d2e3f405/7"), Some((a(), m(), PartId(7))));
        for bad in [
            "/a1/m/7/x", "/a1/m", "/../m/7", "/a1/m/-1", "/a1/m/x", "/a1/%2e%2e/1", "/a1/m/1234567890", "//m/1",
            "/a1/.hidden/1", "/a1/m/", "",
        ] {
            assert!(parse_asset_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn only_raster_images_are_inline_safe() {
        assert!(inline_safe_mime("image/png"));
        assert!(inline_safe_mime("image/x-icon"));
        assert!(!inline_safe_mime("image/svg+xml"));
        assert!(!inline_safe_mime("text/html"));
    }

    #[test]
    fn asset_headers_inline_vs_download() {
        let h = asset_headers("image/png");
        assert!(h.contains(&("Content-Type", "image/png".to_string())));
        assert!(!h.iter().any(|(k, _)| *k == "Content-Disposition"));
        assert!(h.contains(&("X-Content-Type-Options", "nosniff".to_string())));
        assert!(h.contains(&("Content-Security-Policy", "default-src 'none'; sandbox".to_string())));
        for mime in ["image/svg+xml", "text/html", "application/pdf"] {
            let h = asset_headers(mime);
            assert!(h.contains(&("Content-Type", "application/octet-stream".to_string())), "{mime}");
            assert!(h.contains(&("Content-Disposition", "attachment".to_string())), "{mime}");
            assert!(h.contains(&("X-Content-Type-Options", "nosniff".to_string())), "{mime}");
        }
    }

    #[test]
    fn page_is_white_with_csp_first() {
        let p = Parsed { html: Some("<p>hi</p>".into()), ..Default::default() };
        let r = render(&p, &a(), &m(), ImagePolicy::Block);
        let csp_at = r.page.find("Content-Security-Policy").expect("csp");
        assert!(csp_at < r.page.find("<p>hi</p>").expect("body"));
        assert!(r.page.contains("default-src 'none'") && r.page.contains("<base target=\"_blank\">"));
        assert!(r.page.contains("name=\"referrer\" content=\"no-referrer\""));
        assert!(r.page.contains("background: #fff") && !r.page.contains("invert") && !r.page.contains("#202124;\n  background"));
        assert!(!r.page.to_ascii_lowercase().contains("<script"));
        let t = Parsed { text: Some("plain".into()), text_items: vec![BodyItem::Text("plain".into())], ..Default::default() };
        let r = render(&t, &a(), &m(), ImagePolicy::Block);
        assert!(r.page.contains("<div class=\"plain\">plain</div>"));
        assert!(!r.page.contains("dark-text"));
    }
}
