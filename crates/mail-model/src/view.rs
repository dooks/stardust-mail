//! View and command DTOs: the JSON the UI sees (camelCase). See PLAN §3.7.

use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, MessageId, PartId};

/// Whether an account can currently be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountState {
    SignedIn,
    NeedsReauth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: AccountId,
    pub email: String,
    pub state: AccountState,
    pub inbox_unread: Option<u32>,
    pub spam_unread: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FolderKind {
    Inbox,
    Starred,
    Sent,
    Drafts,
    AllMail,
    Spam,
    Trash,
    /// The collapsible "Categories" header (not clickable).
    CategoryGroup,
    Category,
    User,
    /// A user-label parent that exists only as a name prefix (not clickable).
    Group,
}

/// One row of the folder tree, already flattened in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderView {
    /// Label id; `"ALL"` for All Mail; `"group:categories"` / `"group:<path>"` for non-clickable groups.
    pub id: String,
    /// Last path segment for user labels, display name otherwise.
    pub name: String,
    pub kind: FolderKind,
    pub depth: u8,
    pub unread: Option<u32>,
    pub clickable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRow {
    pub id: MessageId,
    /// Milliseconds since the epoch.
    pub internal_date: i64,
    /// Empty when the sender has no display name.
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    /// Entity-decoded.
    pub snippet: String,
    pub unread: bool,
    pub starred: bool,
    pub important: bool,
    /// `multipart/mixed` hint.
    pub attachment_hint: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResult {
    pub rows: Vec<MessageRow>,
    /// More older messages can be loaded (online).
    pub more: bool,
    /// Offline and the cached window ends here: show "end of cached mail".
    pub end_of_cache: bool,
}

/// A mailbox, `Name <addr>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Addr {
    pub name: Option<String>,
    pub addr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthLevel {
    Good,
    Mixed,
    Bad,
    /// No usable Authentication-Results header.
    None,
}

/// The sender check, e.g. `SPF pass · DKIM pass (example.com) · DMARC pass`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSummary {
    pub line: String,
    pub level: AuthLevel,
    pub mailed_by: Option<String>,
    pub signed_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentInfo {
    pub part_id: PartId,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    pub id: MessageId,
    pub subject: String,
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    /// Only when it differs from `from`.
    pub reply_to: Option<Addr>,
    /// Milliseconds since the epoch (`Date` header, falling back to `internalDate`).
    pub date: i64,
    /// Label display names.
    pub labels: Vec<String>,
    pub auth: AuthSummary,
    pub attachments: Vec<AttachmentInfo>,
    /// Complete sanitized document for `<iframe srcdoc>`.
    pub page: String,
    pub remote_images_blocked: u32,
    pub images_shown: bool,
    /// Served from the cache without contacting Gmail.
    pub cached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncState {
    Ok,
    Syncing,
    Offline,
    Throttled,
    NeedsReauth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub account_id: AccountId,
    pub state: SyncState,
    /// When the cache was last known current (ms since the epoch).
    pub as_of: Option<i64>,
}

/// Result of `save_attachment`. Never carries (or logs) the full path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub saved: bool,
    pub file_name: Option<String>,
}

/// Result of `sign_out`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignOutResult {
    /// Google confirmed the revoke (or the token was already invalid).
    pub revoked: bool,
    /// The revoke failed: tell the user to remove access at myaccount.google.com → Security → Third-party apps.
    pub manual_removal_needed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SignInStage {
    Browser,
    Waiting,
    Done,
    Error,
}

/// Payload of the `sign-in-progress` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInProgress {
    pub stage: SignInStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Payload of the `mail-changed` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailChanged {
    pub account_id: AccountId,
    /// Label ids whose lists changed.
    pub labels: Vec<String>,
}

/// Window state tracked by the host; selects the polling cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowState {
    Focused,
    /// Visible but not focused.
    Unfocused,
    Minimized,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_json() {
        let a = AccountView {
            id: AccountId::new("1"),
            email: "a@example.com".into(),
            state: AccountState::NeedsReauth,
            inbox_unread: Some(3),
            spam_unread: None,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["state"], "needsReauth");
        assert_eq!(v["inboxUnread"], 3);
        assert!(v["spamUnread"].is_null());

        let f = FolderView {
            id: "ALL".into(),
            name: "All Mail".into(),
            kind: FolderKind::AllMail,
            depth: 0,
            unread: None,
            clickable: true,
        };
        assert_eq!(serde_json::to_value(&f).unwrap()["kind"], "allMail");
        assert_eq!(serde_json::to_value(FolderKind::CategoryGroup).unwrap(), "categoryGroup");

        let r = ListResult { rows: vec![], more: true, end_of_cache: false };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["endOfCache"], false);

        let att = AttachmentInfo { part_id: PartId(2), name: "a.pdf".into(), mime: "application/pdf".into(), size: 9 };
        assert_eq!(serde_json::to_value(&att).unwrap()["partId"], 2);

        let s = SyncStatus { account_id: AccountId::new("1"), state: SyncState::NeedsReauth, as_of: Some(5) };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["accountId"], "1");
        assert_eq!(v["state"], "needsReauth");
        assert_eq!(v["asOf"], 5);

        let p = SignInProgress { stage: SignInStage::Waiting, message: None };
        assert_eq!(serde_json::to_value(&p).unwrap(), serde_json::json!({"stage": "waiting"}));
        let o = SignOutResult { revoked: true, manual_removal_needed: false };
        assert_eq!(serde_json::to_value(&o).unwrap()["manualRemovalNeeded"], false);
        assert_eq!(serde_json::to_value(WindowState::Minimized).unwrap(), "minimized");
        assert_eq!(serde_json::to_value(AuthLevel::None).unwrap(), "none");
    }

    #[test]
    fn roundtrip() {
        let m = MessageRow {
            id: MessageId::new("m1"),
            internal_date: 1,
            from_name: String::new(),
            from_addr: "x@example.com".into(),
            subject: "s".into(),
            snippet: "n".into(),
            unread: true,
            starred: false,
            important: false,
            attachment_hint: true,
            size: 10,
        };
        let back: MessageRow = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
