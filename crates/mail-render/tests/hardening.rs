//! WP-K1: property tests (no panics, output invariants) and size / nesting guards. Synthetic input only.

use mail_model::{AccountId, MessageId};
use mail_render::{ImagePolicy, Parsed, fixtures, parse, parse_asset_path, render};
use proptest::prelude::*;

fn acct() -> AccountId {
    AccountId::new("a1")
}
fn mid() -> MessageId {
    MessageId::new("m1")
}

const OWN_BASE: &str = "<base target=\"_blank\">";

/// Split `<...>` tags out of the body and return `(tag_name_lowercase, [(attr_lowercase, value)])`.
/// Sanitized output never contains a literal `<` outside tags (text is escaped), so every `<` starts a tag.
fn tags(html: &str) -> Vec<(String, Vec<(String, String)>)> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        i += 1;
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let name = html[start..i].to_ascii_lowercase();
        let mut attrs = Vec::new();
        loop {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
                i += 1;
            }
            if i >= b.len() || b[i] == b'>' {
                break;
            }
            let s = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
                i += 1;
            }
            let an = html[s..i].to_ascii_lowercase();
            let mut val = String::new();
            if i < b.len() && b[i] == b'=' {
                i += 1;
                if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                    let q = b[i];
                    i += 1;
                    let vs = i;
                    while i < b.len() && b[i] != q {
                        i += 1;
                    }
                    val = html[vs..i].to_string();
                    i += 1;
                } else {
                    let vs = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    val = html[vs..i].to_string();
                }
            }
            if an.is_empty() {
                i += 1; // never loop forever on junk
            } else {
                attrs.push((an, val));
            }
        }
        out.push((name, attrs));
    }
    out
}

fn has_remote_css_url(css: &str) -> bool {
    let l = css.to_ascii_lowercase();
    let mut rest = l.as_str();
    while let Some(p) = rest.find("url(") {
        let after = rest[p + 4..].trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'');
        if after.starts_with("http://") || after.starts_with("https://") || after.starts_with("//") {
            return true;
        }
        rest = &rest[p + 4..];
    }
    false
}

fn assert_block_page(page: &str) {
    // CSP meta is the first thing in the head after the charset.
    assert!(page.starts_with("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n<meta http-equiv=\"Content-Security-Policy\""));
    let lower = page.to_ascii_lowercase();
    assert!(!lower.contains("<script"), "script");
    for bad in ["<iframe", "<form", "<svg", "<object", "<embed", "<frame", "<meta http-equiv=\"refresh"] {
        // The only legitimate meta http-equiv is our CSP.
        if bad.starts_with("<meta") {
            continue;
        }
        assert!(!lower.contains(bad), "{bad}");
    }
    assert_eq!(page.matches(OWN_BASE).count(), 1, "own base exactly once");
    assert!(!page.replacen(OWN_BASE, "", 1).to_ascii_lowercase().contains("<base"), "foreign base");
    let body_at = page.find("<body>").expect("body");
    let body = &page[body_at..];
    for (name, attrs) in tags(body) {
        assert!(!matches!(name.as_str(), "script" | "iframe" | "form" | "svg" | "object" | "embed" | "base" | "meta" | "link"), "tag {name}");
        for (a, v) in attrs {
            assert!(!a.starts_with("on"), "event attr {a}");
            let vl: String = v.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect::<String>().to_ascii_lowercase();
            assert!(!vl.starts_with("javascript:") && !vl.starts_with("vbscript:"), "js url in {a}");
            let vl = vl.replace("http://gmail-asset.localhost/", "asset:");
            if matches!(a.as_str(), "src" | "background" | "srcset" | "poster" | "data" | "action" | "formaction") {
                assert!(!vl.starts_with("http:") && !vl.starts_with("https:") && !vl.starts_with("//"), "remote {a}={v}");
            }
            if a == "style" {
                assert!(!has_remote_css_url(&v), "remote url() in style: {v}");
            }
        }
    }
    // <style> block contents.
    let lb = body.to_ascii_lowercase();
    let mut rest = lb.as_str();
    while let Some(p) = rest.find("<style") {
        let end = rest[p..].find("</style>").map_or(rest.len(), |e| p + e);
        assert!(!has_remote_css_url(&rest[p..end]), "remote url() in <style>");
        rest = &rest[end..];
    }
}

fn check_all(raw: &[u8]) {
    let Ok(p) = parse(raw) else { return };
    let allow = render(&p, &acct(), &mid(), ImagePolicy::Allow);
    assert!(allow.page.starts_with("<!doctype html>"));
    assert!(allow.page.contains("Content-Security-Policy"));
    let block = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert_block_page(&block.page);
}

