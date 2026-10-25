//! Q7: atomic removal of an account's cache directory.
//!
//! Layout: `<root>/<account>/cache.db` (+ `-wal`, `-shm` while open) and `<root>/<account>/key.dpapi`.
//! Sign-out = close the store, rename the directory to `<account>.deleting-N` (one atomic NTFS
//! rename: the account vanishes from the cache root in a single step), then delete the renamed
//! tree. A crash between the two leaves only a `*.deleting-*` directory that the next start sweeps.

use std::io;
use std::path::{Path, PathBuf};

pub fn account_dir(root: &Path, account: &str) -> PathBuf {
    root.join(account)
}

/// Rename-then-delete. Fails (and leaves the cache intact) if any handle is still open in the tree.
pub fn remove_account_cache(root: &Path, account: &str) -> io::Result<()> {
    let dir = account_dir(root, account);
    if !dir.exists() {
        return Ok(());
    }
    let mut n = 0u32;
    let tomb = loop {
        let t = root.join(format!("{account}.deleting-{n}"));
        if !t.exists() {
            break t;
        }
        n += 1;
    };
    std::fs::rename(&dir, &tomb)?;
    std::fs::remove_dir_all(&tomb)
}

/// Startup sweep for tombstones left by a crash. Returns how many were removed.
pub fn sweep(root: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(".deleting-") && std::fs::remove_dir_all(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    n
}

pub fn list(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// Live demonstration on synthetic data; prints what happens. Returns true if everything held.
pub fn demo(root: &Path) -> bool {
    use crate::store::{Codec, Row, Store};
    let _ = std::fs::remove_dir_all(root);
    let acct = "synthetic_account";
    let dir = account_dir(root, acct);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("cache.db");
    let st = Store::open(&db, None, Codec::Plain).unwrap();
    let reader = rusqlite::Connection::open(&db).unwrap();
    st.begin().unwrap();
    for i in 0..100 {
        let r = Row { id: format!("id{i}"), thread_id: "t".into(), internal_date: i, history_id: i, size_estimate: 1, labels: ["INBOX".to_string()].into(), meta: vec![b'x'; 500] };
        st.upsert(&r, 0).unwrap();
    }
    st.commit().unwrap();
    let n: i64 = reader.query_row("SELECT COUNT(*) FROM msgs", [], |r| r.get(0)).unwrap();
    println!("open, 2 connections, rows visible to reader={n}; files: {:?}", list(&dir));

    let busy = remove_account_cache(root, acct);
    println!("remove while connections open: {} ; account dir still intact={}", busy.as_ref().map_or_else(|e| format!("refused ({:?})", e.kind()), |_| "succeeded".into()), dir.exists());
    let refused_ok = busy.is_err() && dir.exists();

    drop(reader);
    st.close().unwrap();
    println!("after closing every connection (checkpoint TRUNCATE): files: {:?}", list(&dir));

    // Simulated crash after the rename, before the delete.
    let tomb = root.join(format!("{acct}.deleting-0"));
    std::fs::rename(&dir, &tomb).unwrap();
    println!("simulated crash after rename: root entries {:?}; account dir exists={}", list(root), dir.exists());
    let swept = sweep(root);
    println!("startup sweep removed {swept} tombstone(s); root entries {:?}", list(root));
    let sweep_ok = swept == 1 && list(root).is_empty();

    // Normal path.
    std::fs::create_dir_all(&dir).unwrap();
    let st = Store::open(&db, None, Codec::Plain).unwrap();
    st.kv_set("a", "b").unwrap();
    st.close().unwrap();
    let t = std::time::Instant::now();
    remove_account_cache(root, acct).unwrap();
    println!("normal sign-out: removed in {:.1} ms; root entries {:?}", t.elapsed().as_secs_f64() * 1000.0, list(root));
    let normal_ok = list(root).is_empty();

    // Dropping a Store without close(): does the WAL/SHM get cleaned by the last connection closing?
    std::fs::create_dir_all(&dir).unwrap();
    let st = Store::open(&db, None, Codec::Plain).unwrap();
    st.kv_set("a", "b").unwrap();
    drop(st);
    println!("plain drop (no explicit checkpoint): files {:?}", list(&dir));
    let drop_ok = remove_account_cache(root, acct).is_ok() && list(root).is_empty();
    let _ = std::fs::remove_dir_all(root);
    refused_ok && sweep_ok && normal_ok && drop_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Codec, Store};

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("s4-signout-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn removal_leaves_nothing_behind() {
        let root = tmp("a");
        let dir = account_dir(&root, "acct");
        std::fs::create_dir_all(&dir).unwrap();
        let st = Store::open(&dir.join("cache.db"), None, Codec::Plain).unwrap();
        st.kv_set("k", "v").unwrap();
        st.close().unwrap();
        std::fs::write(dir.join("key.dpapi"), b"x").unwrap();
        remove_account_cache(&root, "acct").unwrap();
        assert!(list(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_connection_blocks_removal_and_keeps_cache() {
        let root = tmp("b");
        let dir = account_dir(&root, "acct");
        std::fs::create_dir_all(&dir).unwrap();
        let st = Store::open(&dir.join("cache.db"), None, Codec::Plain).unwrap();
        assert!(remove_account_cache(&root, "acct").is_err());
        assert!(dir.join("cache.db").exists());
        st.close().unwrap();
        remove_account_cache(&root, "acct").unwrap();
        assert!(list(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sweep_removes_tombstones() {
        let root = tmp("c");
        std::fs::create_dir_all(root.join("acct.deleting-3")).unwrap();
        assert_eq!(sweep(&root), 1);
        assert!(list(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
