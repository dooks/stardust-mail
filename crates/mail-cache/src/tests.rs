//! Synthetic-data tests for the cache. DPAPI is used for real (Windows only, like the whole app).

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use mail_model::MAX_CACHED_BODY;

static N: AtomicU32 = AtomicU32::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "gmail-ro-cache-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MARKER: &str = "SYNTHETIC-MARKER-7f3a";

fn acct() -> AccountId {
    AccountId::from("1234567890")
}

fn setup() -> (Tmp, CacheRoot, AccountCache) {
    let t = Tmp::new();
    let root = CacheRoot::open(&t.0).unwrap();
    let c = root.account(&acct()).unwrap();
    (t, root, c)
}

fn lab(v: &[&str]) -> Vec<LabelId> {
    v.iter().map(|s| LabelId::from(*s)).collect()
}

fn meta(id: &str, date: i64, labels: &[&str]) -> MessageMeta {
    MessageMeta {
        id: id.into(),
        thread_id: format!("t{id}").into(),
        label_ids: lab(labels),
        internal_date_ms: date,
        size_estimate: 1234,
        history_id: HistoryId(42),
        mime_type: "multipart/mixed".into(),
        from_raw: format!("From {MARKER} <a@example.com>"),
        to_raw: "b@example.com".into(),
        subject: format!("Subject {MARKER}"),
        date_raw: "Mon, 1 Jan 2024 00:00:00 +0000".into(),
        snippet_raw: format!("snippet {MARKER}"),
    }
}

fn win(label: &str, oldest: i64, complete: bool) -> Window {
    Window {
        label: label.into(),
        oldest_date: oldest,
        complete,
        next_page_token: if complete { None } else { Some("tok".into()) },
        as_of: 5,
    }
}

fn mid(s: &str) -> MessageId {
    s.into()
}

fn ids(v: &[MessageMeta]) -> Vec<String> {
    v.iter().map(|m| m.id.to_string()).collect()
}

fn labels_set(c: &AccountCache, id: &str) -> Vec<String> {
    c.label_ids(&mid(id))
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|l| l.to_string())
        .collect()
}

/// The S4 fixture: a, b, c in INBOX, partial INBOX window from 100.
fn s4_store() -> (Tmp, CacheRoot, AccountCache) {
    let (t, r, c) = setup();
    c.upsert_meta(
        &[
            meta("a", 100, &["INBOX", "UNREAD"]),
            meta("b", 200, &["INBOX"]),
            meta("c", 300, &["INBOX", "STARRED"]),
        ],
        0,
        UpsertMode::Always,
    )
    .unwrap();
    c.set_window(&win("INBOX", 100, false)).unwrap();
    (t, r, c)
}

#[test]
fn roundtrip_meta_body_cursor() {
    let (_t, _r, c) = setup();
    assert_eq!(c.cursor().unwrap(), None);
    assert_eq!(c.synced_at().unwrap(), None);
    let m = meta("m1", 1000, &["INBOX", "UNREAD"]);
    assert_eq!(
        c.upsert_meta(std::slice::from_ref(&m), 7, UpsertMode::Always)
            .unwrap(),
        1
    );
    let rows = c.rows(&"INBOX".into(), None, 10, false).unwrap();
    assert_eq!(rows.len(), 1);
    let mut want = m.clone();
    want.label_ids.sort();
    let mut got = rows[0].clone();
    got.label_ids.sort();
    assert_eq!(got, want);
    assert_eq!(
        c.has(&[mid("m1"), mid("zz")]).unwrap(),
        HashSet::from([mid("m1")])
    );
    c.set_cursor(HistoryId(99), 555).unwrap();
    assert_eq!(c.cursor().unwrap(), Some(HistoryId(99)));
    assert_eq!(c.synced_at().unwrap(), Some(555));
    assert_eq!(c.body(&mid("m1")).unwrap(), None);
    assert!(c.put_body(&mid("m1"), b"raw rfc822", 1).unwrap());
    assert_eq!(c.body(&mid("m1")).unwrap().unwrap(), b"raw rfc822");
    assert_eq!(c.label_ids(&mid("nope")).unwrap(), None);
}

