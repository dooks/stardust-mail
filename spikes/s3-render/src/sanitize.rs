//! Email HTML -> safe HTML fragment, with `ammonia` plus a strict attribute filter and a CSS scrubber.
//!
//! Layers (defence in depth; this module is only the first):
//!  1. `ammonia` allow-list of tags/attributes; everything else is unwrapped or (for `clean_content_tags`) dropped.
//!  2. `attribute_filter`: the ONLY place URLs can be rewritten. Its output is not re-checked by ammonia, so it is a
//!     strict allow-list (http/https/mailto/tel/#fragment for links; cid:, data:image and remote images for images).
//!  3. `scrub_css` for `style=""` attributes and `<style>` blocks (no @import, no @font-face, `url()` classified).
//!  4. The page CSP and the iframe sandbox (see `render.rs`).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const ASSET_HOST: &str = "gmail-asset.localhost";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePolicy {
    /// Remote image URLs are removed from the HTML (nothing is ever requested).
    Block,
    /// Remote `http(s)` images are kept; the page CSP must then allow them.
    Allow,
}

/// Maps a normalised content-id to a servable URL (asset URL or `data:` URI), or None if unknown.
pub type CidResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// `<img>`/`background` references to remote http(s) or `//host` URLs.
    pub remote_images: usize,
    /// Remote `url(...)` in CSS (attributes and `<style>`).
    pub remote_css_urls: usize,
    /// Remote images that look like tracking pixels (width or height <= 2). Only computed when images are allowed.
    pub tracking_pixels: usize,
    pub cid_images: usize,
    pub cid_missing: usize,
    pub data_images: usize,
    pub has_style_block: bool,
    pub has_dark_media: bool,
    pub bytes_in: usize,
    pub bytes_out: usize,
}

#[derive(Default)]
struct Counters {
    remote_images: AtomicUsize,
    remote_css: AtomicUsize,
    cid_ok: AtomicUsize,
    cid_missing: AtomicUsize,
    data: AtomicUsize,
}

struct Ctx {
    images: ImagePolicy,
    cid: CidResolver,
    c: Counters,
}

// ---------------------------------------------------------------------------------------------
// URL classification
// ---------------------------------------------------------------------------------------------

/// Scheme the way a browser computes it: strip leading C0/space, drop tab/CR/LF anywhere, lower-case.
pub fn scheme_of(v: &str) -> Option<String> {
    let cleaned: String = v
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .take(32)
        .collect();
    let (s, _) = cleaned.split_once(':')?;
    let mut chars = s.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then(|| s.to_ascii_lowercase())
}

#[derive(Debug, PartialEq, Eq)]
enum UrlClass {
    Cid(String),
    DataImage,
    Remote(String),
    OurAsset,
    Other,
}

fn strip_ws(v: &str) -> String {
    v.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect::<String>().trim_end().to_string()
}

/// Decode `%XX` escapes (cid references are sometimes URL-encoded, e.g. `%40` for `@`).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn classify(v: &str) -> UrlClass {
    let v = strip_ws(v);
    let lower = v.to_ascii_lowercase();
    match scheme_of(&v).as_deref() {
        Some("cid") => UrlClass::Cid(crate::parse::norm_cid(&percent_decode(&v[4..]))),
        Some("data") => {
            let ok = ["png", "jpeg", "jpg", "gif", "webp", "bmp", "avif", "x-icon", "vnd.microsoft.icon"].iter().any(|t| {
                lower.starts_with(&format!("data:image/{t};base64,")) || lower.starts_with(&format!("data:image/{t},"))
            });
            if ok { UrlClass::DataImage } else { UrlClass::Other }
        }
        Some("http" | "https") => {
            let host = lower.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
            if host == ASSET_HOST || host.starts_with(&format!("{ASSET_HOST}:")) {
                UrlClass::OurAsset
            } else {
                UrlClass::Remote(v)
            }
        }
        None if v.starts_with("//") => UrlClass::Remote(format!("https:{v}")),
        _ => UrlClass::Other,
    }
}

/// Decide an image URL (`img src`, `background=`, CSS `url()`); None = drop it.
fn image_url(v: &str, cx: &Ctx, css: bool) -> Option<String> {
    match classify(v) {
        UrlClass::Cid(id) => match (cx.cid)(&id) {
            Some(u) => {
                cx.c.cid_ok.fetch_add(1, Ordering::Relaxed);
                Some(u)
            }
            None => {
                cx.c.cid_missing.fetch_add(1, Ordering::Relaxed);
                None
            }
        },
        UrlClass::DataImage => {
            cx.c.data.fetch_add(1, Ordering::Relaxed);
            Some(strip_ws(v))
        }
        UrlClass::Remote(u) => {
            let counter = if css { &cx.c.remote_css } else { &cx.c.remote_images };
            counter.fetch_add(1, Ordering::Relaxed);
            (cx.images == ImagePolicy::Allow).then_some(u)
        }
        UrlClass::OurAsset | UrlClass::Other => None,
    }
}

