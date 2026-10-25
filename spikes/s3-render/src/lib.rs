//! Spike S3: Gmail message parsing, sanitizing and rendering. See the `s3` CLI and `harness/`.

pub mod auth;
pub mod compare;
#[cfg(feature = "fetch")]
pub mod fetch;
pub mod parse;
pub mod render;
pub mod sample;
pub mod sanitize;
pub mod stats;
pub mod text;

/// Synthetic fixtures shared by the tests and the Tauri harness.
pub mod fixtures {
    const HOSTILE: &str = include_str!("../fixtures/hostile.html");

    /// The hostile test email body; every remote URL points at `canary` (`host:port`).
    pub fn hostile_html(canary: &str) -> String {
        HOSTILE.replace("{{C}}", canary)
    }

    /// Names of the synthetic `.eml` fixtures the harness can show.
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
}