#[test]
fn data_persists_across_reopen_and_no_plaintext_in_files() {
    let (t, r, c) = setup();
    c.upsert_meta(&[meta("m1", 1000, &["INBOX"])], 0, UpsertMode::Always)
        .unwrap();
    let body = format!("Subject: x\r\n\r\n{MARKER} body text").into_bytes();
    c.put_body(&mid("m1"), &body, 0).unwrap();
    // Look at the raw files while the connection is still open (WAL included).
    let dir = c.dir().to_path_buf();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let b = std::fs::read(e.path()).unwrap();
        assert!(
            !b.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
            "plaintext in {:?}",
            e.file_name()
        );
    }
    c.close().unwrap();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let b = std::fs::read(e.path()).unwrap();
        assert!(!b.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()));
    }
    let c = r.account(&acct()).unwrap();
    assert_eq!(
        c.rows(&"INBOX".into(), None, 10, false).unwrap()[0].subject,
        format!("Subject {MARKER}")
    );
    assert_eq!(c.body(&mid("m1")).unwrap().unwrap(), body);
    drop(t);
}

#[test]
fn tampering_and_wrong_row_id_are_detected() {
    let (_t, _r, c) = setup();
    c.upsert_meta(
        &[meta("m1", 1000, &["INBOX"]), meta("m2", 900, &["INBOX"])],
        0,
        UpsertMode::Always,
    )
    .unwrap();
    c.put_body(&mid("m1"), b"hello", 0).unwrap();
    {
        // Move m1's body blob under m2's id: the AAD no longer matches.
        let g = c.conn.lock().unwrap();
        g.as_ref()
            .unwrap()
            .execute(
                "INSERT INTO bodies(id,raw,fetched_at) SELECT 'm2',raw,0 FROM bodies WHERE id='m1'",
                [],
            )
            .unwrap();
    }
    assert!(matches!(c.body(&mid("m2")), Err(CacheError::Crypto(_))));
    assert_eq!(c.body(&mid("m1")).unwrap().unwrap(), b"hello");
}

#[test]
fn rows_order_before_limit_and_stale() {
    let (_t, _r, c) = setup();
    c.upsert_meta(
        &[
            meta("a", 100, &["INBOX"]),
            meta("c", 300, &["INBOX", "STARRED"]),
            meta("b", 200, &["INBOX"]),
            meta("s", 250, &["SENT"]),
        ],
        0,
        UpsertMode::Always,
    )
    .unwrap();
    let inbox = LabelId::from("INBOX");
    assert_eq!(
        ids(&c.rows(&inbox, None, 10, false).unwrap()),
        ["c", "b", "a"]
    );
    assert_eq!(ids(&c.rows(&inbox, None, 2, false).unwrap()), ["c", "b"]);
    assert_eq!(ids(&c.rows(&inbox, Some(200), 10, false).unwrap()), ["a"]);
    assert_eq!(
        ids(&c.rows(&"ALL".into(), None, 10, false).unwrap()),
        ["c", "s", "b", "a"]
    );
    assert_eq!(
        ids(&c.rows(&"STARRED".into(), None, 10, false).unwrap()),
        ["c"]
    );
    assert!(c.rows(&"TRASH".into(), None, 10, false).unwrap().is_empty());
}

