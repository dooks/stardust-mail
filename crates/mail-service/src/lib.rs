//! The facade between the Tauri host and everything else (PLAN §3.7 commands, §5 WP-H).
//!
//! One method per command, same names; the host's `MockService` (WP-F) mirrors these method names and argument
//! types so swapping the backend is mechanical. Arguments use the model's id newtypes, which deserialize
//! transparently from the UI's plain strings. Events go out through [`EventSink`]; the host forwards them to the
//! window (`mail-changed`, `sync-status`, `account-changed`, `sign-in-progress`).
//!
//! Layout: [`Service`] is a thin shell over `inner::Inner` (per-account runtime, poll tasks, sign-in, fills).
//! Tests and embedders can swap the Gmail API and OAuth endpoints through [`Parts`] and [`Service::with_parts`].
//!
//! Privacy: log ids, counts, sizes and timings only.

mod api;
mod inner;
mod tap;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gauth::{Client, Store, Tokens};
use mail_cache::CacheRoot;
use mail_model::{
    AccountId, AccountView, CmdError, FolderView, ListResult, MailChanged, MessageId, MessageView, PartId,
    SaveResult, SignInProgress, SignOutResult, SyncStatus, WindowState,
};
use mail_sync::Cadence;

pub use api::DynApi;
use inner::Inner;

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

/// Builds the Gmail API object of one account from its token source.
pub type ApiFactory = Arc<dyn Fn(&Tokens) -> DynApi + Send + Sync>;

/// The replaceable pieces of a [`Service`]: what [`Service::open`] fills in for production and tests override
/// (a fake Gmail API, OAuth endpoints pointing at a local server, fast cadences).
#[derive(Clone)]
pub struct Parts {
    /// The OAuth client; `None` means no `oauth-client.json`: no accounts load and sign-in fails with a config error.
    pub client: Option<Client>,
    /// Creates each account's Gmail API.
    pub api_factory: ApiFactory,
    /// Poll intervals.
    pub cadence: Cadence,
    /// How long a pending sign-in waits for the redirect.
    pub login_timeout: Duration,
}

impl Parts {
    /// Production wiring: the real `GmailClient` against `gmail.googleapis.com`, default cadence, 5-minute sign-in.
    pub fn production(client: Option<Client>) -> Parts {
        Parts {
            client,
            api_factory: Arc::new(|tokens: &Tokens| DynApi::new(gmail_api::GmailClient::new(tokens.clone(), None))),
            cadence: Cadence::default(),
            login_timeout: gauth::LOGIN_TIMEOUT,
        }
    }
}

/// The backend. Cheap to share behind an `Arc`; methods take `&self`.
pub struct Service {
    inner: Arc<Inner>,
}

