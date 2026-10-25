//! End-to-end tests on SYNTHETIC emails only (fixtures\*.eml and the hostile HTML), through the public API.

use mail_model::{AccountId, AuthLevel, MessageId, PartId};
use mail_render::{
    BodyItem, BodyKind, ImagePolicy, Parsed, asset_url, attachments, auth_summary, fixtures, header_block, parse, part,
    render,
};

fn eml(name: &str) -> Vec<u8> {
    fixtures::synthetic(name).expect("fixture")
}

fn acct() -> AccountId {
    AccountId::new("a1")
}

fn mid() -> MessageId {
    MessageId::new("m1")
}

#[test]
fn multipart_alternative_headers_and_auth() {
    let p = parse(&eml("alt")).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::Html);
    assert!(p.has_html_part && p.has_text_part);
    assert_eq!(p.from[0].name.as_deref(), Some("Alice Example"));
    assert_eq!(p.from[0].addr, "alice@example.com");
    assert_eq!(p.to.len(), 2);
    assert_eq!(p.to[1].addr, "carol@example.net");
    assert_eq!(p.cc[0].name.as_deref(), Some("Dave, D."));
    assert_eq!(p.reply_to[0].addr, "replies@lists.example.com");
    assert_eq!(p.date_ts, Some(1_637_446_921)); // 2021-11-20T22:22:01Z
    assert_eq!(p.authentication_results.len(), 1);

    let a = auth_summary(&p);
    assert_eq!(a.line, "SPF pass \u{b7} DKIM pass (example.com) \u{b7} DMARC pass");
    assert_eq!(a.level, AuthLevel::Good);
    assert_eq!(a.mailed_by.as_deref(), Some("mail.example.com"));

    let h = header_block(&p);
    assert_eq!(h.from.addr, "alice@example.com");
    assert_eq!(h.reply_to.as_ref().map(|r| r.addr.as_str()), Some("replies@lists.example.com"));
    assert_eq!(h.date_ms, Some(1_637_446_921_000));
}

#[test]
fn related_cid_and_attachments() {
    let p = parse(&eml("related_cid")).expect("parse");
    let pdf = p.parts.iter().find(|a| a.mime == "application/pdf").expect("pdf");
    assert_eq!(pdf.file_name.as_deref(), Some("r\u{e9}sum\u{e9}.pdf")); // RFC 2231 filename
    assert_eq!(pdf.data.len(), 9);
    let logo = p.parts.iter().find(|a| a.mime == "image/png").expect("logo");
    assert!(logo.inline_referenced, "the cid image is shown inline, so it is hidden from the attachment list");
    assert_eq!(p.cids["logo@shop"], logo.id);

    let list = attachments(&p);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "r\u{e9}sum\u{e9}.pdf");
    assert_eq!(list[0].mime, "application/pdf");
    assert_eq!(list[0].size, 9);

    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.contains(&asset_url(&acct(), &mid(), logo.id)), "{}", r.page);
    assert!(!r.page.contains("nothere"));
    // Asset bytes come back byte-exact.
    let (bytes, mime, name) = part(&p, logo.id).expect("part");
    assert_eq!(mime, "image/png");
    assert_eq!(name.as_deref(), Some("logo.png"));
    assert_eq!(&bytes[..4], b"\x89PNG");
    assert!(part(&p, PartId(9999)).is_none());
}

#[test]
fn non_utf8_charsets_and_encoded_words() {
    let p = parse(&eml("charsets")).expect("parse");
    assert_eq!(p.from[0].name.as_deref(), Some("J\u{fc}rgen M\u{fc}ller"));
    assert_eq!(p.to[0].name.as_deref(), Some("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"));
    // Adjacent encoded words are joined without the folding whitespace between them.
    assert_eq!(p.subject.as_deref(), Some("Caf\u{e9} \u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"));
    let text = p.text.expect("text");
    assert!(text.contains("caf\u{e9} na\u{ef}ve joined"), "{text:?}");
    assert!(text.contains("\u{65e5}\u{672c}\u{8a9e}"), "{text:?}");
    assert_eq!(p.charset_problems, 0);
}

