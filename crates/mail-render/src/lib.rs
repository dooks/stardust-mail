//! MIME parse, body choice, sanitize, page wrap, sender check, attachments (PLAN §3.5, WP-E).
//!
//! Pipeline: `parse(raw)` -> `Parsed`; `render(&parsed, account, msg, ImagePolicy)` -> a complete white-background
//! document for `<iframe srcdoc>`; `header_block`, `auth_summary`, `attachments` feed the header area; `part` and
//! `asset_headers` serve the `gmail-asset` protocol and attachment saves.
//!
//! Port of spike S3 (parse, auth, text, sanitize, render) with S2 address and snippet helpers. The page is always
//! white with dark text.
//!
//! Privacy: never log header values or bodies.

mod auth;
mod meta;
mod parse;
mod render;
mod sanitize;
mod text;

pub use meta::{HeaderBlock, auth_summary, attachments, header_block, message_row, parse_from, snippet_text};
pub use parse::{BodyItem, BodyKind, Part, Parsed, fix_c1, norm_cid, parse};
pub use render::{
    ImagePolicy, Rendered, asset_headers, asset_url, inline_safe_mime, parse_asset_path, part, render,
};

/// Synthetic fixtures shared by the tests and the Tauri host's mock.
pub mod fixtures {
    const HOSTILE: &str = include_str!("../fixtures/hostile.html");

    /// The hostile test email body; every remote URL points at `canary` (`host:port`).
    pub fn hostile_html(canary: &str) -> String {
        HOSTILE.replace("{{C}}", canary)
    }

    /// Names of the synthetic `.eml` fixtures.
    pub const SYNTHETIC: &[&str] = &["alt", "related_cid", "charsets", "text_inline_image", "attachment_only"];

    /// A synthetic message with CRLF line endings.
    pub fn synthetic(name: &str) -> Option<Vec<u8>> {
        let text = match name {
            "alt" => include_str!("../fixtures/alt.eml"),
            "related_cid" => include_str!("../fixtures/related_cid.eml"),
            "charsets" => include_str!("../fixtures/charsets.eml"),
            "text_inline_image" => include_str!("../fixtures/text_inline_image.eml"),
            "attachment_only" => include_str!("../fixtures/attachment_only.eml"),
            _ => return None,
        };
        Some(text.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fixtures_load() {
            for n in SYNTHETIC {
                let raw = synthetic(n).unwrap();
                assert!(raw.windows(2).any(|w| w == b"\r\n"), "{n}");
            }
            assert!(synthetic("nope").is_none());
            assert!(hostile_html("127.0.0.1:9").contains("127.0.0.1:9"));
            assert!(!hostile_html("127.0.0.1:9").contains("{{C}}"));
        }
    }
}
