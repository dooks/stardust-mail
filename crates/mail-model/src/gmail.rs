//! Plain shapes of Gmail API results, already parsed (no `serde_json::Value` crosses crate boundaries).

use serde::{Deserialize, Serialize};

use crate::ids::{HistoryId, LabelId, MessageId, ThreadId};

/// Whether a label is built in or user-created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LabelKind {
    System,
    User,
}

/// One entry of `labels.list`. The API's visibility fields are ignored on purpose (PLAN §1 Folders).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelInfo {
    pub id: LabelId,
    /// Full name; user labels nest with `/`.
    pub name: String,
    pub kind: LabelKind,
}

/// Message counts of one label (`labels.get`). Either may be unknown. Counts can be wrong: show unread only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelCounts {
    pub total: Option<u32>,
    pub unread: Option<u32>,
}

/// One page of `messages.list`: ids only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListPage {
    pub ids: Vec<(MessageId, ThreadId)>,
    pub next_page_token: Option<String>,
}

/// `messages.get?format=metadata` for one message. Header values arrive already RFC 2047-decoded; the
/// snippet is raw (HTML entities are decoded later by `mail_render::snippet_text`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub label_ids: Vec<LabelId>,
    /// Milliseconds since the epoch; the list sort key.
    pub internal_date_ms: i64,
    pub size_estimate: u64,
    pub history_id: HistoryId,
    /// `payload.mimeType`; `multipart/mixed` is the paperclip hint.
    pub mime_type: String,
    pub from_raw: String,
    pub to_raw: String,
    pub subject: String,
    pub date_raw: String,
    pub snippet_raw: String,
}

/// One record-level change from `history.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEvent {
    Added { id: MessageId, thread_id: ThreadId, label_ids: Vec<LabelId> },
    Deleted { id: MessageId },
    LabelsAdded { id: MessageId, label_ids: Vec<LabelId> },
    LabelsRemoved { id: MessageId, label_ids: Vec<LabelId> },
}

impl HistoryEvent {
    /// The message the event is about.
    pub fn id(&self) -> &MessageId {
        match self {
            HistoryEvent::Added { id, .. }
            | HistoryEvent::Deleted { id }
            | HistoryEvent::LabelsAdded { id, .. }
            | HistoryEvent::LabelsRemoved { id, .. } => id,
        }
    }
}

/// One page of `history.list`, events flattened in record order (within a record: added, deleted,
/// labels added, labels removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub events: Vec<HistoryEvent>,
    /// The mailbox's current history id (the new cursor once all pages are applied).
    pub history_id: HistoryId,
    pub next_page_token: Option<String>,
}

/// `users.getProfile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub email: String,
    pub messages_total: u64,
    pub history_id: HistoryId,
}
