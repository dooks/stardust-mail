//! The folder tree (PLAN §1 Folders). Pure; ported from spike S2's `build_tree`.

use std::collections::BTreeMap;

use crate::gmail::{LabelInfo, LabelKind};
use crate::label;
use crate::view::{FolderKind, FolderView};

const FIXED: [(&str, &str, FolderKind); 7] = [
    (label::INBOX, "Inbox", FolderKind::Inbox),
    (label::STARRED, "Starred", FolderKind::Starred),
    (label::SENT, "Sent", FolderKind::Sent),
    (label::DRAFT, "Drafts", FolderKind::Drafts),
    (label::ALL_MAIL, "All Mail", FolderKind::AllMail),
    (label::SPAM, "Spam", FolderKind::Spam),
    (label::TRASH, "Trash", FolderKind::Trash),
];

const CATEGORY_NAMES: [&str; 5] = ["Personal", "Updates", "Promotions", "Social", "Forums"];

fn depth_u8(d: usize) -> u8 {
    u8::try_from(d).unwrap_or(u8::MAX)
}

/// Build the flattened folder list in display order, with every `unread` set to `None` (the caller fills counts).
///
/// Order: Inbox, Starred, Sent, Drafts, All Mail (`id = "ALL"`), Spam, Trash, the clickable-less "Categories"
/// group followed by its five children (depth 1, `CATEGORY_*` ids), then user labels depth-first, sorted
/// case-insensitively and nested by `/`. A parent that exists only as a name prefix becomes a non-clickable
/// `Group` row with id `group:<path>`. The fixed system folders and categories are always present, whether or
/// not `labels` lists them; UNREAD, IMPORTANT, CHAT, YELLOW_STAR and all other system labels are never shown.
pub fn build_tree(labels: &[LabelInfo]) -> Vec<FolderView> {
    let mut out = Vec::with_capacity(FIXED.len() + 6 + labels.len());
    let folder = |id: &str, name: &str, kind, depth, clickable| FolderView {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        depth,
        unread: None,
        clickable,
    };
    for (id, name, kind) in FIXED {
        out.push(folder(id, name, kind, 0, true));
    }
    out.push(folder(label::CATEGORY_GROUP, "Categories", FolderKind::CategoryGroup, 0, false));
    for (id, name) in label::CATEGORIES.iter().zip(CATEGORY_NAMES) {
        out.push(folder(id, name, FolderKind::Category, 1, true));
    }

    // Key: (lower-cased segments, original path) gives a depth-first, case-insensitive order.
    struct Node {
        name: String,
        depth: usize,
        label_id: Option<String>,
        path: String,
    }
    let mut map: BTreeMap<(Vec<String>, String), Node> = BTreeMap::new();
    for l in labels.iter().filter(|l| l.kind == LabelKind::User) {
        let segs: Vec<&str> = l.name.split('/').filter(|s| !s.is_empty()).collect();
        for i in 1..=segs.len() {
            let part = &segs[..i];
            let key = (part.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>(), part.join("/"));
            let node = map.entry(key).or_insert_with(|| Node {
                name: part[i - 1].to_string(),
                depth: i - 1,
                label_id: None,
                path: part.join("/"),
            });
            if i == segs.len() {
                node.label_id = Some(l.id.0.clone());
            }
        }
    }
    for n in map.into_values() {
        match n.label_id {
            Some(id) => out.push(folder(&id, &n.name, FolderKind::User, depth_u8(n.depth), true)),
            None => out.push(folder(&format!("{}{}", label::GROUP_PREFIX, n.path), &n.name, FolderKind::Group, depth_u8(n.depth), false)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::LabelId;

    fn user(id: &str, name: &str) -> LabelInfo {
        LabelInfo { id: LabelId::new(id), name: name.into(), kind: LabelKind::User }
    }

    fn system(id: &str) -> LabelInfo {
        LabelInfo { id: LabelId::new(id), name: id.into(), kind: LabelKind::System }
    }

    #[test]
    fn fixed_order_and_categories() {
        let t = build_tree(&[]);
        let ids: Vec<&str> = t.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "INBOX",
                "STARRED",
                "SENT",
                "DRAFT",
                "ALL",
                "SPAM",
                "TRASH",
                "group:categories",
                "CATEGORY_PERSONAL",
                "CATEGORY_UPDATES",
                "CATEGORY_PROMOTIONS",
                "CATEGORY_SOCIAL",
                "CATEGORY_FORUMS"
            ]
        );
        let all = t.iter().find(|f| f.id == "ALL").unwrap();
        assert_eq!(all.kind, FolderKind::AllMail);
        let group = t.iter().find(|f| f.kind == FolderKind::CategoryGroup).unwrap();
        assert!(!group.clickable);
        assert!(t.iter().filter(|f| f.kind == FolderKind::Category).all(|f| f.depth == 1 && f.clickable));
        assert!(t.iter().all(|f| f.unread.is_none()));
    }

    #[test]
    fn user_labels_nest_with_groups() {
        let t = build_tree(&[
            user("L1", "Work/Projects/Alpha"),
            user("L2", "Work"),
            user("L3", "home/Bills"),
            user("L4", "Zed"),
            user("L5", "alpha"),
        ]);
        let users: Vec<_> = t.iter().skip(13).map(|f| (f.id.as_str(), f.name.as_str(), f.depth, f.kind, f.clickable)).collect();
        assert_eq!(
            users,
            vec![
                ("L5", "alpha", 0, FolderKind::User, true),
                ("group:home", "home", 0, FolderKind::Group, false),
                ("L3", "Bills", 1, FolderKind::User, true),
                ("L2", "Work", 0, FolderKind::User, true),
                ("group:Work/Projects", "Projects", 1, FolderKind::Group, false),
                ("L1", "Alpha", 2, FolderKind::User, true),
                ("L4", "Zed", 0, FolderKind::User, true),
            ]
        );
    }

    #[test]
    fn hidden_and_unknown_system_labels_are_excluded() {
        let t = build_tree(&[
            system("UNREAD"),
            system("IMPORTANT"),
            system("CHAT"),
            system("YELLOW_STAR"),
            system("INBOX"),
            system("CATEGORY_PERSONAL"),
            system("SOMETHING_NEW"),
        ]);
        assert_eq!(t.len(), 13);
        assert!(t.iter().all(|f| !matches!(f.id.as_str(), "UNREAD" | "IMPORTANT" | "CHAT" | "YELLOW_STAR" | "SOMETHING_NEW")));
        assert_eq!(t.iter().filter(|f| f.id == "INBOX").count(), 1);
    }

    #[test]
    fn odd_names_do_not_panic() {
        let t = build_tree(&[user("A", ""), user("B", "/"), user("C", "x//y"), user("D", "x/")]);
        let ids: Vec<&str> = t.iter().skip(13).map(|f| f.id.as_str()).collect();
        // "" and "/" have no segments; "x//y" -> x/y under a group; "x/" -> the label x itself.
        assert_eq!(ids, vec!["D", "C"]);
        assert_eq!(t[14].depth, 1);
    }
}
