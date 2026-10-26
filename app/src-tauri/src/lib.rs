//! Tauri host: windows, the PLAN §3.7 commands, the `gmail-asset` protocol, link policy, save dialog, focus
//! tracking and window raising.
//!
//! Commands delegate to the backend held in Tauri state: `mail_service::Service`, or with the `mock` cargo
//! feature `mock::MockService` (same method names and signatures; canned data for UI development). Extra
//! Rust-side parameters (`State`, `AppHandle`, `WebviewWindow`) may be added freely: the JS contract is only the
//! argument names (snake_case here, camelCase from the UI) and the JSON results.
//!
//! Privacy: nothing here logs addresses, subjects, bodies, file names or paths, only ids, counts and sizes.

mod assets;
mod logging;
#[cfg(feature = "mock")]
mod mock;
mod save;
mod settings;

use std::sync::Arc;
use std::time::Duration;

use mail_model::{
    AccountId, AccountView, CmdError, FolderView, ListResult, MessageId, MessageView, PartId, SaveResult,
    SignOutResult, SyncStatus, WindowState,
};
use mail_service::{EventSink, ServiceEvent};
use tauri::http::{Request, Response, StatusCode};
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Emitter, Manager, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Window, WindowEvent};
use tauri_plugin_opener::OpenerExt;

const MAIN_LABEL: &str = "main";

/// The backend behind the commands.
#[cfg(feature = "mock")]
type Backend = mock::MockService;
#[cfg(not(feature = "mock"))]
type Backend = mail_service::Service;
type Shared<'a> = State<'a, Arc<Backend>>;

// ---------------------------------------------------------------------------------------------
// Commands (PLAN §3.7)
// ---------------------------------------------------------------------------------------------

/// `accounts` -> `AccountView[]`
#[tauri::command]
async fn accounts(svc: Shared<'_>) -> Result<Vec<AccountView>, CmdError> {
    svc.accounts().await
}

/// `add_account {hint?}` -> `AccountView`; resolves when sign-in completes. The window is raised afterwards,
/// because the browser has been in front for the whole sign-in.
#[tauri::command]
async fn add_account(app: AppHandle, svc: Shared<'_>, hint: Option<String>) -> Result<AccountView, CmdError> {
    let r = svc.add_account(hint).await;
    raise_window(&app);
    r
}

/// `reopen_sign_in`
#[tauri::command]
async fn reopen_sign_in(svc: Shared<'_>) -> Result<(), CmdError> {
    svc.reopen_sign_in().await
}

/// `cancel_sign_in`
#[tauri::command]
async fn cancel_sign_in(svc: Shared<'_>) -> Result<(), CmdError> {
    svc.cancel_sign_in().await
}

/// `reauth {accountId}` -> `AccountView`
#[tauri::command]
async fn reauth(app: AppHandle, svc: Shared<'_>, account_id: AccountId) -> Result<AccountView, CmdError> {
    let r = svc.reauth(account_id).await;
    raise_window(&app);
    r
}

/// `sign_out {accountId}` -> `{revoked, manualRemovalNeeded}`
#[tauri::command]
async fn sign_out(svc: Shared<'_>, account_id: AccountId) -> Result<SignOutResult, CmdError> {
    svc.sign_out(account_id).await
}

/// `folders {accountId}` -> `FolderView[]`
#[tauri::command]
async fn folders(svc: Shared<'_>, account_id: AccountId) -> Result<Vec<FolderView>, CmdError> {
    svc.folders(account_id).await
}

/// `messages {accountId, folderId, more?}` -> `ListResult`
#[tauri::command]
async fn messages(
    svc: Shared<'_>,
    account_id: AccountId,
    folder_id: String,
    more: Option<bool>,
) -> Result<ListResult, CmdError> {
    svc.messages(account_id, folder_id, more.unwrap_or(false)).await
}

/// `message {accountId, messageId, showImages}` -> `MessageView`
#[tauri::command]
async fn message(
    svc: Shared<'_>,
    account_id: AccountId,
    message_id: MessageId,
    show_images: bool,
) -> Result<MessageView, CmdError> {
    svc.message(account_id, message_id, show_images).await
}

/// `refresh {accountId}`
#[tauri::command]
async fn refresh(svc: Shared<'_>, account_id: AccountId) -> Result<(), CmdError> {
    svc.refresh(account_id).await
}

/// `set_view {accountId, folderId}`
#[tauri::command]
async fn set_view(svc: Shared<'_>, account_id: AccountId, folder_id: String) -> Result<(), CmdError> {
    svc.set_view(account_id, folder_id).await
}