#[test]
fn stale_handling() {
    let (_t, _r, c) = s4_store();
    assert_eq!(c.mark_all_stale_and_drop_windows().unwrap(), 3);
    assert!(c.window(&"INBOX".into()).unwrap().is_none());
    let inbox = LabelId::from("INBOX");
    assert!(c.rows(&inbox, None, 10, false).unwrap().is_empty());
    assert_eq!(
        ids(&c.rows(&inbox, None, 10, true).unwrap()),
        ["c", "b", "a"]
    );
    // has() still sees stale rows; a fresh upsert revalidates one.
    assert_eq!(c.has(&[mid("a")]).unwrap().len(), 1);
    c.upsert_meta(&[meta("b", 200, &["INBOX"])], 9, UpsertMode::Always)
        .unwrap();
    assert_eq!(ids(&c.rows(&inbox, None, 10, false).unwrap()), ["b"]);
    // Admit of a stale-but-cached row also revalidates it (no window needed).
    assert_eq!(
        c.upsert_meta(&[meta("a", 100, &["INBOX"])], 9, UpsertMode::Admit)
            .unwrap(),
        1
    );
    assert_eq!(ids(&c.rows(&inbox, None, 10, false).unwrap()), ["b", "a"]);
}

#[test]
fn window_semantics() {
    let (_t, _r, c) = setup();
    assert!(c.window(&"INBOX".into()).unwrap().is_none());
    let w = win("INBOX", 100, false);
    c.set_window(&w).unwrap();
    assert_eq!(c.window(&"INBOX".into()).unwrap().unwrap(), w);
    let w2 = win("INBOX", 50, true);
    c.set_window(&w2).unwrap();
    assert_eq!(c.window(&"INBOX".into()).unwrap().unwrap(), w2);

    // ALL window admits any label set (within its range).
    c.set_window(&win("ALL", 500, false)).unwrap();
    c.set_window(&win("INBOX", 100, false)).unwrap();
    let n = c
        .upsert_meta(
            &[meta("x", 600, &["SENT"]), meta("y", 400, &["SENT"])],
            0,
            UpsertMode::Admit,
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        c.has(&[mid("x"), mid("y")]).unwrap(),
        HashSet::from([mid("x")])
    );
}

#[test]
fn delta_parse_free_archive_star_delete_new_mail() {
    let (_t, _r, c) = s4_store();
    let ev = vec![
        HistoryEvent::LabelsRemoved {
            id: mid("a"),
            label_ids: lab(&["INBOX"]),
        },
        HistoryEvent::LabelsAdded {
            id: mid("b"),
            label_ids: lab(&["STARRED"]),
        },
        HistoryEvent::Deleted { id: mid("c") },
        HistoryEvent::Added {
            id: mid("n1"),
            thread_id: "tn1".into(),
            label_ids: lab(&["INBOX", "UNREAD"]),
        },
        HistoryEvent::Added {
            id: mid("x9"),
            thread_id: "tx9".into(),
            label_ids: lab(&["SENT"]),
        },
    ];
    let need = c.apply_events(&ev).unwrap();
    assert_eq!(need, vec![mid("n1")]);
    assert_eq!(labels_set(&c, "a"), ["UNREAD"]);
    assert_eq!(labels_set(&c, "b"), ["INBOX", "STARRED"]);
    assert!(c.label_ids(&mid("c")).unwrap().is_none());
    assert_eq!(
        ids(&c.rows(&"INBOX".into(), None, 10, false).unwrap()),
        ["b"]
    );
}

#[test]
fn delta_deleted_removes_body_and_replay_is_idempotent() {
    let (_t, _r, c) = s4_store();
    c.put_body(&mid("c"), b"x", 0).unwrap();
    let ev = vec![
        HistoryEvent::LabelsAdded {
            id: mid("b"),
            label_ids: lab(&["STARRED"]),
        },
        HistoryEvent::LabelsRemoved {
            id: mid("a"),
            label_ids: lab(&["INBOX"]),
        },
        HistoryEvent::Deleted { id: mid("c") },
    ];
    c.apply_events(&ev).unwrap();
    assert_eq!(c.body(&mid("c")).unwrap(), None);
    let before: Vec<_> = ["a", "b"].iter().map(|i| labels_set(&c, i)).collect();
    assert!(c.apply_events(&ev).unwrap().is_empty());
    let after: Vec<_> = ["a", "b"].iter().map(|i| labels_set(&c, i)).collect();
    assert_eq!(before, after);
}

