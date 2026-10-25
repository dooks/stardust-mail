//! S1 auth spike: Tauri host. Commands wrap `authcore`; the UI is vanilla JS in `../ui`.
//!
//! Account state machine (in memory; `needs_reauth` would be persisted in the real app):
//! signed out -> pending (one at a time) -> signed in <-> needs re-auth (invalid_grant) -> pending.

use authcore::{Error, LoginOpts, SignOutOpts};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Notify;

/// Error shape sent to the UI.
#[derive(Serialize)]
struct UiError {
    kind: &'static str,
    message: String,
    detail: Option<String>,
}

impl From<Error> for UiError {
    fn from(e: Error) -> Self {
        let detail = match &e {
            Error::MissingScope { granted, probe } => Some(match probe {
                Some(p) => format!("granted: {granted}; diagnostic read with that token: {p}"),
                None => format!("granted: {granted}"),
            }),
            _ => None,
        };
        UiError { kind: e.kind(), message: e.to_string(), detail }
    }
}

fn ui_err(kind: &'static str, message: &str) -> UiError {
    UiError { kind, message: message.into(), detail: None }
}

struct PendingInfo {
    cancel: Arc<Notify>,
    url: String,
    started: u64,
    timeout_secs: u64,
}

#[derive(Default)]
struct AppState {
    pending: Mutex<Option<PendingInfo>>,
    /// sub -> reason; present means "needs re-auth".
    reauth: Mutex<HashMap<String, String>>,
}

#[derive(Serialize)]
struct AccountView {
    email: String,
    sub: String,
    state: &'static str,
    detail: Option<String>,
    scopes: Vec<String>,
    signed_in_at: String,
}

#[derive(Serialize)]
struct PendingView {
    url: String,
    started: u64,
    timeout_secs: u64,
}

#[derive(Serialize)]
struct Snapshot {
    accounts: Vec<AccountView>,
    pending: Option<PendingView>,
    unreadable_files: usize,
    data_dir: String,
}

#[tauri::command]
fn snapshot(state: State<'_, AppState>) -> Result<Snapshot, UiError> {
    let (all, bad) = authcore::load_all(&authcore::data_dir())?;
    let reauth = state.reauth.lock().unwrap();
    let accounts = all
        .iter()
        .map(|s| {
            let r = reauth.get(&s.sub);
            AccountView {
                email: s.email.clone(),
                sub: s.sub.clone(),
                state: if r.is_some() { "needs_reauth" } else { "signed_in" },
                detail: r.cloned(),
                scopes: s.scopes.clone(),
                signed_in_at: authcore::format_utc(s.created_at),
            }
        })
        .collect();
    let pending = state.pending.lock().unwrap().as_ref().map(|p| PendingView {
        url: p.url.clone(),
        started: p.started,
        timeout_secs: p.timeout_secs,
    });
    Ok(Snapshot { accounts, pending, unreadable_files: bad, data_dir: authcore::data_dir().display().to_string() })
}

fn bring_to_front(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        // Windows blocks plain focus-stealing; a brief always-on-top toggle reliably raises it.
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
        let _ = w.set_always_on_top(false);
    }
}

#[derive(Serialize)]
struct AddReport {
    email: String,
    sub: String,
    replaced: bool,
    hint_mismatch: bool,
    messages_total: u64,
    tokeninfo_scopes: Vec<String>,
    read_only: bool,
}

/// Start a sign-in. Only one at a time: a second call is refused with `already_pending`.
#[tauri::command]
async fn add_account(
    app: AppHandle,
    state: State<'_, AppState>,
    hint: Option<String>,
    timeout_secs: Option<u64>,
    probe: Option<bool>,
) -> Result<AddReport, UiError> {
    let timeout_secs = timeout_secs.unwrap_or(300).clamp(5, 900);
    let cancel = authcore::new_cancel();
    // Reserve the slot before any await so two clicks cannot both pass.
    {
        let mut slot = state.pending.lock().unwrap();
        if slot.is_some() {
            return Err(ui_err(
                "already_pending",
                "A sign-in is already waiting for the browser. Finish it, re-open the browser page, or cancel it first.",
            ));
        }
        *slot = Some(PendingInfo {
            cancel: cancel.clone(),
            url: String::new(),
            started: authcore::now_secs(),
            timeout_secs,
        });
    }
    let result = run_login(&app, &state, hint.as_deref(), timeout_secs, probe.unwrap_or(false), &cancel).await;
    *state.pending.lock().unwrap() = None;
    let _ = app.emit("auth-finished", ());
    bring_to_front(&app);
    let out = result?;
    state.reauth.lock().unwrap().remove(&out.info.sub);
    Ok(AddReport {
        email: out.info.email,
        sub: out.info.sub,
        replaced: out.replaced,
        hint_mismatch: out.hint_mismatch,
        messages_total: out.profile.messages_total,
        read_only: out.verdict.read_only,
        tokeninfo_scopes: out.tokeninfo_scopes,
    })
}

