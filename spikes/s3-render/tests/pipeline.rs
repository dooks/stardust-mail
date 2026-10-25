//! End-to-end tests on SYNTHETIC emails only (fixtures\*.eml and the hostile HTML).

use std::sync::Arc;

use s3render::parse::{self, BodyItem, BodyKind};
use s3render::render::{self, CidMode, DarkPolicy, RenderOpts};
use s3render::sanitize::{self, ImagePolicy};

fn eml(name: &str) -> Vec<u8> {
    let text = std::fs::read_to_string(format!("fixtures/{name}")).expect("fixture");
    text.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes()
}

fn opts(images: ImagePolicy) -> RenderOpts {
    RenderOpts { account: "a1".into(), msg_id: "m1".into(), images, dark: DarkPolicy::HtmlWhite }
}

#[test]
fn multipart_alternative_headers_and_auth() {
    let p = parse::parse(&eml("alt.eml")).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::Html);
    assert!(p.has_html_part && p.has_text_part);
    assert_eq!(p.from[0].name.as_deref(), Some("Alice Example"));
    assert_eq!(p.from[0].address, "alice@example.com");
    assert_eq!(p.to.len(), 2);
    assert_eq!(p.to[1].address, "carol@example.net");
    assert_eq!(p.cc[0].name.as_deref(), Some("Dave, D."));
    assert_eq!(p.reply_to[0].address, "replies@lists.example.com");
    assert_eq!(p.date_ts, Some(1_637_446_921)); // 2021-11-20T22:22:01Z
    let a = p.auth.expect("auth");
    assert_eq!(a.line(), "SPF pass \u{b7} DKIM pass (example.com) \u{b7} DMARC pass");
    assert_eq!(a.mailed_by.as_deref(), Some("mail.example.com"));
    assert_eq!(p.auth_header_count, 1);
    assert_eq!(p.structure, "multipart/alternative(text/plain,text/html)");
}

#[test]
fn related_cid_and_attachments() {
    let raw = eml("related_cid.eml");
    let p = parse::parse(&raw).expect("parse");
    assert_eq!(
        p.structure,
        "multipart/mixed(multipart/related(multipart/alternative(text/plain,text/html),image/png),application/pdf)"
    );
    let pdf = p.attachments.iter().find(|a| a.mime == "application/pdf").expect("pdf");
    assert_eq!(pdf.name.as_deref(), Some("r\u{e9}sum\u{e9}.pdf")); // RFC 2231 filename
    assert_eq!(pdf.size, 9);
    let logo = p.attachments.iter().find(|a| a.mime == "image/png").expect("logo listed by the parser");
    assert!(logo.inline_referenced, "the cid image is shown inline, so it is hidden from the attachment list");
    assert!(p.cids.contains_key("logo@shop"));

    let r = render::render(&p, &opts(ImagePolicy::Block), &CidMode::Asset);
    assert_eq!(r.report.cid_images, 1);
    assert_eq!(r.report.cid_missing, 1);
    let part = p.cids["logo@shop"];
    assert!(r.page.contains(&format!("http://gmail-asset.localhost/a1/m1/{part}")), "{}", r.page);
    assert!(!r.page.contains("nothere"));
    // Asset bytes come back byte-exact from the raw message.
    let (mime, bytes) = parse::part_bytes(&raw, part).expect("part");
    assert_eq!(mime, "image/png");
    assert_eq!(&bytes[..4], b"\x89PNG");

    let inline = render::render(&p, &opts(ImagePolicy::Block), &CidMode::Inline(Arc::new(raw)));
    assert!(inline.page.contains("src=\"data:image/png;base64,iVBOR"), "{}", inline.page);
}

#[test]
fn non_utf8_charsets_and_encoded_words() {
    let p = parse::parse(&eml("charsets.eml")).expect("parse");
    assert!(p.had_encoded_words);
    assert_eq!(p.from[0].name.as_deref(), Some("J\u{fc}rgen M\u{fc}ller"));
    assert_eq!(p.to[0].name.as_deref(), Some("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"));
    // Adjacent encoded words are joined without the folding whitespace between them.
    assert_eq!(p.subject.as_deref(), Some("Caf\u{e9} \u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"));
    let text = p.text.expect("text");
    assert!(text.contains("caf\u{e9} na\u{ef}ve joined"), "{text:?}");
    assert!(text.contains("\u{65e5}\u{672c}\u{8a9e}"), "{text:?}");
    assert!(p.charsets.contains(&"iso-8859-1".to_string()) && p.charsets.contains(&"shift_jis".to_string()));
    assert_eq!(p.charset_problems, 0);
}