#[test]
fn delta_added_then_deleted_in_one_batch_is_not_fetched() {
    let (_t, _r, c) = s4_store();
    let ev = vec![
        HistoryEvent::Added {
            id: mid("n1"),
            thread_id: "t".into(),
            label_ids: lab(&["INBOX"]),
        },
        HistoryEvent::Deleted { id: mid("n1") },
    ];
    assert!(c.apply_events(&ev).unwrap().is_empty());
    // Same for an added message that loses its label before the batch ends.
    let ev = vec![
        HistoryEvent::Added {
            id: mid("n2"),
            thread_id: "t".into(),
            label_ids: lab(&["INBOX"]),
        },
        HistoryEvent::LabelsRemoved {
            id: mid("n2"),
            label_ids: lab(&["INBOX"]),
        },
    ];
    assert!(c.apply_events(&ev).unwrap().is_empty());
}

#[test]
fn admit_respects_partial_window() {
    let (_t, _r, c) = s4_store();
    let adm = |m: MessageMeta| c.upsert_meta(&[m], 0, UpsertMode::Admit).unwrap();
    assert_eq!(adm(meta("n1", 400, &["INBOX"])), 1);
    assert_eq!(adm(meta("old", 50, &["INBOX"])), 0);
    c.set_window(&win("INBOX", 100, true)).unwrap();
    assert_eq!(adm(meta("old", 50, &["INBOX"])), 1);
    assert_eq!(adm(meta("z", 500, &["SENT"])), 0);
    // Always stores regardless.
    assert_eq!(
        c.upsert_meta(&[meta("z", 500, &["SENT"])], 0, UpsertMode::Always)
            .unwrap(),
        1
    );
}

#[test]
fn body_cap() {
    let (_t, _r, c) = setup();
    let ok = vec![0u8; MAX_CACHED_BODY];
    let big = vec![0u8; MAX_CACHED_BODY + 1];
    assert!(c.put_body(&mid("a"), &ok, 0).unwrap());
    assert!(!c.put_body(&mid("b"), &big, 0).unwrap());
    assert_eq!(c.body(&mid("b")).unwrap(), None);
    assert_eq!(c.body(&mid("a")).unwrap().unwrap().len(), MAX_CACHED_BODY);
}

const DAY: i64 = 86_400_000;

#[test]
fn evict_bodies_lru_with_keep_rules() {
    let (_t, _r, c) = setup();
    let now = 100 * DAY;
    // 30 INBOX messages with bodies (dates 1..=30), 5 SENT ones; each body ~1 KiB.
    let mut metas = Vec::new();
    for i in 1..=30 {
        metas.push(meta(&format!("i{i:02}"), i, &["INBOX"]));
    }
    for i in 1..=5 {
        metas.push(meta(&format!("s{i}"), 1000 + i, &["SENT"]));
    }
    c.upsert_meta(&metas, 0, UpsertMode::Always).unwrap();
    for m in &metas {
        c.put_body(&m.id, &[7u8; 1000], 10).unwrap(); // fetched at 10 (old)
    }
    // s1 opened 1 day ago -> protected; s2 opened 30 days ago -> not protected but more recent than the rest.
    c.touch_opened(&mid("s1"), now - DAY).unwrap();
    c.touch_opened(&mid("s2"), now - 30 * DAY).unwrap();
    // Cap that forces removal of nearly everything evictable.
    let rep = c.evict_with(now, 1, 10_000).unwrap();
    let kept: Vec<String> = metas
        .iter()
        .filter(|m| c.body(&m.id).unwrap().is_some())
        .map(|m| m.id.to_string())
        .collect();
    // Newest 20 INBOX (i11..=i30) and s1 (opened recently) survive; the rest go.
    let mut want: Vec<String> = (11..=30).map(|i| format!("i{i:02}")).collect();
    want.push("s1".into());
    assert_eq!(kept, want);
    assert_eq!(rep.bodies_removed, 35 - 21);
    assert_eq!(rep.rows_removed, 0);
    // LRU order: with a cap allowing a few more bodies than the protected set, oldest-touched go first.
    let (_t2, _r2, c2) = setup();
    c2.upsert_meta(
        &[
            meta("p", 5, &["SENT"]),
            meta("q", 6, &["SENT"]),
            meta("r", 7, &["SENT"]),
        ],
        0,
        UpsertMode::Always,
    )
    .unwrap();
    c2.put_body(&mid("p"), &[1u8; 1000], 1).unwrap();
    c2.put_body(&mid("q"), &[1u8; 1000], 2).unwrap();
    c2.put_body(&mid("r"), &[1u8; 1000], 3).unwrap();
    c2.touch_opened(&mid("p"), 50 * DAY).unwrap(); // p is the most recently used of the old ones
    let cap = 2 * (1000 + 28) as u64; // room for two bodies
    c2.evict_with(100 * DAY, cap, 10_000).unwrap();
    assert!(c2.body(&mid("p")).unwrap().is_some());
    assert!(c2.body(&mid("q")).unwrap().is_none()); // least recently used
    assert!(c2.body(&mid("r")).unwrap().is_some());
}

