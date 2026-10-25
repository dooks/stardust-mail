//! The facade between the Tauri host and everything else (PLAN §3.7 commands, §5 WP-H).
//!
//! One method per command, same names; the host's `MockService` (WP-F) mirrors these method names and argument
//! types so swapping the backend is mechanical. Arguments use the model's id newtypes, which deserialize
//! transparently from the UI's plain strings. Events go out through [`EventSink`]; the host forwards them to the
//! window (`mail-changed`, `sync-status`, `account-changed`, `sign-in-progress`).
//!
//! Wave 0 stubs: bodies are `todo!()`; signatures are final.
//!
//! Privacy: log ids, counts, sizes and timings only.

#![allow(unused_variables, dead_code)] // Wave 0 stubs: remove when the file is implemented.

use std::path::PathBuf;
use std::sync::Arc;

use mail_model::{
    AccountId, AccountView, CmdError, FolderView, ListResult, MailChanged, MessageId, MessageView, PartId,
    SaveResult, SignInProgress, SignOutResult, SyncStatus, WindowState,
};

/// Where the service sends events. Must be cheap and non-blocking (the host emits to the webview).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: ServiceEvent);
}

/// Events the UI listens to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    /// `mail-changed {accountId, labels}`
    MailChanged(MailChanged),
    /// `sync-status SyncStatus`
    SyncStatus(SyncStatus),
    /// `account-changed AccountView`
    AccountChanged(AccountView),
    /// `sign-in-progress {stage, message?}`
    SignInProgress(SignInProgress),
}

impl ServiceEvent {
    /// The Tauri event name.
    pub fn name(&self) -> &'static str {
        match self {
            ServiceEvent::MailChanged(_) => "mail-changed",
            ServiceEvent::SyncStatus(_) => "sync-status",
            ServiceEvent::AccountChanged(_) => "account-changed",
            ServiceEvent::SignInProgress(_) => "sign-in-progress",
        }
    }

    /// The camelCase JSON payload for the UI.
    pub fn payload(&self) -> serde_json::Value {
        let v = match self {
            ServiceEvent::MailChanged(p) => serde_json::to_value(p),
            ServiceEvent::SyncStatus(p) => serde_json::to_value(p),
            ServiceEvent::AccountChanged(p) => serde_json::to_value(p),
            ServiceEvent::SignInProgress(p) => serde_json::to_value(p),
        };
        v.unwrap_or(serde_json::Value::Null)
    }
}

/// The host's save dialog, given to [`Service::save_attachment`]. The host implements it with
/// `tauri-plugin-dialog` (prefilled `suggested_name`, last folder used, Downloads the first time), writes the bytes
/// and records the folder. Blocking; the host calls the service from `spawn_blocking`-safe contexts.
/// Cancel -> `Ok(SaveResult { saved: false, file_name: None })`.
pub trait FileSaver: Send + Sync {
    fn save(&self, suggested_name: &str, bytes: &[u8]) -> Result<SaveResult, CmdError>;
}

/// A body for the `gmail-asset` protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub bytes: Vec<u8>,
    /// From `mail_render::asset_headers`.
    pub headers: Vec<(&'static str, String)>,
}

/// The backend. Cheap to share behind an `Arc`; methods take `&self`.
pub struct Service {
    data_dir: PathBuf,
    sink: Arc<dyn EventSink>,
}

impl Service {
    /// Load accounts from `<data_dir>\accounts` (skipping bad files), open caches under `<data_dir>\cache`, sweep
    /// leftover deletions, and start one poll task per signed-in account (driven by `Cadence`, `PollGate` and the
    /// window state). `data_dir` is `%LOCALAPPDATA%\com.ziggy.gmail-ro`; the OAuth client is read from
    /// `<data_dir>\oauth-client.json` (a missing file only makes sign-in fail with a config error).
    pub async fn open(data_dir: PathBuf, sink: Arc<dyn EventSink>) -> Result<Service, CmdError> {
        todo!()
    }

    /// Stop the poll tasks and close the caches.
    pub async fn shutdown(&self) {
        todo!()
    }

    /// Host-tracked window state (focus / minimize); changes the cadence and triggers an immediate poll when the
    /// window becomes focused or is restored.
    pub fn set_window_state(&self, state: WindowState) {
        todo!()
    }

    /// `accounts`
    pub async fn accounts(&self) -> Result<Vec<AccountView>, CmdError> {
        todo!()
    }

    /// `add_account {hint?}`: begin login, emit `sign-in-progress{browser}`, open the browser, `waiting`, complete,
    /// check scopes, save, start polling, emit `done` and `account-changed`. Resolves when sign-in completes.
    pub async fn add_account(&self, hint: Option<String>) -> Result<AccountView, CmdError> {
        todo!()
    }

    /// `reopen_sign_in`
    pub async fn reopen_sign_in(&self) -> Result<(), CmdError> {
        todo!()
    }

    /// `cancel_sign_in`
    pub async fn cancel_sign_in(&self) -> Result<(), CmdError> {
        todo!()
    }

    /// `reauth {accountId}`: like `add_account` with `login_hint` = the account's e-mail; replaces the token file.
    pub async fn reauth(&self, account_id: AccountId) -> Result<AccountView, CmdError> {
        todo!()
    }

    /// `sign_out {accountId}`: stop its poll task, close its cache, revoke at Google, then always delete the token
    /// file and the cache directory; emit `account-changed`.
    pub async fn sign_out(&self, account_id: AccountId) -> Result<SignOutResult, CmdError> {
        todo!()
    }

    /// `folders {accountId}`: `labels` + `folder_counts` + `build_tree`, with unread counts filled in.
    pub async fn folders(&self, account_id: AccountId) -> Result<Vec<FolderView>, CmdError> {
        todo!()
    }

    /// `messages {accountId, folderId, more?}`: cached rows immediately, and a spawned `fill_folder` that emits
    /// `mail-changed` when it adds rows. `more` loads the next older page.
    pub async fn messages(&self, account_id: AccountId, folder_id: String, more: bool) -> Result<ListResult, CmdError> {
        todo!()
    }

    /// `message {accountId, messageId, showImages}`: cached body or `raw`, cache it (under the cap), touch it,
    /// render with the image policy.
    pub async fn message(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        show_images: bool,
    ) -> Result<MessageView, CmdError> {
        todo!()
    }

    /// `refresh {accountId}`: a manual poll through `PollGate` (at most one trailing poll queued).
    pub async fn refresh(&self, account_id: AccountId) -> Result<(), CmdError> {
        todo!()
    }

    /// `set_view {accountId, folderId}`: marks the active account (5 s cadence) and the visible folder.
    pub async fn set_view(&self, account_id: AccountId, folder_id: String) -> Result<(), CmdError> {
        todo!()
    }

    /// `save_attachment {accountId, messageId, partId}`: bytes from `mail_render::part`, saved through `saver`.
    pub async fn save_attachment(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
        saver: &dyn FileSaver,
    ) -> Result<SaveResult, CmdError> {
        todo!()
    }

    /// `status`
    pub async fn status(&self) -> Result<Vec<SyncStatus>, CmdError> {
        todo!()
    }

    /// Bytes for `gmail-asset://` requests, from the cached or just-fetched raw message. Segments are already
    /// validated by `mail_render::parse_asset_path`.
    pub async fn asset(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
    ) -> Result<Asset, CmdError> {
        todo!()
    }
}