/// `save_attachment {accountId, messageId, partId}` -> `{saved, fileName?}`. The save dialog is opened from
/// Rust and blocks until answered, so the whole call runs on a blocking thread (not an async worker).
#[tauri::command]
async fn save_attachment(
    app: AppHandle,
    window: WebviewWindow,
    svc: Shared<'_>,
    account_id: AccountId,
    message_id: MessageId,
    part_id: PartId,
) -> Result<SaveResult, CmdError> {
    let svc = svc.inner().clone();
    let saver = save::DialogSaver { data_dir: app_data_dir(&app)?, app, window: Some(window) };
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(svc.save_attachment(account_id, message_id, part_id, &saver))
    })
    .await
    .map_err(|e| CmdError::internal(format!("save task: {e}")))?
}

/// `status` -> `SyncStatus[]`
#[tauri::command]
async fn status(svc: Shared<'_>) -> Result<Vec<SyncStatus>, CmdError> {
    svc.status().await
}

fn app_data_dir(app: &AppHandle) -> Result<std::path::PathBuf, CmdError> {
    app.path().app_local_data_dir().map_err(|e| CmdError::internal(format!("app data dir: {e}")))
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

/// Forwards service events (`mail-changed`, `sync-status`, `account-changed`, `sign-in-progress`) to the UI.
/// Also logs sign-in progress and account state changes (ids and states only, never the address).
struct TauriSink {
    app: AppHandle,
    /// Last logged state per account, so unread-count updates are not logged as state changes.
    states: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl TauriSink {
    fn new(app: AppHandle) -> Self {
        Self { app, states: Default::default() }
    }

    fn log(&self, event: &ServiceEvent) {
        match event {
            ServiceEvent::SignInProgress(p) => tracing::info!(stage = ?p.stage, "sign-in progress"),
            ServiceEvent::AccountChanged(v) => {
                let (id, state) = (format!("{:?}", v.id), format!("{:?}", v.state));
                let mut g = self.states.lock().unwrap_or_else(|p| p.into_inner());
                if g.insert(id.clone(), state.clone()).as_deref() != Some(state.as_str()) {
                    tracing::info!(account = %id, state = %state, "account state");
                }
            }
            _ => {}
        }
    }
}

impl EventSink for TauriSink {
    fn emit(&self, event: ServiceEvent) {
        self.log(&event);
        if let Err(e) = self.app.emit(event.name(), event.payload()) {
            tracing::warn!(event = event.name(), error = %e, "could not emit event");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Link and navigation policy
// ---------------------------------------------------------------------------------------------

/// Schemes a mail link may open in the OS. Anything else (javascript:, file:, data:, ms-*:, gmail-asset:, ...)
/// is dropped.
const LINK_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];

/// Parse `url` and accept only [`LINK_SCHEMES`].
fn check_link(url: &str) -> Option<Url> {
    let u = Url::parse(url.trim()).ok()?;
    LINK_SCHEMES.contains(&u.scheme()).then_some(u)
}

/// Open an allowed link with the OS (from Rust, never in-app).
fn open_checked(app: &AppHandle, url: &str) -> bool {
    let Some(u) = check_link(url) else {
        tracing::info!("link refused (scheme not allowed)");
        return false;
    };
    tracing::info!(scheme = u.scheme(), "opening link");
    if let Err(e) = app.opener().open_url(u.as_str(), None::<&str>) {
        tracing::warn!(error = %e, "could not open link");
        return false;
    }
    true
}

/// Is `url` the app's own page? (`http://tauri.localhost` on Windows, `tauri://localhost` elsewhere, the dev
/// server in `tauri dev`.) `about:srcdoc` is the message iframe. Everything else is refused, including the
/// `gmail-asset` host: assets are sub-resources, never navigation targets.
fn is_app_origin(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => matches!(url.host_str(), Some("tauri.localhost" | "localhost" | "127.0.0.1")),
        "about" => url.as_str() == "about:srcdoc",
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Window state and raising
// ---------------------------------------------------------------------------------------------

/// Minimized wins over focus; an unfocused, visible window is `Unfocused`.
fn derive_window_state(focused: bool, minimized: bool) -> WindowState {
    if minimized {
        WindowState::Minimized
    } else if focused {
        WindowState::Focused
    } else {
        WindowState::Unfocused
    }
}

/// Tell the backend (cadence + poll-on-focus) about the window's state.
fn push_window_state(app: &AppHandle, focused: Option<bool>, minimized: Option<bool>) {
    let state = derive_window_state(focused.unwrap_or(true), minimized.unwrap_or(false));
    if let Some(svc) = app.try_state::<Arc<Backend>>() {
        svc.set_window_state(state);
    }
}

fn push_window_state_of(w: &Window) {
    push_window_state(w.app_handle(), w.is_focused().ok(), w.is_minimized().ok());
}

/// Bring the main window to the front after the system browser had it covered: restore if minimized, show,
/// briefly keep it on top (Windows otherwise only flashes the taskbar button), focus, then drop the pin.
pub fn raise_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window(MAIN_LABEL) else { return };
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_always_on_top(true);
    let _ = w.set_focus();
    let w2 = w.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = w2.set_always_on_top(false);
        let _ = w2.set_focus();
    });
}

// ---------------------------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------------------------

async fn serve_asset(svc: Arc<Backend>, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    if req.method() != tauri::http::Method::GET {
        return assets::plain(StatusCode::METHOD_NOT_ALLOWED);
    }
    // WP-I: switch to `mail_render::parse_asset_path` once implemented (same rules, see assets.rs).
    let Some((account, message, part)) = assets::parse_asset_path(req.uri().path()) else {
        return assets::plain(StatusCode::BAD_REQUEST);
    };
    match svc.asset(account, message, part).await {
        Ok(a) => assets::ok(a.bytes, a.headers),
        Err(e) => {
            tracing::info!(code = ?e.code, part = part.0, "asset not served");
            assets::plain(StatusCode::NOT_FOUND)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Backend construction and app
// ---------------------------------------------------------------------------------------------

/// `Service::open` / `MockService::open` have the same signature.
fn open_backend(app: &AppHandle) -> Result<Backend, Box<dyn std::error::Error>> {
    let dir = app.path().app_local_data_dir()?;
    let sink: Arc<dyn EventSink> = Arc::new(TauriSink::new(app.clone()));
    // Setup runs before the event loop; block until the backend is open.
    tauri::async_runtime::block_on(Backend::open(dir, sink)).map_err(|e| e.message.into())
}

/// App Content-Security-Policy (also in `tauri.conf.json`; the test below keeps the two in sync).
///
/// Chosen to be as tight as the message view allows. The message iframe is `srcdoc`, so it INHERITS this policy
/// on top of its own `<meta>` CSP (PLAN §1 Render); the effective policy is the intersection, so whatever the page
/// CSP says must also be allowed here:
/// - `img-src` lists `http://gmail-asset.localhost` (cid images), `data:` (inline images) and `https: http:`
///   (remote images after "Show images"). The parent cannot know per message whether images are on, so remote
///   images are allowed here and BLOCKED by the page's own CSP and the sanitizer by default. The app UI itself
///   has no remote images.
/// - `style-src-elem`/`style-src-attr` allow `'unsafe-inline'` because mail uses inline `<style>` and `style=""`.
///   They are used instead of `style-src` on purpose: Tauri adds a nonce/hash to `style-src` when present, and a
///   nonce makes browsers ignore `'unsafe-inline'`, which would unstyle every message.
/// - `script-src 'self'` only: the iframe has no `allow-scripts`, and nothing inline runs in the app.
/// - `connect-src ipc: http://ipc.localhost` for Tauri IPC and events; the app makes no other requests.
/// - `frame-src about:` for the `srcdoc` iframe; no `object-src`, `form-action`, `base-uri` or fonts from outside.
pub const APP_CSP: &str = "default-src 'none'; script-src 'self'; style-src-elem 'self' 'unsafe-inline'; \
style-src-attr 'unsafe-inline'; img-src 'self' data: http://gmail-asset.localhost https: http:; font-src 'self'; \
connect-src ipc: http://ipc.localhost; frame-src about:; object-src 'none'; base-uri 'none'; form-action 'none'";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .on_window_event(|w, ev| {
            if w.label() == MAIN_LABEL && matches!(ev, WindowEvent::Focused(_) | WindowEvent::Resized(_)) {
                push_window_state_of(w);
            }
        })
        .register_asynchronous_uri_scheme_protocol("gmail-asset", |ctx, req, responder| {
            let svc: Arc<Backend> = ctx.app_handle().state::<Arc<Backend>>().inner().clone();
            tauri::async_runtime::spawn(async move { responder.respond(serve_asset(svc, req).await) });
        })
        .setup(|app| {
            let handle = app.handle().clone();
            // Logging first, so the backend's own startup lines are captured. A log failure never stops the app.
            let log_dir = app_data_dir(&handle).map(|d| d.join("logs"));
            match log_dir.map_err(|e| e.message).and_then(|d| logging::init(&d)) {
                Ok(()) => {}
                Err(e) => eprintln!("logging disabled: {e}"),
            }
            let backend = Arc::new(open_backend(&handle)?);
            let accounts = tauri::async_runtime::block_on(backend.accounts()).map_or(0, |a| a.len());
            tracing::info!(version = env!("CARGO_PKG_VERSION"), accounts, "stardust-mail started");
            app.manage(backend);
            // Built in code (not tauri.conf.json) so the navigation and new-window hooks can be attached.
            let win = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::App("index.html".into()))
                .title("Stardust Mail")
                .inner_size(1280.0, 800.0)
                .min_inner_size(900.0, 560.0)
                .background_color(tauri::window::Color(22, 23, 26, 255))
                // Only the app's own page may be navigated to; mail content can never navigate the window.
                .on_navigation(is_app_origin)
                // The message iframe (`sandbox="allow-popups"`) asks for a new window when a link is clicked:
                // open allowed schemes with the OS, always deny the in-app window.
                .on_new_window(move |url, _features| {
                    open_checked(&handle, url.as_str());
                    NewWindowResponse::Deny
                })
                .build()?;
            push_window_state(app.handle(), win.is_focused().ok(), win.is_minimized().ok());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            accounts,
            add_account,
            reopen_sign_in,
            cancel_sign_in,
            reauth,
            sign_out,
            folders,
            messages,
            message,
            refresh,
            set_view,
            save_attachment,
            status
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(svc) = app.try_state::<Arc<Backend>>()
            {
                tauri::async_runtime::block_on(svc.shutdown());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_four_schemes_open() {
        for ok in ["https://example.com/a?b=c", "http://example.com", "HTTPS://EXAMPLE.COM", "mailto:a@example.com?subject=x", "tel:+15555550123"] {
            assert!(check_link(ok).is_some(), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "file:///C:/Windows/notepad.exe",
            "data:text/html,x",
            "ms-msdt:/x",
            "vbscript:x",
            "about:blank",
            "blob:https://example.com/x",
            "ftp://example.com/",
            "sms:+15555550123",
            "gmail-asset://localhost/a/b/1",
            "tauri://localhost/",
            "not a url",
            "",
            "//example.com/x",
            "/relative",
        ] {
            let allowed = check_link(bad).is_some();
            assert!(!allowed, "{bad}");
        }
    }

    #[test]
    fn leading_whitespace_does_not_smuggle_a_scheme() {
        assert!(check_link("  javascript:alert(1)").is_none());
        assert!(check_link("\tjava\nscript:alert(1)").is_none());
        assert!(check_link(" https://example.com ").is_some());
    }

    #[test]
    fn only_the_app_itself_may_be_navigated_to() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(is_app_origin(&u("http://tauri.localhost/index.html")));
        assert!(is_app_origin(&u("tauri://localhost/")));
        assert!(is_app_origin(&u("about:srcdoc")));
        for bad in [
            "https://example.com/",
            "http://gmail-asset.localhost/a/b/1",
            "https://tauri.localhost.evil.com/",
            "about:blank",
            "javascript:alert(1)",
            "file:///C:/x",
            "data:text/html,x",
        ] {
            assert!(!is_app_origin(&u(bad)), "{bad}");
        }
    }

    #[test]
    fn window_state_priority() {
        assert_eq!(derive_window_state(true, false), WindowState::Focused);
        assert_eq!(derive_window_state(false, false), WindowState::Unfocused);
        assert_eq!(derive_window_state(false, true), WindowState::Minimized);
        assert_eq!(derive_window_state(true, true), WindowState::Minimized);
    }

    #[test]
    fn csp_matches_tauri_conf_and_allows_what_the_message_view_needs() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["app"]["security"]["csp"].as_str(), Some(APP_CSP));
        for needed in ["http://gmail-asset.localhost", " data:", " https:", " http:", "ipc: http://ipc.localhost", "frame-src about:"] {
            assert!(APP_CSP.contains(needed), "{needed}");
        }
        for forbidden in ["'unsafe-eval'", "script-src 'self' 'unsafe-inline'", "default-src *", " *;"] {
            assert!(!APP_CSP.contains(forbidden), "{forbidden}");
        }
        assert!(!APP_CSP.contains("style-src "), "plain style-src would get Tauri's nonce and lose unsafe-inline");
    }

    #[test]
    fn capability_has_no_dialog_permission() {
        let cap = include_str!("../capabilities/default.json");
        assert!(cap.contains("core:default"));
        assert!(!cap.contains("dialog:"), "dialogs are opened from Rust and need no IPC permission");
    }
}