#[test]
fn evict_rows_per_label_moves_window_edge() {
    let (_t, _r, c) = setup();
    let metas: Vec<_> = (1..=10)
        .map(|i| meta(&format!("m{i:02}"), i * 10, &["INBOX"]))
        .collect();
    c.upsert_meta(&metas, 0, UpsertMode::Always).unwrap();
    c.put_body(&mid("m01"), b"b", 0).unwrap();
    c.set_window(&win("INBOX", 10, false)).unwrap();
    c.set_window(&win("SENT", 10, true)).unwrap();
    let rep = c.evict_with(0, u64::MAX, 4).unwrap();
    assert_eq!(rep.rows_removed, 6);
    let rows = c.rows(&"INBOX".into(), None, 100, false).unwrap();
    assert_eq!(ids(&rows), ["m10", "m09", "m08", "m07"]);
    assert_eq!(c.body(&mid("m01")).unwrap(), None);
    let w = c.window(&"INBOX".into()).unwrap().unwrap();
    assert_eq!(w.oldest_date, 70);
    assert!(!w.complete);
    // An unrelated label's window is untouched.
    assert!(c.window(&"SENT".into()).unwrap().unwrap().complete);
    // Under the cap: nothing happens.
    assert_eq!(
        c.evict_with(0, u64::MAX, 4).unwrap(),
        EvictReport::default()
    );
}

#[test]
fn account_id_validation() {
    let t = Tmp::new();
    let root = CacheRoot::open(&t.0).unwrap();
    for bad in ["", "..", "a/b", "a\\b", "x.deleting-1", "a b", "C:evil"] {
        assert!(root.account(&bad.into()).is_err(), "{bad}");
        assert!(root.delete_account(&bad.into()).is_err(), "{bad}");
    }
    assert!(root.account(&"12-34_ab".into()).is_ok());
}

#[test]
fn delete_refused_while_open_then_rename_sweep_no_strays() {
    let (t, root, c) = setup();
    c.upsert_meta(&[meta("m1", 1, &["INBOX"])], 0, UpsertMode::Always)
        .unwrap();
    assert!(matches!(
        root.delete_account(&acct()),
        Err(CacheError::Locked)
    ));
    assert!(matches!(root.account(&acct()), Err(CacheError::Locked)));
    let dir = c.dir().to_path_buf();
    assert!(dir.join("cache.db").exists());
    c.close().unwrap();
    // After a clean close there are no WAL/SHM files.
    for s in ["cache.db-wal", "cache.db-shm"] {
        assert!(!dir.join(s).exists(), "{s} left behind");
    }
    root.delete_account(&acct()).unwrap();
    assert!(!dir.exists());
    let left: Vec<_> = std::fs::read_dir(&t.0).unwrap().flatten().collect();
    assert!(left.is_empty(), "stray entries");
    // Deleting a missing account is fine; reopening gives a fresh, empty cache with a new key.
    root.delete_account(&acct()).unwrap();
    let c = root.account(&acct()).unwrap();
    assert!(c.rows(&"ALL".into(), None, 10, true).unwrap().is_empty());
}