fn email_with_html(html: &str) -> Vec<u8> {
    let mut v = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: t\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n".to_vec();
    v.extend_from_slice(html.as_bytes());
    v
}

const SNIPPETS: &[&str] = &[
    "<script>alert(1)</script>", "<SCRIPT src=//evil/x.js>", "<img src=http://evil/a.png>", "<img src=\"HTTPS://evil/a.png\">",
    "<img src=' java\tscript:alert(1)'>", "<a href=\"javascript:alert(1)\">x</a>", "<a href=\"  JaVa&#x73;cript:1\">x</a>",
    "<div onclick=\"x()\" onmouseover=y>t</div>", "<body onload=x background=http://evil/b.png>", "<iframe src=http://evil></iframe>",
    "<form action=http://evil><input name=a></form>", "<svg onload=alert(1)><circle/></svg>", "<object data=http://evil></object>",
    "<embed src=http://evil>", "<base href=http://evil/>", "<meta http-equiv=refresh content=\"0;url=http://evil\">",
    "<link rel=stylesheet href=http://evil/c.css>", "<style>@import 'http://evil/x.css'; p{background:url(http://evil/b.png)}</style>",
    "<style>p{background:url( \"//evil/b.png\" )}</style>", "<p style=\"background:url(https://evil/x);color:red\">s</p>",
    "<p style=\"background:u\\72l(http://evil/x)\">s</p>", "<table background=\"http://evil/t.png\"><tr><td>c</td></tr></table>",
    "<img srcset=\"http://evil/a.png 1x\">", "<video poster=http://evil/p.png></video>", "<img src=cid:logo@shop>", "<img src=data:image/png;base64,AAAA>",
    "<math><mtext><style><img src=x onerror=1>", "<noscript><p title=\"</noscript><img src=http://evil onerror=1>\">", "<!-- <script> -->",
    "<![CDATA[<script>]]>", "</div></div></table>", "<div", "<a href=", "<p style=\"", "&lt;script&gt;", "&#0;&#x110000;", "plain text ",
    "<b>bold</b>", "<td>", "<select><option>1</select>", "<textarea><script></textarea>", "<xmp><script></xmp>", "<plaintext><script>",
];

fn html_soup() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        3 => proptest::sample::select(SNIPPETS).prop_map(str::to_string),
        1 => "[ -~]{0,12}".prop_map(|s: String| s),
        1 => any::<String>().prop_map(|s| s.chars().take(8).collect()),
    ];
    proptest::collection::vec(piece, 0..20).prop_map(|v| v.concat())
}

