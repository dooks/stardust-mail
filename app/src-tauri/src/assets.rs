//! The `gmail-asset` protocol: `http://gmail-asset.localhost/<accountId>/<messageId>/<partId>`.
//!
//! The path and header rules live in `mail-render` (one source of truth); the tests below pin them from the
//! host's side.

use tauri::http::{Response, StatusCode, header};

#[cfg(any(feature = "mock", test))]
pub use mail_render::asset_headers;
pub use mail_render::parse_asset_path;

/// A bodiless response with a status (errors never carry details).
pub fn plain(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .header("X-Content-Type-Options", "nosniff")
        .body(Vec::new())
        .unwrap_or_default()
}

/// A 200 response with the service's bytes and headers.
pub fn ok(bytes: Vec<u8>, headers: Vec<(&'static str, String)>) -> Response<Vec<u8>> {
    let mut b = Response::builder().status(StatusCode::OK);
    for (k, v) in headers {
        b = b.header(k, v);
    }
    b.body(bytes).unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_model::PartId;

    #[test]
    fn good_paths_parse() {
        let (a, m, p) = parse_asset_path("/1234567890.abc/18f3a0c1d2e3f405/7").unwrap();
        assert_eq!((a.as_str(), m.as_str(), p), ("1234567890.abc", "18f3a0c1d2e3f405", PartId(7)));
        assert!(parse_asset_path("/a@b+c-d_e/m-1/0").is_some());
        assert!(parse_asset_path("a/m/1").is_some(), "leading slash optional");
        assert!(parse_asset_path("/a/m/123456789").is_some());
    }

    #[test]
    fn junk_paths_are_rejected() {
        for bad in [
            "",
            "/",
            "/a/m",
            "/a/m/1/x",
            "/a/m/1/",
            "/../m/7",
            "/a/../7",
            "/a/.hidden/1",
            "/.a/m/1",
            "//m/1",
            "/a//1",
            "/a/m/",
            "/a/m/-1",
            "/a/m/x",
            "/a/m/+1",
            "/a/m/1234567890",
            "/a/%2e%2e/1",
            "/a%2fb/m/1",
            "/a b/m/1",
            "/a\\b/m/1",
            "/a/m:1/1",
            "/a/m*/1",
            "/a/\u{e9}/1",
        ] {
            assert!(parse_asset_path(bad).is_none(), "{bad:?}");
        }
        assert!(parse_asset_path(&format!("/{}/m/1", "a".repeat(129))).is_none());
        assert!(parse_asset_path(&format!("/a/{}/1", "m".repeat(65))).is_none());
        assert!(parse_asset_path(&format!("/{}/{}/1", "a".repeat(128), "m".repeat(64))).is_some());
    }

    #[test]
    fn only_raster_images_are_inline() {
        for m in ["image/png", "IMAGE/JPEG", "image/gif", "image/webp"] {
            let h = asset_headers(m);
            assert!(h.iter().any(|(k, v)| *k == "Content-Type" && v.starts_with("image/")), "{m}");
            assert!(!h.iter().any(|(k, _)| *k == "Content-Disposition"), "{m}");
        }
        for m in ["image/svg+xml", "text/html", "application/pdf", "", "image/png; x=y"] {
            let h = asset_headers(m);
            assert!(h.contains(&("Content-Type", "application/octet-stream".to_string())), "{m}");
            assert!(h.contains(&("Content-Disposition", "attachment".to_string())), "{m}");
        }
    }

    #[test]
    fn every_response_is_nosniff_and_sandboxed() {
        for m in ["image/png", "text/html"] {
            let h = asset_headers(m);
            assert!(h.contains(&("X-Content-Type-Options", "nosniff".to_string())));
            assert!(h.iter().any(|(k, v)| *k == "Content-Security-Policy" && v.contains("sandbox")));
        }
    }

    #[test]
    fn responses_carry_status_and_headers() {
        let r = ok(vec![1, 2, 3], asset_headers("image/png"));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "image/png");
        assert_eq!(r.body(), &vec![1, 2, 3]);
        assert_eq!(plain(StatusCode::NOT_FOUND).status(), StatusCode::NOT_FOUND);
    }
}