// ---------------------------------------------------------------------------------------------
// CSS scrubbing
// ---------------------------------------------------------------------------------------------

/// Find `needle` (already lower-case) in `lower` at or after `from`.
fn find_from(lower: &str, needle: &str, from: usize) -> Option<usize> {
    lower.get(from..)?.find(needle).map(|p| p + from)
}

/// Remove comments, `@import`, `@font-face` blocks and legacy script hooks; classify every `url()`.
fn scrub_css(css: &str, cx: &Ctx) -> String {
    // Backslash escapes can spell `url(` or `expression(`; email CSS has no real use for them.
    let mut s: String = css.chars().filter(|&c| c != '\\' && c != '\0').collect();
    // Comments.
    while let Some(a) = s.find("/*") {
        let end = s[a + 2..].find("*/").map_or(s.len(), |e| a + 2 + e + 2);
        s.replace_range(a..end, " ");
    }
    // @import ...; and @font-face { ... }
    loop {
        let lower = s.to_ascii_lowercase();
        if let Some(a) = lower.find("@import") {
            let end = lower[a..].find([';', '\n']).map_or(s.len(), |e| a + e + 1);
            s.replace_range(a..end, " ");
            continue;
        }
        if let Some(a) = lower.find("@font-face") {
            let end = match lower[a..].find('{') {
                Some(open) => {
                    let mut depth = 0usize;
                    let mut end = s.len();
                    for (i, c) in lower[a + open..].char_indices() {
                        match c {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = a + open + i + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    end
                }
                None => s.len(),
            };
            s.replace_range(a..end, " ");
            continue;
        }
        break;
    }
    for bad in ["expression(", "-moz-binding", "behavior:", "javascript:", "vbscript:"] {
        loop {
            let lower = s.to_ascii_lowercase();
            let Some(a) = lower.find(bad) else { break };
            s.replace_range(a..a + bad.len(), "x-removed");
        }
    }
    // url(...)
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    let lower = s.to_ascii_lowercase();
    while let Some(a) = find_from(&lower, "url(", pos) {
        out.push_str(&s[pos..a]);
        let inner_start = a + 4;
        let rest = &s[inner_start..];
        let trimmed = rest.trim_start();
        let lead = rest.len() - trimmed.len();
        let (value, after) = match trimmed.chars().next() {
            Some(q @ ('"' | '\'')) => match trimmed[1..].find(q) {
                Some(e) => (&trimmed[1..1 + e], inner_start + lead + 1 + e + 1),
                None => ("", s.len()),
            },
            _ => match trimmed.find(')') {
                Some(e) => (trimmed[..e].trim(), inner_start + lead + e),
                None => ("", s.len()),
            },
        };
        let close = s[after.min(s.len())..].find(')').map_or(s.len(), |e| after + e + 1);
        match image_url(value, cx, true) {
            Some(u) => {
                out.push_str("url(\"");
                out.push_str(&u.replace(['"', '\\', '\n', '\r', '(', ')'], ""));
                out.push_str("\")");
            }
            None => out.push_str("url(\"about:blank\")"),
        }
        pos = close;
    }
    out.push_str(&s[pos.min(s.len())..]);
    out
}

/// Scrub the text of every `<style>` element in already-sanitized output.
fn scrub_style_blocks(html: &str, cx: &Ctx) -> (String, bool, bool) {
    let lower = html.to_ascii_lowercase();
    let (mut out, mut pos, mut any, mut dark) = (String::with_capacity(html.len()), 0, false, false);
    while let Some(a) = find_from(&lower, "<style", pos) {
        let Some(gt) = find_from(&lower, ">", a) else { break };
        let end = find_from(&lower, "</style", gt).unwrap_or(html.len());
        out.push_str(&html[pos..=gt]);
        let css = &html[gt + 1..end];
        dark |= css.to_ascii_lowercase().contains("prefers-color-scheme");
        out.push_str(&scrub_css(css, cx));
        pos = end;
        any = true;
    }
    out.push_str(&html[pos..]);
    (out, any, dark)
}

// ---------------------------------------------------------------------------------------------
// ammonia configuration
// ---------------------------------------------------------------------------------------------

const TAGS: &[&str] = &[
    "a", "abbr", "acronym", "address", "article", "aside", "b", "bdi", "bdo", "big", "blockquote", "br", "caption",
    "center", "cite", "code", "col", "colgroup", "dd", "del", "dfn", "div", "dl", "dt", "em", "figcaption", "figure",
    "font", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "i", "img", "ins", "kbd", "li", "main",
    "mark", "nav", "ol", "p", "pre", "q", "s", "samp", "section", "small", "span", "strike", "strong", "style", "sub",
    "summary", "details", "sup", "table", "tbody", "td", "tfoot", "th", "thead", "time", "tr", "tt", "u", "ul", "var",
    "wbr",
];

/// Elements removed together with their content (anything else not in `TAGS` is only unwrapped).
const DROP_CONTENT: &[&str] = &[
    "script", "noscript", "title", "head", "template", "iframe", "frame", "frameset", "object", "embed", "applet", "svg",
    "math", "video", "audio", "canvas", "textarea", "select", "option", "datalist", "map", "xmp", "noembed", "noframes",
];

const GENERIC_ATTRS: &[&str] = &[
    "class", "dir", "lang", "title", "style", "align", "valign", "bgcolor", "width", "height", "role", "id",
];

fn tag_attrs() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("a", vec!["href", "name"]),
        ("img", vec!["src", "alt", "border", "hspace", "vspace"]),
        ("table", vec!["border", "cellpadding", "cellspacing", "background", "summary", "frame", "rules"]),
        ("td", vec!["colspan", "rowspan", "nowrap", "background", "scope", "headers", "abbr"]),
        ("th", vec!["colspan", "rowspan", "nowrap", "background", "scope", "headers", "abbr"]),
        ("tr", vec!["background"]),
        ("col", vec!["span"]),
        ("colgroup", vec!["span"]),
        ("font", vec!["color", "face", "size"]),
        ("hr", vec!["noshade", "size"]),
        ("ol", vec!["start", "type", "reversed"]),
        ("ul", vec!["type"]),
        ("li", vec!["value", "type"]),
        ("time", vec!["datetime"]),
    ]
}

