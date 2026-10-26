//! Spike S3 harness: shows rendered messages in `<iframe sandbox srcdoc>`, serves `http://gmail-asset.localhost/...`
//! from a custom protocol, routes link clicks to a command that only LOGS the scheme (nothing is ever opened), and
//! runs a hostile-email self-test against a loopback "canary" HTTP server.
//!
//! Privacy: log lines contain ids, counts, sizes, schemes and test names only. Real mail is shown in the window but
//! never logged.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use s3render::parse;
use s3render::render::{self, CidMode, DarkPolicy, RenderOpts};
use s3render::sample::{self, SampleRef};
use s3render::sanitize::{self, ImagePolicy};
use serde::Serialize;
use tauri::http::{Request, Response, StatusCode, header};
use tauri::webview::NewWindowResponse;
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder};

// ---------------------------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------------------------

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("out")
}

fn log(msg: &str) {
    println!("[s3] {msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(out_dir().join("harness.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

// ---------------------------------------------------------------------------------------------
// Canary server: records every request that reaches 127.0.0.1:<port>
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
struct Hit {
    path: String,
    referer: bool,
    cookie: bool,
    tls: bool,
}

struct Canary {
    port: u16,
    hits: Mutex<Vec<Hit>>,
}

fn handle_conn(mut s: TcpStream, canary: &Canary) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = vec![0u8; 16 * 1024];
    let n = s.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return;
    }
    let data = &buf[..n];
    if data[0] == 0x16 {
        // TLS ClientHello: something tried https:// against the canary.
        canary.hits.lock().expect("lock").push(Hit { path: "(tls handshake)".into(), referer: false, cookie: false, tls: true });
        return;
    }
    let text = String::from_utf8_lossy(data);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("?").to_string();
    let (mut referer, mut cookie) = (false, false);
    let mut referer_val = String::new();
    for l in lines {
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("referer:") {
            referer = true;
            // The only possible value here is the harness's own origin; kept to see which one leaks.
            referer_val = l[8..].trim().chars().take(40).collect();
        }
        cookie |= lower.starts_with("cookie:");
    }
    let path = if referer { format!("{path} [referer={referer_val}]") } else { path };
    canary.hits.lock().expect("lock").push(Hit { path, referer, cookie, tls: false });
    let gif: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", gif.len());
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(gif);
}

fn start_canary() -> Arc<Canary> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind canary");
    let port = listener.local_addr().expect("addr").port();
    let canary = Arc::new(Canary { port, hits: Mutex::new(Vec::new()) });
    let c = canary.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let c = c.clone();
            std::thread::spawn(move || handle_conn(conn, &c));
        }
    });
    canary
}

// ---------------------------------------------------------------------------------------------
// Asset protocol
// ---------------------------------------------------------------------------------------------

fn plain(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder().status(status).header(header::CONTENT_TYPE, "text/plain").body(Vec::new()).expect("response")
}

fn raw_for(account: &str, msg: &str) -> Option<Vec<u8>> {
    if account == "syn" {
        return s3render::fixtures::synthetic(msg);
    }
    // `parse_asset_path` already restricted both segments to a safe charset without leading dots.
    SampleRef { alias: account.to_string(), id: msg.to_string() }.raw()
}