fn mutated_fixture() -> impl Strategy<Value = Vec<u8>> {
    let name = proptest::sample::select(fixtures::SYNTHETIC);
    (name, proptest::collection::vec((any::<u16>(), any::<u8>(), 0u8..3), 0..12), proptest::option::of(any::<u16>())).prop_map(
        |(n, edits, trunc)| {
            let mut raw = fixtures::synthetic(n).expect("fixture");
            for (pos, byte, kind) in edits {
                if raw.is_empty() {
                    break;
                }
                let at = usize::from(pos) % raw.len();
                match kind {
                    0 => raw[at] = byte,
                    1 => raw.insert(at, byte),
                    _ => {
                        raw.remove(at);
                    }
                }
            }
            if let Some(t) = trunc {
                let keep = usize::from(t) % (raw.len() + 1);
                raw.truncate(keep);
            }
            raw
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic(raw in proptest::collection::vec(any::<u8>(), 0..2048)) {
        check_all(&raw);
    }

    #[test]
    fn mutated_fixtures_never_panic_and_stay_safe(raw in mutated_fixture()) {
        check_all(&raw);
    }

    #[test]
    fn hostile_html_soup_is_neutralised(html in html_soup()) {
        let raw = email_with_html(&html);
        let p = parse(&raw).expect("well-formed envelope");
        let block = render(&p, &acct(), &mid(), ImagePolicy::Block);
        assert_block_page(&block.page);
        let allow = render(&p, &acct(), &mid(), ImagePolicy::Allow);
        let lower = allow.page.to_ascii_lowercase();
        prop_assert!(!lower.contains("<script") && !lower.contains("<iframe") && !lower.contains("<form"));
    }

    #[test]
    fn hand_built_parsed_never_panics(html in proptest::option::of(any::<String>()), text in proptest::option::of(any::<String>())) {
        let p = Parsed { html, text, ..Default::default() };
        for pol in [ImagePolicy::Block, ImagePolicy::Allow] {
            let r = render(&p, &acct(), &mid(), pol);
            prop_assert!(r.page.starts_with("<!doctype html>"));
        }
        assert_block_page(&render(&p, &acct(), &mid(), ImagePolicy::Block).page);
    }

    #[test]
    fn asset_path_never_escapes(path in prop_oneof![any::<String>(), "[a-zA-Z0-9_.@+/\\\\%-]{0,40}"]) {
        if let Some((a, m, p)) = parse_asset_path(&path) {
            for s in [a.as_str(), m.as_str()] {
                prop_assert!(!s.is_empty());
                prop_assert!(!s.contains('/') && !s.contains('\\') && !s.starts_with('.'));
            }
            let _ = p;
        }
    }
}

#[test]
fn asset_path_rejects_dot_and_slash_forms() {
    for bad in ["/../m/1", "/./m/1", "/a/../1", "/a/m/..", "/a\\b/m/1", "/a/m\\x/1", "/.a/m/1", "/a/.m/1", "/a/m/1/", "////"] {
        assert!(parse_asset_path(bad).is_none(), "{bad}");
    }
}

fn plain_email(text: &str) -> Vec<u8> {
    let mut v = b"From: a@example.com\r\nSubject: t\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n".to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}

#[test]
fn oversized_html_falls_back_to_notice() {
    let html = format!("<p>{}</p>", "x".repeat(2 * 1024 * 1024 + 10));
    let p = parse(&email_with_html(&html)).expect("parse");
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.len() < 4096, "{}", r.page.len());
    assert!(r.page.contains("too large to display"));
    assert!(r.page.contains("Content-Security-Policy") && r.page.contains("background: #fff"));
    assert_block_page(&r.page);
    assert!(render(&p, &acct(), &mid(), ImagePolicy::Allow).page.contains("too large to display"));
}

#[test]
fn oversized_html_with_text_alternative_uses_text() {
    let big = "y".repeat(2 * 1024 * 1024 + 10);
    let raw = format!(
        "From: a@example.com\r\nSubject: t\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nthe small text body\r\n--b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>{big}</p>\r\n--b1--\r\n"
    );
    let p = parse(raw.as_bytes()).expect("parse");
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.len() < 8192);
    assert!(r.page.contains("<div class=\"plain\">the small text body"), "{}", &r.page[r.page.len().saturating_sub(300)..]);
    assert!(!r.page.contains("too large"));
    assert_block_page(&r.page);
}

#[test]
fn oversized_plain_text_falls_back_to_notice() {
    let p = parse(&plain_email(&"z".repeat(2 * 1024 * 1024 + 10))).expect("parse");
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.contains("too large to display") && r.page.len() < 4096);
}

#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    // Run on a deliberately small stack (the default test thread size) for several shapes.
    let shapes: Vec<String> = vec![
        format!("{}x{}", "<div>".repeat(10_000), "</div>".repeat(10_000)),
        format!("{}x", "<div>".repeat(100_000)),
        format!("{}x", "<b><i>".repeat(30_000)),
        format!("{}<td>x", "<table><tr>".repeat(10_000)),
        format!("{}x", "<span style=\"color:red\">".repeat(20_000)),
        format!("{}x", "<blockquote>".repeat(50_000)),
        format!("{}x", "<font>".repeat(50_000)),
    ];
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            for s in &shapes {
                let p = parse(&email_with_html(s)).expect("parse");
                for pol in [ImagePolicy::Block, ImagePolicy::Allow] {
                    let r = render(&p, &acct(), &mid(), pol);
                    assert!(r.page.starts_with("<!doctype html>"));
                    assert!(r.page.len() <= 2 * 1024 * 1024 + 4096);
                }
            }
        })
        .expect("spawn");
    h.join().expect("no panic / overflow");
}

#[test]
fn deep_nesting_is_fast_and_shows_notice_or_text() {
    let t = std::time::Instant::now();
    let p = parse(&email_with_html(&format!("{}x", "<div>".repeat(100_000)))).expect("parse");
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.contains("too large to display"));
    assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    // Ordinary nesting is untouched.
    let ok = parse(&email_with_html(&format!("{}x{}", "<div>".repeat(100), "</div>".repeat(100)))).expect("parse");
    assert!(render(&ok, &acct(), &mid(), ImagePolicy::Block).page.contains("<div><div>"));
}