/// `<body ...>` attributes (background colour, text colour) matter in newsletters, but ammonia drops `<body>`.
/// Turn the first `<body>` into `<div class="gm-body">` and the last `</body>` into `</div>`.
fn lift_body(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    let start = loop {
        let Some(p) = find_from(&lower, "<body", from) else { return html.to_string() };
        match lower.as_bytes().get(p + 5) {
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') => break p,
            _ => from = p + 5,
        }
    };
    let mut out = String::with_capacity(html.len() + 32);
    out.push_str(&html[..start]);
    out.push_str("<div class=\"gm-body\"");
    out.push_str(&html[start + 5..]);
    match lower.rfind("</body") {
        Some(e) if e > start => {
            let off = out.len() - html.len() + e;
            let gt = out[off..].find('>').map_or(out.len(), |g| off + g + 1);
            out.replace_range(off..gt, "</div>");
        }
        _ => out.push_str("</div>"),
    }
    out
}

fn count_tracking_pixels(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let mut n = 0;
    let mut pos = 0;
    let dim = |tag: &str, name: &str| -> Option<u32> {
        let key = format!(" {name}=\"");
        let p = tag.find(&key)? + key.len();
        let v: String = tag[p..].chars().take_while(|c| c.is_ascii_digit()).collect();
        v.parse().ok()
    };
    while let Some(a) = find_from(&lower, "<img ", pos) {
        let end = find_from(&lower, ">", a).unwrap_or(lower.len());
        let tag = &lower[a..end];
        if tag.contains(" src=\"http") && (dim(tag, "width").is_some_and(|w| w <= 2) || dim(tag, "height").is_some_and(|h| h <= 2)) {
            n += 1;
        }
        pos = end;
    }
    n
}

