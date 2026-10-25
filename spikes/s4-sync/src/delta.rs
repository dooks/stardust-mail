//! Apply `history.list` records to the cache. Pure over `Store` + JSON (unit-tested with synthetic data).

use std::collections::BTreeSet;

use serde_json::Value;

use crate::store::{Row, Store};

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Added { id: String, labels: BTreeSet<String> },
    Deleted { id: String },
    LabelAdded { id: String, labels: BTreeSet<String> },
    LabelRemoved { id: String, labels: BTreeSet<String> },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Added { .. } => "messageAdded",
            Event::Deleted { .. } => "messageDeleted",
            Event::LabelAdded { .. } => "labelAdded",
            Event::LabelRemoved { .. } => "labelRemoved",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Event::Added { id, .. } | Event::Deleted { id } | Event::LabelAdded { id, .. } | Event::LabelRemoved { id, .. } => id,
        }
    }
}

fn label_set(v: &Value) -> BTreeSet<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Flatten a `history.list` response into ordered events (record order; within a record:
/// added, deleted, labelAdded, labelRemoved).
pub fn parse(resp: &Value) -> Vec<Event> {
    let mut out = Vec::new();
    for rec in resp["history"].as_array().into_iter().flatten() {
        for m in rec["messagesAdded"].as_array().into_iter().flatten() {
            if let Some(id) = m["message"]["id"].as_str() {
                out.push(Event::Added { id: id.into(), labels: label_set(&m["message"]["labelIds"]) });
            }
        }
        for m in rec["messagesDeleted"].as_array().into_iter().flatten() {
            if let Some(id) = m["message"]["id"].as_str() {
                out.push(Event::Deleted { id: id.into() });
            }
        }
        for m in rec["labelsAdded"].as_array().into_iter().flatten() {
            if let Some(id) = m["message"]["id"].as_str() {
                out.push(Event::LabelAdded { id: id.into(), labels: label_set(&m["labelIds"]) });
            }
        }
        for m in rec["labelsRemoved"].as_array().into_iter().flatten() {
            if let Some(id) = m["message"]["id"].as_str() {
                out.push(Event::LabelRemoved { id: id.into(), labels: label_set(&m["labelIds"]) });
            }
        }
    }
    out
}

#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    /// Uncached messages that now belong to a label whose list we hold: fetch metadata, then `admit`.
    pub need_fetch: BTreeSet<String>,
    pub deleted: usize,
    pub relabeled: usize,
    pub ignored: usize,
}

pub fn apply(store: &Store, events: &[Event]) -> rusqlite::Result<Applied> {
    let mut a = Applied::default();
    for e in events {
        match e {
            Event::Added { id, labels } | Event::LabelAdded { id, labels } => {
                if let Some(mut cur) = store.labels(id)? {
                    let before = cur.len();
                    cur.extend(labels.iter().cloned());
                    // Re-adding on replay is a no-op; a real change counts as relabeled.
                    if cur.len() != before {
                        store.set_labels(id, &cur)?;
                        a.relabeled += 1;
                    }
                } else if store.any_window_label(labels)? {
                    a.need_fetch.insert(id.clone());
                } else {
                    a.ignored += 1;
                }
            }
            Event::LabelRemoved { id, labels } => {
                if let Some(mut cur) = store.labels(id)? {
                    let before = cur.len();
                    for l in labels {
                        cur.remove(l);
                    }
                    if cur.len() != before {
                        store.set_labels(id, &cur)?;
                        a.relabeled += 1;
                    }
                } else {
                    a.need_fetch.remove(id);
                    a.ignored += 1;
                }
            }
            Event::Deleted { id } => {
                a.need_fetch.remove(id);
                if store.delete(id)? {
                    a.deleted += 1;
                } else {
                    a.ignored += 1;
                }
            }
        }
    }
    Ok(a)
}