fn serve_asset(req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    if req.method() != tauri::http::Method::GET {
        return plain(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some((account, msg, part)) = render::parse_asset_path(req.uri().path()) else {
        log("asset: bad path");
        return plain(StatusCode::BAD_REQUEST);
    };
    let Some((mime, bytes)) = raw_for(&account, &msg).and_then(|raw| parse::part_bytes(&raw, part)) else {
        log(&format!("asset: not found part={part}"));
        return plain(StatusCode::NOT_FOUND);
    };
    let inline = render::inline_safe_mime(&mime);
    log(&format!("asset: ok part={part} mime={mime} bytes={} inline={inline}", bytes.len()));
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, if inline { mime.as_str() } else { "application/octet-stream" })
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", "default-src 'none'; sandbox");
    if !inline {
        b = b.header(header::CONTENT_DISPOSITION, "attachment");
    }
    b.body(bytes).expect("response")
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

struct App {
    canary: Arc<Canary>,
    /// When true the main webview refuses navigations away from the app (the production setting).
    nav_guard: AtomicBool,
}

#[derive(Serialize)]
struct Item {
    key: String,
    label: String,
    group: String,
}

#[tauri::command]
fn list_items() -> Vec<Item> {
    let mut v = Vec::new();
    for mode in ["prod", "prod-images", "raw-csp", "raw-nocsp", "raw-open"] {
        v.push(Item { key: format!("hostile:{mode}"), label: format!("hostile / {mode}"), group: "hostile".into() });
    }
    for n in s3render::fixtures::SYNTHETIC {
        v.push(Item { key: format!("syn:{n}"), label: format!("synthetic / {n}"), group: "synthetic".into() });
    }
    for s in sample::list() {
        let m = s.meta();
        let size = std::fs::metadata(s.raw_path()).map(|m| m.len()).unwrap_or(0);
        v.push(Item {
            key: format!("real:{}:{}", s.alias, s.id),
            label: format!("{} {} [{}] {} KB", s.alias, s.id, m.tag, size / 1024),
            group: "sample".into(),
        });
    }
    v
}

#[derive(Serialize, Default)]
struct ReportView {
    remote_images: usize,
    remote_css_urls: usize,
    tracking_pixels: usize,
    cid_images: usize,
    cid_missing: usize,
}

#[derive(Serialize)]
struct Opened {
    page: String,
    meta: Option<render::MetaView>,
    body: String,
    report: ReportView,
    csp: String,
}

fn dark_of(s: &str) -> DarkPolicy {
    match s {
        "light" => DarkPolicy::Light,
        "html-invert" => DarkPolicy::HtmlInvert,
        _ => DarkPolicy::HtmlWhite,
    }
}

/// A synthetic message whose only body is the hostile HTML.
fn hostile_eml(canary: &str) -> Vec<u8> {
    let mut v = b"From: Mallory <mallory@example.com>\r\nTo: me@example.org\r\nSubject: hostile test\r\nDate: Mon, 25 Oct 2026 08:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n".to_vec();
    v.extend_from_slice(s3render::fixtures::hostile_html(canary).as_bytes());
    v
}

fn strip_meta_csp(page: &str) -> String {
    match page.find("<meta http-equiv=\"Content-Security-Policy\"") {
        Some(a) => {
            let end = page[a..].find('>').map_or(page.len(), |e| a + e + 1);
            format!("{}{}", &page[..a], &page[end..])
        }
        None => page.to_string(),
    }
}

#[tauri::command]
fn open_message(app: State<'_, Arc<App>>, key: String, images: bool, dark: String) -> Result<Opened, String> {
    let policy = if images { ImagePolicy::Allow } else { ImagePolicy::Block };
    let dark = dark_of(&dark);
    let canary = format!("127.0.0.1:{}", app.canary.port);
    let parts: Vec<&str> = key.splitn(3, ':').collect();
    let (account, msg_id, raw, meta, mode) = match parts.as_slice() {
        ["hostile", mode] => ("syn".to_string(), format!("hostile-{mode}"), hostile_eml(&canary), sample::Meta::default(), Some(*mode)),
        ["syn", name] => (
            "syn".to_string(),
            (*name).to_string(),
            s3render::fixtures::synthetic(name).ok_or("unknown fixture")?,
            sample::Meta { id: (*name).to_string(), label_ids: vec!["INBOX".into()], ..Default::default() },
            None,
        ),
        ["real", alias, id] => {
            let s = SampleRef { alias: (*alias).to_string(), id: (*id).to_string() };
            (s.alias.clone(), s.id.clone(), s.raw().ok_or("missing raw")?, s.meta(), None)
        }
        _ => return Err("bad key".into()),
    };
    let p = parse::parse(&raw).ok_or("unparseable")?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (policy, mode_page) = match mode {
        Some("prod-images") => (ImagePolicy::Allow, None),
        Some("prod") => (ImagePolicy::Block, None),
        Some(m @ ("raw-csp" | "raw-nocsp" | "raw-open")) => (ImagePolicy::Block, Some(m)),
        _ => (policy, None),
    };
    let opts = RenderOpts { account: account.clone(), msg_id: msg_id.clone(), images: policy, dark };
    let mut r = render::render(&p, &opts, &CidMode::Asset);
    if let Some(m) = mode_page {
        // Deliberately NOT sanitized: the raw hostile HTML inside the production wrapper, to test CSP + sandbox alone.
        let html = p.html.clone().unwrap_or_default();
        r.page = render::page(&html, policy, "");
        if m != "raw-csp" {
            r.page = strip_meta_csp(&r.page);
        }
    }
    let _ = sanitize::ASSET_HOST;
    log(&format!(
        "open key={} body={:?} page_bytes={} remote_img={} css_url={} cid_ok={} cid_missing={}",
        key.split(':').next().unwrap_or(""),
        r.body,
        r.page.len(),
        r.report.remote_images,
        r.report.remote_css_urls,
        r.report.cid_images,
        r.report.cid_missing
    ));
    Ok(Opened {
        meta: Some(render::meta_view(&p, &meta, &account, now)),
        body: format!("{:?}", r.body),
        report: ReportView {
            remote_images: r.report.remote_images,
            remote_css_urls: r.report.remote_css_urls,
            tracking_pixels: r.report.tracking_pixels,
            cid_images: r.report.cid_images,
            cid_missing: r.report.cid_missing,
        },
        csp: render::csp(policy),
        page: r.page,
    })
}

/// Link click from the message iframe. Production would open allowed schemes in the OS browser; this harness only
/// logs the scheme and the verdict.
#[tauri::command]
fn link_clicked(url: String) -> String {
    let scheme = sanitize::scheme_of(&url).unwrap_or_else(|| "(relative)".into());
    let verdict = if matches!(scheme.as_str(), "http" | "https" | "mailto" | "tel") { "would-open-in-os-browser" } else { "blocked" };
    log(&format!("link_clicked scheme={scheme} verdict={verdict}"));
    verdict.to_string()
}

#[tauri::command]
fn canary_port(app: State<'_, Arc<App>>) -> u16 {
    app.canary.port
}

#[tauri::command]
fn canary_hits(app: State<'_, Arc<App>>) -> Vec<Hit> {
    app.canary.hits.lock().expect("lock").clone()
}

#[tauri::command]
fn canary_clear(app: State<'_, Arc<App>>) {
    app.canary.hits.lock().expect("lock").clear();
}

#[tauri::command]
fn set_nav_guard(app: State<'_, Arc<App>>, on: bool) {
    app.nav_guard.store(on, Ordering::Relaxed);
}

#[tauri::command]
fn log_line(line: String) {
    log(&line.chars().take(600).collect::<String>());
}

#[tauri::command]
fn autotest_enabled() -> bool {
    std::env::var_os("S3_AUTOTEST").is_some()
}

#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
    app.exit(0);
}

fn main() {
    sample::set_out_dir(out_dir());
    let _ = std::fs::create_dir_all(out_dir());
    let canary = start_canary();
    let state = Arc::new(App { canary: canary.clone(), nav_guard: AtomicBool::new(true) });
    log(&format!("harness start; canary on 127.0.0.1:{}", canary.port));
    let st = state.clone();
    let st2 = state.clone();
    tauri::Builder::default()
        .manage(state)
        .register_uri_scheme_protocol("gmail-asset", |_ctx, req| serve_asset(req))
        .setup(move |app| {
            let st_nav = st.clone();
            let st_win = st2.clone();
            let _ = &st_win;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("stardust-mail S3 harness")
                .inner_size(1280.0, 860.0)
                .on_navigation(move |url| {
                    let ok = matches!(url.scheme(), "tauri" | "about") || url.host_str() == Some("tauri.localhost");
                    if !ok {
                        let host_kind = if url.host_str() == Some("127.0.0.1") { "canary" } else { "other" };
                        let guard = st_nav.nav_guard.load(Ordering::Relaxed);
                        log(&format!("on_navigation: non-app navigation scheme={} host={host_kind} blocked={guard}", url.scheme()));
                        return !guard;
                    }
                    true
                })
                .on_new_window(|url, _features| {
                    let scheme = sanitize::scheme_of(url.as_str()).unwrap_or_default();
                    log(&format!("on_new_window: scheme={scheme} -> denied (a real app would open allowed schemes in the OS browser)"));
                    NewWindowResponse::Deny
                })
                .build()?;
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_items,
            open_message,
            link_clicked,
            canary_port,
            canary_hits,
            canary_clear,
            set_nav_guard,
            log_line,
            autotest_enabled,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("tauri");
}