#[test]
fn mislabeled_windows_1252_bytes_are_repaired() {
    // Quoted-printable: what mail-parser returns for the invalid byte 0x96 (labelled UTF-8) is a C1 control.
    let qp = b"From: a@example.com\r\nSubject: x\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nrange 1=962\r\n".to_vec();
    let t = parse(&qp).expect("parse").text.expect("text");
    assert!(t.contains("1\u{2013}2"), "{t:?}");
    // 8bit: mail-parser yields U+FFFD, so the part is re-decoded from its bytes with a windows-1252 fallback.
    let mut raw = b"From: a@example.com\r\nSubject: x\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nrange 1".to_vec();
    raw.push(0x96);
    raw.extend_from_slice(b"2\r\n");
    let t8 = parse(&raw).expect("parse").text.expect("text");
    assert!(t8.contains("1\u{2013}2"), "{t8:?}");
    assert_eq!(mail_render::fix_c1("\u{93}hi\u{94}"), "\u{201c}hi\u{201d}");
}

#[test]
fn inline_image_between_text_parts_is_rendered_in_flow() {
    let p = parse(&eml("text_inline_image")).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::Text);
    assert!(
        matches!(p.text_items.as_slice(), [BodyItem::Text(_), BodyItem::Image(_), BodyItem::Text(_)]),
        "{:?}",
        p.text_items
    );
    assert!(attachments(&p).is_empty());
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    let (b, i, a) = (
        r.page.find("Before the picture"),
        r.page.find("http://gmail-asset.localhost/a1/m1/"),
        r.page.find("After the picture"),
    );
    assert!(b < i && i < a, "order: {b:?} {i:?} {a:?}");
    // Plain text is on white too.
    assert!(r.page.contains("background: #fff") && !r.page.contains("dark-text"));
}

#[test]
fn attachment_only_and_nested_message() {
    let p = parse(&eml("attachment_only")).expect("parse");
    assert_eq!(p.body_kind(), BodyKind::None);
    assert_eq!(p.parts.len(), 2);
    assert_eq!(p.parts[0].data.len(), 6);
    assert_eq!(p.parts[1].mime, "message/rfc822");
    assert_eq!(attachments(&p).len(), 2);
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.contains("no displayable body"));
    let (bytes, _, _) = part(&p, p.parts[0].id).expect("bytes");
    assert_eq!(bytes, vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn hostile_html_page_has_nothing_active_or_remote_in_block_mode() {
    let canary = "127.0.0.1:9";
    let p = Parsed { html: Some(fixtures::hostile_html(canary)), ..Default::default() };
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.remote_images_blocked >= 7, "{}", r.remote_images_blocked);
    let l = r.page.to_ascii_lowercase();
    assert!(!l.contains(canary), "remote URL in page");
    for bad in [
        "<script", "onload", "onclick", "onerror", "<form", "<input", "<iframe", "<object", "<embed", "<svg", "<video",
        "javascript:", "<link", "@import", "@font-face",
    ] {
        assert!(!l.contains(bad), "leaked {bad}");
    }
    assert!(r.page.contains("Visible text"));
    assert!(r.page.contains("img-src http://gmail-asset.localhost data:;"));

    let allowed = render(&p, &acct(), &mid(), ImagePolicy::Allow);
    assert_eq!(allowed.remote_images_blocked, 0);
    assert!(allowed.page.contains(&format!("http://{canary}/img-remote.png")));
    assert!(allowed.page.contains("img-src http://gmail-asset.localhost data: https: http:;"));
}

#[test]
fn page_wrapper_is_white_with_csp_and_base() {
    let p = Parsed { html: Some("<p>hi</p>".into()), ..Default::default() };
    let r = render(&p, &acct(), &mid(), ImagePolicy::Block);
    assert!(r.page.contains(
        "default-src 'none'; style-src 'unsafe-inline'; img-src http://gmail-asset.localhost data:; form-action 'none'; base-uri 'none'"
    ));
    assert!(r.page.contains("<base target=\"_blank\">") && r.page.contains("content=\"no-referrer\""));
    assert!(r.page.find("Content-Security-Policy") < r.page.find("<p>hi</p>"));
    assert!(!r.page.to_ascii_lowercase().contains("<script"));
}