#[test]
fn drop_releases_the_account_and_sweep_removes_tombstones() {
    let (t, root, c) = setup();
    drop(c);
    // Simulate a crash between rename and remove.
    std::fs::create_dir_all(t.0.join("999.deleting-0")).unwrap();
    std::fs::write(t.0.join("999.deleting-0").join("cache.db"), b"x").unwrap();
    std::fs::create_dir_all(t.0.join("999.deleting-3")).unwrap();
    assert_eq!(root.sweep(), 2);
    assert_eq!(root.sweep(), 0);
    root.delete_account(&acct()).unwrap();
}

#[test]
fn wrong_account_key_cannot_read() {
    // A key swapped in from another account fails authentication (AAD/key mismatch).
    let (t, root, c) = setup();
    c.upsert_meta(&[meta("m1", 1, &["INBOX"])], 0, UpsertMode::Always)
        .unwrap();
    c.close().unwrap();
    let other = root.account(&"222".into()).unwrap();
    other.close().unwrap();
    std::fs::copy(
        t.0.join("222").join("key.dpapi"),
        t.0.join("1234567890").join("key.dpapi"),
    )
    .unwrap();
    let c = root.account(&acct()).unwrap();
    assert!(matches!(
        c.rows(&"INBOX".into(), None, 10, false),
        Err(CacheError::Crypto(_))
    ));
}

#[test]
#[ignore = "benchmark: prints timings only"]
fn bench_1000_rows() {
    use std::time::Instant;
    let (_t, _r, c) = setup();
    let metas: Vec<_> = (0..1000)
        .map(|i| meta(&format!("m{i:04}"), i, &["INBOX"]))
        .collect();
    let t0 = Instant::now();
    c.upsert_meta(&metas, 0, UpsertMode::Always).unwrap();
    println!("upsert 1000 rows: {:?}", t0.elapsed());
    let t0 = Instant::now();
    let rows = c.rows(&"INBOX".into(), None, 1000, false).unwrap();
    println!("read 1000 rows: {:?} ({} rows)", t0.elapsed(), rows.len());
    let body = vec![b'a'; 30_000];
    let t0 = Instant::now();
    for m in metas.iter().take(200) {
        c.put_body(&m.id, &body, 0).unwrap();
    }
    println!("put 200 bodies of 30 KB: {:?}", t0.elapsed());
    let t0 = Instant::now();
    for m in metas.iter().take(200) {
        c.body(&m.id).unwrap().unwrap();
    }
    println!("get 200 bodies: {:?}", t0.elapsed());
}

#[test]
fn meta_roundtrip_with_stale_flag() {
    let (_t, _r, c) = s4_store();
    assert!(c.meta(&mid("zzz")).unwrap().is_none());
    let (m, stale) = c.meta(&mid("c")).unwrap().unwrap();
    assert!(!stale);
    assert_eq!(m, meta("c", 300, &["INBOX", "STARRED"]));
    // Resync marks it stale; meta() still returns it, flagged.
    c.mark_all_stale_and_drop_windows().unwrap();
    let (m, stale) = c.meta(&mid("c")).unwrap().unwrap();
    assert!(stale);
    assert_eq!(m.internal_date_ms, 300);
    // A fresh upsert clears the flag.
    c.upsert_meta(&[meta("c", 300, &["INBOX"])], 9, UpsertMode::Always).unwrap();
    let (m, stale) = c.meta(&mid("c")).unwrap().unwrap();
    assert!(!stale);
    assert_eq!(m.label_ids, lab(&["INBOX"]));
    // A deleted row is gone.
    c.apply_events(&[HistoryEvent::Deleted { id: mid("c") }]).unwrap();
    assert!(c.meta(&mid("c")).unwrap().is_none());
}
