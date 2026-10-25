//! Sanitize + page wrap + asset helpers (ports of spike S3 `sanitize.rs`, `text.rs`, `render.rs`).

use mail_model::{AccountId, MessageId, PartId};

use crate::parse::Parsed;

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

/// Sanitize (`ammonia` with S3's exact config) and wrap. `cid:` images become
/// `http://gmail-asset.localhost/<account>/<msg>/<part>`. Never fails: an unparseable body renders as empty.
pub fn render(parsed: &Parsed, account: &AccountId, msg: &MessageId, images: ImagePolicy) -> Rendered {
    todo!()
}

/// `http://gmail-asset.localhost/<account>/<msg>/<part>` with each segment restricted to `[A-Za-z0-9_.@+-]`.
pub fn asset_url(account: &AccountId, msg: &MessageId, part: PartId) -> String {
    todo!()
}

/// Parse the path of a `gmail-asset` request (`/<account>/<msg>/<part>`). Strict charset and length limits;
/// `None` for anything else (dot-leading segments, extra segments, non-digit part).
pub fn parse_asset_path(path: &str) -> Option<(AccountId, MessageId, PartId)> {
    todo!()
}

/// Mime types that may be shown inline: png, jpeg, gif, webp, bmp, avif, x-icon.
pub fn inline_safe_mime(mime: &str) -> bool {
    todo!()
}

/// One part for serving or saving: `(bytes, mime, file_name)`.
pub fn part(parsed: &Parsed, id: PartId) -> Option<(Vec<u8>, String, Option<String>)> {
    todo!()
}

/// Response headers for a served asset: inline-safe raster images get their `Content-Type`; anything else is
/// served as `application/octet-stream` with `Content-Disposition: attachment`. Always `X-Content-Type-Options:
/// nosniff` and a `Content-Security-Policy: sandbox` header.
pub fn asset_headers(mime: &str) -> Vec<(&'static str, String)> {
    todo!()
}