/// Insert a freshly fetched row if it belongs in a list we hold (or is already cached).
/// A row older than a partial window's `oldest_date` is outside the window and is not admitted.
pub fn admit(store: &Store, row: &Row, now: i64) -> rusqlite::Result<bool> {
    let cached = store.has(&row.id)?;
    let mut in_window = false;
    for l in &row.labels {
        if let Some((oldest, complete, _)) = store.window(l)?
            && (complete || row.internal_date >= oldest)
        {
            in_window = true;
        }
    }
    if cached || in_window {
        store.upsert(row, now)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Codec;
    use serde_json::json;
    use std::path::Path;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn row(id: &str, date: i64, labels: &[&str]) -> Row {
        Row { id: id.into(), thread_id: format!("t{id}"), internal_date: date, history_id: 1, size_estimate: 10, labels: set(labels), meta: b"{}".to_vec() }
    }

    fn store() -> Store {
        let s = Store::open(Path::new(":memory:"), None, Codec::Plain).unwrap();
        s.upsert(&row("a", 100, &["INBOX", "UNREAD"]), 0).unwrap();
        s.upsert(&row("b", 200, &["INBOX"]), 0).unwrap();
        s.upsert(&row("c", 300, &["INBOX", "STARRED"]), 0).unwrap();
        s.set_window("INBOX", 100, false, 0).unwrap();
        s
    }

    #[test]
    fn parses_all_four_types_in_order() {
        let j = json!({"history":[
            {"id":"5","messagesAdded":[{"message":{"id":"n1","threadId":"t","labelIds":["INBOX","UNREAD"]}}]},
            {"id":"6","labelsRemoved":[{"message":{"id":"a"},"labelIds":["INBOX"]}],"labelsAdded":[{"message":{"id":"b"},"labelIds":["STARRED"]}]},
            {"id":"7","messagesDeleted":[{"message":{"id":"c"}}]}
        ],"historyId":"9"});
        let ev = parse(&j);
        let names: Vec<_> = ev.iter().map(|e| e.name()).collect();
        assert_eq!(names, ["messageAdded", "labelAdded", "labelRemoved", "messageDeleted"]);
        assert!(parse(&json!({"historyId":"9"})).is_empty());
    }

    #[test]
    fn archive_star_delete_and_new_mail() {
        let s = store();
        let ev = vec![
            Event::LabelRemoved { id: "a".into(), labels: set(&["INBOX"]) },
            Event::LabelAdded { id: "b".into(), labels: set(&["STARRED"]) },
            Event::Deleted { id: "c".into() },
            Event::Added { id: "n1".into(), labels: set(&["INBOX", "UNREAD"]) },
            Event::Added { id: "x9".into(), labels: set(&["SENT"]) },
        ];
        let a = apply(&s, &ev).unwrap();
        assert_eq!(a.relabeled, 2);
        assert_eq!(a.deleted, 1);
        assert_eq!(a.ignored, 1);
        assert_eq!(a.need_fetch, set(&["n1"]));
        assert_eq!(s.labels("a").unwrap().unwrap(), set(&["UNREAD"]));
        assert_eq!(s.labels("b").unwrap().unwrap(), set(&["INBOX", "STARRED"]));
        assert!(s.labels("c").unwrap().is_none());
        let inbox: Vec<_> = s.rows_for_label("INBOX", 10).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(inbox, ["b"]);
    }

    #[test]
    fn replay_is_idempotent() {
        let s = store();
        let ev = vec![Event::LabelAdded { id: "b".into(), labels: set(&["STARRED"]) }, Event::LabelRemoved { id: "a".into(), labels: set(&["INBOX"]) }];
        let first = apply(&s, &ev).unwrap();
        let second = apply(&s, &ev).unwrap();
        assert_eq!(first.relabeled, 2);
        assert_eq!(second.relabeled, 0);
    }

    #[test]
    fn added_then_deleted_in_one_batch_is_not_fetched() {
        let s = store();
        let ev = vec![Event::Added { id: "n1".into(), labels: set(&["INBOX"]) }, Event::Deleted { id: "n1".into() }];
        assert!(apply(&s, &ev).unwrap().need_fetch.is_empty());
    }

    #[test]
    fn admit_respects_partial_window() {
        let s = store();
        // newer than oldest cached -> inside the window
        assert!(admit(&s, &row("n1", 400, &["INBOX"]), 0).unwrap());
        // older than the window's edge and window not complete -> not admitted
        assert!(!admit(&s, &row("old", 50, &["INBOX"]), 0).unwrap());
        // complete window admits anything
        s.set_window("INBOX", 100, true, 0).unwrap();
        assert!(admit(&s, &row("old", 50, &["INBOX"]), 0).unwrap());
        // not in any window and not cached -> not admitted
        assert!(!admit(&s, &row("z", 500, &["SENT"]), 0).unwrap());
    }
}