impl Service {
    /// Load accounts from `<data_dir>\accounts` (skipping bad files), open caches under `<data_dir>\cache`, sweep
    /// leftover deletions, and start one poll task per signed-in account (driven by `Cadence`, `PollGate` and the
    /// window state). `data_dir` is `%LOCALAPPDATA%\com.ziggy.stardust-mail`; the OAuth client is read from
    /// `<data_dir>\oauth-client.json` (a missing file only makes sign-in fail with a config error, and no accounts
    /// are loaded). Must be called inside a tokio runtime.
    pub async fn open(data_dir: PathBuf, sink: Arc<dyn EventSink>) -> Result<Service, CmdError> {
        let client = match Client::load(&data_dir.join("oauth-client.json")) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no usable OAuth client; sign-in is disabled");
                None
            }
        };
        Self::with_parts(data_dir, sink, Parts::production(client)).await
    }

    /// [`Service::open`] with explicit [`Parts`] (tests, or an embedder with its own endpoints).
    pub async fn with_parts(data_dir: PathBuf, sink: Arc<dyn EventSink>, parts: Parts) -> Result<Service, CmdError> {
        let store = Store::open(&data_dir.join("accounts"))?;
        let cache_root = CacheRoot::open(&data_dir.join("cache"))?;
        let swept = cache_root.sweep();
        if swept > 0 {
            tracing::info!(swept, "leftover cache deletions removed");
        }
        let inner = Inner::new(
            sink,
            store,
            cache_root,
            parts.client,
            parts.api_factory,
            parts.cadence,
            parts.login_timeout,
        );
        inner.load_accounts();
        Ok(Service { inner })
    }

    /// Stop the poll tasks and close the caches.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// Host-tracked window state (focus / minimize); changes the cadence and triggers an immediate poll when the
    /// window becomes focused or is restored.
    pub fn set_window_state(&self, state: WindowState) {
        self.inner.set_window_state(state);
    }

    /// `accounts`
    pub async fn accounts(&self) -> Result<Vec<AccountView>, CmdError> {
        Ok(self.inner.accounts())
    }

    /// `add_account {hint?}`: begin login, emit `sign-in-progress{browser}`, open the browser, `waiting`, complete,
    /// check scopes, save, start polling, emit `done` and `account-changed`. Resolves when sign-in completes.
    pub async fn add_account(&self, hint: Option<String>) -> Result<AccountView, CmdError> {
        self.inner.sign_in(hint, None).await
    }

    /// `reopen_sign_in`
    pub async fn reopen_sign_in(&self) -> Result<(), CmdError> {
        self.inner.reopen_sign_in().await
    }

    /// `cancel_sign_in`
    pub async fn cancel_sign_in(&self) -> Result<(), CmdError> {
        self.inner.cancel_sign_in();
        Ok(())
    }

    /// `reauth {accountId}`: like `add_account` with `login_hint` = the account's e-mail; replaces the token file.
    pub async fn reauth(&self, account_id: AccountId) -> Result<AccountView, CmdError> {
        self.inner.reauth(account_id).await
    }

    /// `sign_out {accountId}`: stop its poll task, close its cache, revoke at Google, then always delete the token
    /// file and the cache directory. No `account-changed` is emitted for the removed account (the UI reloads the
    /// list when this resolves, and the event payload cannot express removal).
    pub async fn sign_out(&self, account_id: AccountId) -> Result<SignOutResult, CmdError> {
        self.inner.sign_out(account_id).await
    }

    /// `folders {accountId}`: `labels` + `folder_counts` + `build_tree`, with unread counts filled in.
    pub async fn folders(&self, account_id: AccountId) -> Result<Vec<FolderView>, CmdError> {
        self.inner.folders(&account_id).await
    }

    /// `messages {accountId, folderId, more?}`: cached rows immediately, and a spawned `fill_folder` that emits
    /// `mail-changed` when it adds rows. `more` loads the next older page.
    pub async fn messages(&self, account_id: AccountId, folder_id: String, more: bool) -> Result<ListResult, CmdError> {
        self.inner.messages(&account_id, &folder_id, more).await
    }

    /// `message {accountId, messageId, showImages}`: cached body or `raw`, cache it (under the cap), touch it,
    /// render with the image policy.
    pub async fn message(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        show_images: bool,
    ) -> Result<MessageView, CmdError> {
        self.inner.message(&account_id, &message_id, show_images).await
    }

    /// `refresh {accountId}`: a manual poll through `PollGate` (at most one trailing poll queued).
    pub async fn refresh(&self, account_id: AccountId) -> Result<(), CmdError> {
        self.inner.refresh(&account_id).await
    }

    /// `set_view {accountId, folderId}`: marks the active account (5 s cadence) and the visible folder.
    pub async fn set_view(&self, account_id: AccountId, folder_id: String) -> Result<(), CmdError> {
        self.inner.set_view(&account_id, folder_id)
    }

    /// `save_attachment {accountId, messageId, partId}`: bytes from `mail_render::part`, saved through `saver`.
    pub async fn save_attachment(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
        saver: &dyn FileSaver,
    ) -> Result<SaveResult, CmdError> {
        self.inner.save_attachment(&account_id, &message_id, part_id, saver).await
    }

    /// `status`
    pub async fn status(&self) -> Result<Vec<SyncStatus>, CmdError> {
        Ok(self.inner.status())
    }

    /// Bytes for `gmail-asset://` requests, from the cached or just-fetched raw message. Segments are already
    /// validated by `mail_render::parse_asset_path`.
    pub async fn asset(
        &self,
        account_id: AccountId,
        message_id: MessageId,
        part_id: PartId,
    ) -> Result<Asset, CmdError> {
        self.inner.asset(&account_id, &message_id, part_id).await
    }
}