/// Sanitize an email HTML body into a fragment (no `<html>`/`<head>`/`<body>`).
pub fn sanitize(html: &str, images: ImagePolicy, cid: CidResolver) -> (String, Report) {
    let cx = Arc::new(Ctx { images, cid, c: Counters::default() });
    let lifted = lift_body(html);

    let mut b = ammonia::Builder::empty();
    b.tags(TAGS.iter().copied().collect::<HashSet<_>>())
        .clean_content_tags(DROP_CONTENT.iter().copied().collect::<HashSet<_>>())
        .generic_attributes(GENERIC_ATTRS.iter().copied().collect::<HashSet<_>>())
        .generic_attribute_prefixes(["aria-"].into_iter().collect::<HashSet<_>>())
        .tag_attributes(
            tag_attrs()
                .into_iter()
                .map(|(t, a)| (t, a.into_iter().collect::<HashSet<_>>()))
                .collect::<HashMap<_, _>>(),
        )
        .url_schemes(["http", "https", "mailto", "tel", "cid", "data"].into_iter().collect::<HashSet<_>>())
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .set_tag_attribute_value("img", "referrerpolicy", "no-referrer");
    let cx2 = cx.clone();
    b.attribute_filter(move |_el, attr, value| -> Option<Cow<'_, str>> {
        match attr {
            "style" => Some(Cow::Owned(scrub_css(value, &cx2))),
            "href" => {
                let ok = match scheme_of(value).as_deref() {
                    Some("http" | "https" | "mailto" | "tel") => true,
                    None => strip_ws(value).starts_with('#'),
                    _ => false,
                };
                if ok {
                    Some(Cow::Owned(strip_ws(value)))
                } else {
                    None
                }
            }
            "src" | "background" => image_url(value, &cx2, false).map(Cow::Owned),
            _ => Some(Cow::Borrowed(value)),
        }
    });

    let cleaned = b.clean(&lifted).to_string();
    let (fragment, has_style_block, has_dark_media) = scrub_style_blocks(&cleaned, &cx);

    let report = Report {
        remote_images: cx.c.remote_images.load(Ordering::Relaxed),
        remote_css_urls: cx.c.remote_css.load(Ordering::Relaxed),
        tracking_pixels: if images == ImagePolicy::Allow { count_tracking_pixels(&fragment) } else { 0 },
        cid_images: cx.c.cid_ok.load(Ordering::Relaxed),
        cid_missing: cx.c.cid_missing.load(Ordering::Relaxed),
        data_images: cx.c.data.load(Ordering::Relaxed),
        has_style_block,
        has_dark_media,
        bytes_in: html.len(),
        bytes_out: fragment.len(),
    };
    (fragment, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cid() -> CidResolver {
        Arc::new(|_| None)
    }
    fn cid_map() -> CidResolver {
        Arc::new(|id| (id == "logo@x").then(|| "http://gmail-asset.localhost/a1/m1/3".to_string()))
    }
    fn block(h: &str) -> (String, Report) {
        sanitize(h, ImagePolicy::Block, no_cid())
    }

    #[test]
    fn scripts_handlers_forms_base_meta_are_gone() {
        let (h, _) = block(
            "<html><head><title>SECRET TITLE</title><base href=\"http://evil/\"><meta http-equiv=refresh content=\"0;url=http://evil\"><script>alert(1)</script></head>\
             <body onload=\"x()\"><p onclick=\"x()\" onmouseover=x()>hi</p><img src=x onerror=\"alert(1)\">\
             <form action=\"http://evil/\" method=post><input name=a value=b><button>Go</button></form>\
             <iframe src=\"http://evil/\">frame text</iframe><object data=x>obj</object><svg onload=x><text>svgtext</text></svg>\
             <noscript>enable js</noscript></body></html>",
        );
        let l = h.to_ascii_lowercase();
        for bad in ["<script", "onload", "onclick", "onerror", "onmouseover", "<form", "<input", "<base", "<meta", "<iframe", "<object", "<svg", "http://evil", "secret title", "svgtext", "enable js", "frame text", "action="] {
            assert!(!l.contains(bad), "leaked {bad}: {h}");
        }
        assert!(h.contains("hi") && h.contains("Go"));
    }

    #[test]
    fn dangerous_schemes_in_links_are_removed() {
        let (h, _) = block(
            "<a href=\"javascript:alert(1)\">a</a><a href=\" JaVa\tScRiPt:alert(1)\">b</a><a href=\"data:text/html,<script>1</script>\">c</a>\
             <a href=\"file:///c:/x\">d</a><a href=\"vbscript:x\">e</a><a href=\"relative/path\">f</a><a href=\"https://ok.example/p\">g</a>\
             <a href=\"mailto:a@b.c\">h</a><a href=\"#sec\">i</a>",
        );
        assert!(!h.to_ascii_lowercase().contains("script:") && !h.contains("data:") && !h.contains("file:") && !h.contains("relative/path"));
        assert!(h.contains("https://ok.example/p") && h.contains("mailto:a@b.c") && h.contains("href=\"#sec\""));
        assert!(h.contains("rel=\"noopener noreferrer\""));
    }

    #[test]
    fn layout_attributes_survive() {
        let (h, _) = block(
            "<table width=\"600\" bgcolor=\"#eee\" align=\"center\" cellpadding=\"0\" style=\"border-collapse:collapse\"><tr><td colspan=\"2\" valign=\"top\" style=\"padding:4px;color:#333\">x</td></tr></table>",
        );
        for keep in ["width=\"600\"", "bgcolor=\"#eee\"", "align=\"center\"", "cellpadding=\"0\"", "border-collapse:collapse", "colspan=\"2\"", "valign=\"top\"", "padding:4px"] {
            assert!(h.contains(keep), "lost {keep}: {h}");
        }
    }

    #[test]
    fn body_attributes_are_kept_on_a_wrapper() {
        let (h, _) = block("<html><body bgcolor=\"#f0f0f0\" style=\"background:#f0f0f0\"><p>x</p></body></html>");
        assert!(h.contains("class=\"gm-body\"") && h.contains("bgcolor=\"#f0f0f0\"") && h.contains("</div>"), "{h}");
    }

    #[test]
    fn remote_images_blocked_or_allowed() {
        let src = "<img src=\"https://t.example/p.gif\" width=\"1\" height=\"1\"><img src=\"//cdn.example/a.png\"><img src=\"http://gmail-asset.localhost/a1/other/2\">";
        let (h, r) = block(src);
        assert!(!h.contains("example") && !h.contains("gmail-asset"), "{h}");
        assert_eq!(r.remote_images, 2);
        let (h, r) = sanitize(src, ImagePolicy::Allow, no_cid());
        assert!(h.contains("https://t.example/p.gif") && h.contains("src=\"https://cdn.example/a.png\""), "{h}");
        assert!(!h.contains("gmail-asset"));
        assert_eq!(r.tracking_pixels, 1);
        assert!(h.contains("referrerpolicy=\"no-referrer\""));
    }

    #[test]
    fn cid_and_data_images() {
        let (h, r) = sanitize(
            "<img src=\"cid:%3Clogo@x%3E\"><img src=\"cid:logo@x\"><img src=\"cid:missing\"><img src=\"data:image/png;base64,AAAA\"><img src=\"data:image/svg+xml;base64,AAAA\"><img src=\"data:text/html;base64,AAAA\">",
            ImagePolicy::Block,
            cid_map(),
        );
        // The percent-encoded reference and the plain one both resolve; "missing" and the second unknown one do not.
        assert_eq!(r.cid_images, 2);
        assert_eq!(r.cid_missing, 1);
        assert_eq!(r.data_images, 1);
        assert!(h.contains("http://gmail-asset.localhost/a1/m1/3") && h.contains("data:image/png;base64,AAAA"));
        assert!(!h.contains("svg+xml") && !h.contains("text/html"));
    }

    #[test]
    fn css_is_scrubbed_in_attributes_and_blocks() {
        let (h, r) = block(
            "<style>@import url(http://evil/x.css); @font-face{font-family:x;src:url(http://evil/f.woff)} .a{background:url('https://evil/b.png') no-repeat;color:red} /* c */ .b{width:expression(alert(1))}</style>\
             <div style=\"background-image:url(http://evil/c.png);u\\rl(http://evil/d.png)\">x</div><td background=\"http://evil/e.png\">y</td>",
        );
        assert!(!h.contains("evil"), "{h}");
        assert!(!h.contains("@import") && !h.contains("@font-face") && !h.to_ascii_lowercase().contains("expression("));
        assert!(h.contains("color:red") && h.contains("about:blank"));
        assert!(r.remote_css_urls >= 3 && r.has_style_block);
    }

    #[test]
    fn dark_media_detected() {
        let (_, r) = block("<style>@media (prefers-color-scheme: dark){p{color:#fff}}</style><p>x</p>");
        assert!(r.has_dark_media);
    }

    #[test]
    fn style_block_cannot_break_out() {
        let (h, _) = block("<style>p{color:red}</style><p>a</p><style>/*</style><script>alert(1)</script>*/</style>");
        assert!(!h.to_ascii_lowercase().contains("<script"), "{h}");
    }

    #[test]
    fn css_url_with_parens_or_quotes_cannot_inject() {
        let cx = Ctx { images: ImagePolicy::Allow, cid: no_cid(), c: Counters::default() };
        let out = scrub_css("a{background:url(\"https://x.example/a.png);}</style><script>\")}", &cx);
        assert!(!out.contains("a.png);"), "{out}");
    }
}
