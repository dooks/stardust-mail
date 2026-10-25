//! Tauri host (WP-F, wired to the real service in WP-I): windows, the PLAN §3.7 commands, the `gmail-asset`
//! protocol, link policy, save dialog, focus tracking.
//!
//! Wave 0 stubs: every command is registered with its final name and arguments (snake_case; the UI sends
//! camelCase and Tauri converts) and returns `CmdError` "not implemented". JS only ever sees these names, the
//! argument names and the JSON results.
//!
//! Privacy: nothing here logs addresses, subjects or bodies, only ids, counts and names.

#![allow(unused_variables, dead_code)] // Wave 0 stubs: remove when the file is implemented.

use mail_model::{
    AccountId, AccountView, CmdError, FolderView, ListResult, MessageId, MessageView, PartId, SaveResult,
    SignOutResult, SyncStatus,
};
use tauri::http::{Response, StatusCode};
use tauri::webview::NewWindowResponse;
use tauri::{WebviewUrl, WebviewWindowBuilder};

const MAIN_LABEL: &str = "main";

fn not_implemented<T>() -> Result<T, CmdError> {
    Err(CmdError::internal("not implemented"))
}

/// `accounts` -> `AccountView[]`
#[tauri::command]
async fn accounts() -> Result<Vec<AccountView>, CmdError> {
    not_implemented()
}

/// `add_account {hint?}` -> `AccountView`; resolves when sign-in completes.
#[tauri::command]
async fn add_account(hint: Option<String>) -> Result<AccountView, CmdError> {
    not_implemented()
}

/// `reopen_sign_in`
#[tauri::command]
async fn reopen_sign_in() -> Result<(), CmdError> {
    not_implemented()
}

/// `cancel_sign_in`
#[tauri::command]
async fn cancel_sign_in() -> Result<(), CmdError> {
    not_implemented()
}

/// `reauth {accountId}` -> `AccountView`
#[tauri::command]
async fn reauth(account_id: AccountId) -> Result<AccountView, CmdError> {
    not_implemented()
}

/// `sign_out {accountId}` -> `{revoked, manualRemovalNeeded}`
#[tauri::command]
async fn sign_out(account_id: AccountId) -> Result<SignOutResult, CmdError> {
    not_implemented()
}

/// `folders {accountId}` -> `FolderView[]`
#[tauri::command]
async fn folders(account_id: AccountId) -> Result<Vec<FolderView>, CmdError> {
    not_implemented()
}

/// `messages {accountId, folderId, more?}` -> `ListResult`
#[tauri::command]
async fn messages(account_id: AccountId, folder_id: String, more: Option<bool>) -> Result<ListResult, CmdError> {
    not_implemented()
}

/// `message {accountId, messageId, showImages}` -> `MessageView`
#[tauri::command]
async fn message(account_id: AccountId, message_id: MessageId, show_images: bool) -> Result<MessageView, CmdError> {
    not_implemented()
}

/// `refresh {accountId}`
#[tauri::command]
async fn refresh(account_id: AccountId) -> Result<(), CmdError> {
    not_implemented()
}

/// `set_view {accountId, folderId}`
#[tauri::command]
async fn set_view(account_id: AccountId, folder_id: String) -> Result<(), CmdError> {
    not_implemented()
}

/// `save_attachment {accountId, messageId, partId}` -> `{saved, fileName?}`
#[tauri::command]
async fn save_attachment(
    account_id: AccountId,
    message_id: MessageId,
    part_id: PartId,
) -> Result<SaveResult, CmdError> {
    not_implemented()
}

/// `status` -> `SyncStatus[]`
#[tauri::command]
async fn status() -> Result<Vec<SyncStatus>, CmdError> {
    not_implemented()
}

/// Build and run the app.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // `http://gmail-asset.localhost/<accountId>/<messageId>/<partId>` (stub: 501 until WP-F).
        .register_asynchronous_uri_scheme_protocol("gmail-asset", |_ctx, _request, responder| {
            let response = Response::builder().status(StatusCode::NOT_IMPLEMENTED).body(Vec::new());
            if let Ok(response) = response {
                responder.respond(response);
            }
        })
        .setup(|app| {
            // Built in code (not tauri.conf.json) so the navigation and new-window hooks can be attached.
            WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::App("index.html".into()))
                .title("gmail-ro")
                .inner_size(1280.0, 800.0)
                .min_inner_size(900.0, 560.0)
                .background_color(tauri::window::Color(22, 23, 26, 255))
                // Links open via the OS from Rust after a scheme check (WP-F); never in-app.
                .on_new_window(|_url, _features| NewWindowResponse::Deny)
                .build()?;
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