#[test]
fn mislabeled_windows_1252_bytes_are_repaired() {
    // Quoted-printable: what mail-parser returns for the invalid byte 0x96 (labelled UTF-8) is a C1 control.
    let qp = b"From: a@example.com\r\nSubject: x\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nrange 1=962\r\n".to_vec();
    let t = parse::parse(&qp).expect("parse").text.expect("text");
    assert!(t.contains("1\u{2013}2"), "{t:?}");
    // 8bit: mail-parser yields U+FFFD, so the part is re-decoded from its bytes with a windows-1252 fallback.
    let mut raw = b"From: a@example.com\r\nSubject: x\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nrange 1".to_vec();
    raw.push(0x96);
    raw.extend_from_slice(b"2\r\n");
    let t8 = parse::parse(&raw).expect("parse").text.expect("text");
    assert!(t8.contains("1\u{2013}2"), "{t8:?}");
    assert_eq!(parse::fix_c1("\u{93}hi\u{94}"), "\u{201c}hi\u{201d}");
}

#[test]
fn inline_image_between_text_parts_is_rendered_in_flow() {
    let p = parse::parse(&eml("text_inline_image.eml")).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::Text);
    assert!(
        matches!(p.text_items.as_slice(), [BodyItem::Text(_), BodyItem::Image(_), BodyItem::Text(_)]),
        "{:?}",
        p.text_items
    );
    assert!(p.attachments.is_empty());
    let r = render::render(&p, &opts(ImagePolicy::Block), &CidMode::Asset);
    let (b, i, a) = (
        r.page.find("Before the picture"),
        r.page.find("http://gmail-asset.localhost/a1/m1/"),
        r.page.find("After the picture"),
    );
    assert!(b < i && i < a, "order: {b:?} {i:?} {a:?}");
}

#[test]
fn attachment_only_and_nested_message() {
    let raw = eml("attachment_only.eml");
    let p = parse::parse(&raw).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::None);
    assert_eq!(p.attachments.len(), 2);
    assert_eq!(p.attachments[0].size, 6);
    assert_eq!(p.attachments[1].mime, "message/rfc822");
    let r = render::render(&p, &opts(ImagePolicy::Block), &CidMode::Asset);
    assert!(r.page.contains("no displayable body"));
    let (_, bytes) = parse::part_bytes(&raw, p.attachments[0].part_id).expect("bytes");
    assert_eq!(bytes, vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn hostile_html_is_neutralised() {
    let canary = "127.0.0.1:9";
    let html = s3render::fixtures::hostile_html(canary);
    for policy in [ImagePolicy::Block, ImagePolicy::Allow] {
        let (frag, rep) = sanitize::sanitize(&html, policy, Arc::new(|_| None));
        let l = frag.to_ascii_lowercase();
        for bad in [
            "<script", "onload", "onclick", "onerror", "<form", "<input", "<button", "<base", "<meta", "<link", "<iframe",
            "<object", "<embed", "<svg", "<video", "javascript:", "data:text", "file:", "ms-msdt", "hostile-title",
            "iframe-fallback", "@import", "@font-face", "font.woff2", "import.css", "link.css", "meta-refresh", "/form",
        ] {
            assert!(!l.contains(bad), "{policy:?} leaked {bad}");
        }
        for keep in [
            "Visible text",
            "https://example.com/path?a=1",
            "mailto:x@example.com",
            "href=\"#bottom\"",
            "rgb(0,128,0)",
            "prefers-color-scheme",
            "bgcolor=\"#cccccc\"",
            "padding:6px",
            "data:image/png;base64,iVBOR",
            "bgcolor=\"#ffeedd\"",
        ] {
            assert!(frag.contains(keep), "{policy:?} lost {keep}");
        }
        assert!(frag.contains("rel=\"noopener noreferrer\""));
        match policy {
            ImagePolicy::Block => {
                assert!(!frag.contains(canary), "remote URL survived in Block mode");
                assert!(rep.remote_images >= 4 && rep.remote_css_urls >= 3, "{rep:?}");
            }
            ImagePolicy::Allow => {
                assert!(frag.contains(&format!("src=\"http://{canary}/img-remote.png\"")));
                assert!(frag.contains(&format!("src=\"https://{canary}/proto-rel.png\"")));
                assert_eq!(rep.tracking_pixels, 1);
                // Fonts and stylesheets are still never allowed.
                assert!(!frag.contains("/font.woff2") && !frag.contains("/import.css") && !frag.contains("/link.css"));
            }
        }
    }
}

#[test]
fn page_has_csp_before_any_content() {
    let (frag, _) = sanitize::sanitize("<p>hi</p>", ImagePolicy::Block, Arc::new(|_| None));
    let page = render::page(&frag, ImagePolicy::Block, "");
    let csp_at = page.find("Content-Security-Policy").expect("csp");
    assert!(csp_at < page.find("<p>hi</p>").expect("body"));
    assert!(page.contains("default-src 'none'") && page.contains("<base target=\"_blank\">"));
    assert!(!page.to_ascii_lowercase().contains("<script"));
}