async fn run_login(
    app: &AppHandle,
    state: &State<'_, AppState>,
    hint: Option<&str>,
    timeout_secs: u64,
    probe: bool,
    cancel: &Arc<Notify>,
) -> Result<authcore::LoginOutcome, Error> {
    let pending = authcore::begin_login(hint).await?;
    if let Some(p) = state.pending.lock().unwrap().as_mut() {
        p.url = pending.url.clone();
    }
    let _ = app.emit("auth-started", ());
    let opts = LoginOpts { timeout: Duration::from_secs(timeout_secs), probe_on_missing_scope: probe };
    let app2 = app.clone();
    authcore::complete_login(&authcore::data_dir(), pending, &opts, cancel, move || bring_to_front(&app2)).await
}

#[tauri::command]
fn cancel_add(state: State<'_, AppState>) -> bool {
    match state.pending.lock().unwrap().as_ref() {
        Some(p) => {
            p.cancel.notify_one();
            true
        }
        None => false,
    }
}

#[tauri::command]
fn reopen_browser(state: State<'_, AppState>) -> Result<(), UiError> {
    let url = state.pending.lock().unwrap().as_ref().map(|p| p.url.clone());
    match url {
        Some(u) if !u.is_empty() => Ok(authcore::open_browser(&u)?),
        _ => Err(ui_err("error", "no sign-in is pending")),
    }
}

#[tauri::command]
async fn check_account(state: State<'_, AppState>, sub: String) -> Result<authcore::CheckReport, UiError> {
    match authcore::check(&authcore::data_dir(), &sub).await {
        Ok(r) => {
            state.reauth.lock().unwrap().remove(&sub);
            Ok(r)
        }
        Err(e @ Error::Revoked(_)) => {
            state.reauth.lock().unwrap().insert(sub, "access was removed or expired (invalid_grant)".into());
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

#[tauri::command]
async fn sign_out(
    state: State<'_, AppState>,
    sub: String,
    simulate_offline: Option<bool>,
    resend_revoke: Option<bool>,
) -> Result<authcore::SignOutReport, UiError> {
    let opts = SignOutOpts {
        simulate_offline: simulate_offline.unwrap_or(false),
        resend_revoke: resend_revoke.unwrap_or(false),
    };
    let rep = authcore::sign_out(&authcore::data_dir(), &sub, &opts).await?;
    state.reauth.lock().unwrap().remove(&sub);
    Ok(rep)
}

/// Open Google's consent URL in a WebView2 window to record the refusal. The window has no IPC
/// capability. Only a redacted form of each navigation (host, path, parameter NAMES) is reported.
#[tauri::command]
fn try_embedded(app: AppHandle) -> Result<(), UiError> {
    if let Some(w) = app.get_webview_window("embedded") {
        let _ = w.close();
    }
    let url: tauri::Url = authcore::embedded_probe_url()?.parse().map_err(|_| ui_err("error", "bad url"))?;
    let (a1, a2) = (app.clone(), app.clone());
    WebviewWindowBuilder::new(&app, "embedded", WebviewUrl::External(url))
        .title("Embedded WebView2 test (close me)")
        .inner_size(520.0, 720.0)
        .on_navigation(move |u| {
            let _ = a1.emit("embedded-nav", authcore::redact_url(u.as_str()));
            true
        })
        .on_page_load(move |_w, payload| {
            let _ = a2.emit(
                "embedded-nav",
                format!("[{:?}] {}", payload.event(), authcore::redact_url(payload.url().as_str())),
            );
        })
        .build()
        .map_err(|e| ui_err("error", &format!("could not open webview: {e}")))?;
    Ok(())
}

/// Entry point.
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            snapshot,
            add_account,
            cancel_add,
            reopen_browser,
            check_account,
            sign_out,
            try_embedded
        ])
        .run(tauri::generate_context!())
        .expect("tauri run");
}
