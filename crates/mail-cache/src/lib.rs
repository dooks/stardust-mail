//! Per-account encrypted cache (PLAN §1 Cache, §3.3, WP-C).
//!
//! Layout: `<root>\<sub>\cache.db` (SQLite, WAL) + `<root>\<sub>\key.dpapi` (32-byte key, DPAPI with entropy
//! `gmail-ro-cache-key`). Headers, snippet and raw bodies are ChaCha20-Poly1305 blobs (random 12-byte nonce
//! prepended, the row id as AAD). Ids, label ids, dates and sizes are in the clear.
//!
//! Schema (created by `CacheRoot::account`):
//! ```sql
//! meta(k PRIMARY KEY, v)                                  -- history_id, synced_at, schema_version
//! msgs(id PK, thread_id, internal_date, size_estimate, mime_type, fetched_at, stale INT, meta BLOB)
//! msg_labels(id, label, PRIMARY KEY(id,label)) WITHOUT ROWID;  CREATE INDEX msg_labels_label ON msg_labels(label, id)
//! bodies(id PK, raw BLOB, fetched_at, last_opened)
//! windows(label PK, oldest_date, complete INT, next_page_token, as_of)
//! ```
//! `msgs.meta` = AEAD(JSON of the `MessageMeta` text fields: from, to, subject, date, snippet, history id).
//!
//! Privacy: log ids, counts and sizes only.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use mail_model::label::ALL_MAIL;
use mail_model::{
    AccountId, CacheError, EvictReport, HistoryEvent, HistoryId, LabelId, MAX_CACHED_BODY,
    MailCache, MessageId, MessageMeta, ThreadId, UpsertMode, Window,
};
use rusqlite::{Connection, OptionalExtension, params};

#[cfg(test)]
mod tests;

/// DPAPI entropy of `key.dpapi`.
pub const KEY_ENTROPY: &[u8] = b"gmail-ro-cache-key";
/// Schema version stored in `meta`.
pub const SCHEMA_VERSION: u32 = 1;
/// Body cap per account (bytes of raw RFC 822, before encryption).
pub const MAX_BODY_BYTES_PER_ACCOUNT: u64 = 200 * 1024 * 1024;
/// List rows kept per label.
pub const MAX_ROWS_PER_LABEL: usize = 2000;
/// Bodies opened within this many days are never evicted.
pub const KEEP_OPENED_DAYS: i64 = 7;
/// The newest this many INBOX bodies are never evicted.
pub const KEEP_NEWEST_INBOX: usize = 20;

const DAY_MS: i64 = 86_400_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS msgs(
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    internal_date INTEGER NOT NULL,
    size_estimate INTEGER NOT NULL,
    mime_type TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    meta BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS msgs_date ON msgs(internal_date DESC);
CREATE TABLE IF NOT EXISTS msg_labels(id TEXT NOT NULL, label TEXT NOT NULL, PRIMARY KEY(id, label)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS msg_labels_label ON msg_labels(label, id);
CREATE TABLE IF NOT EXISTS bodies(
    id TEXT PRIMARY KEY,
    raw BLOB NOT NULL,
    fetched_at INTEGER NOT NULL,
    last_opened INTEGER
);
CREATE TABLE IF NOT EXISTS windows(
    label TEXT PRIMARY KEY,
    oldest_date INTEGER NOT NULL,
    complete INTEGER NOT NULL,
    next_page_token TEXT,
    as_of INTEGER NOT NULL
);
";

fn db<E: std::fmt::Display>(e: E) -> CacheError {
    CacheError::Db(e.to_string())
}

fn io<E: std::fmt::Display>(e: E) -> CacheError {
    CacheError::Io(e.to_string())
}

fn crypto<E: std::fmt::Display>(e: E) -> CacheError {
    CacheError::Crypto(e.to_string())
}

/// Account ids are Google `sub` values: digits in practice. Anything else could escape the cache root.
fn validate_id(id: &AccountId) -> Result<&str, CacheError> {
    let s = id.as_str();
    if !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(s)
    } else {
        Err(CacheError::Io("invalid account id".into()))
    }
}

/// The directory that holds every account's cache directory.
#[derive(Debug, Clone)]
pub struct CacheRoot {
    dir: PathBuf,
    /// Accounts whose `AccountCache` is currently open (deletion is refused for these).
    open: Arc<Mutex<HashSet<String>>>,
}

impl CacheRoot {
    /// Use `dir` (`%LOCALAPPDATA%\com.ziggy.gmail-ro\cache`), creating it if needed.
    pub fn open(dir: &Path) -> Result<CacheRoot, CacheError> {
        std::fs::create_dir_all(dir).map_err(io)?;
        Ok(CacheRoot {
            dir: dir.to_path_buf(),
            open: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Open (creating directory, `key.dpapi` and schema on first use) one account's cache.
    /// A second open of an account that is still open is refused with `CacheError::Locked`.
    pub fn account(&self, id: &AccountId) -> Result<AccountCache, CacheError> {
        let name = validate_id(id)?.to_string();
        let dir = self.dir.join(&name);
        {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if !open.insert(name.clone()) {
                return Err(CacheError::Locked);
            }
        }
        // From here on, a failure must release the slot: build the struct late, undo on error.
        let built = (|| -> Result<(Connection, [u8; 32]), CacheError> {
            std::fs::create_dir_all(&dir).map_err(io)?;
            let key = load_or_create_key(&dir.join("key.dpapi"))?;
            let conn = Connection::open(dir.join("cache.db")).map_err(db)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(db)?;
            let _: String = conn
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .map_err(db)?;
            conn.pragma_update(None, "synchronous", "NORMAL")
                .map_err(db)?;
            conn.execute_batch(SCHEMA).map_err(db)?;
            let ver: Option<String> = conn
                .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(db)?;
            match ver {
                None => {
                    conn.execute(
                        "INSERT INTO meta(k,v) VALUES('schema_version',?)",
                        [SCHEMA_VERSION.to_string()],
                    )
                    .map_err(db)?;
                }
                Some(v) if v == SCHEMA_VERSION.to_string() => {}
                Some(v) => return Err(CacheError::Db(format!("unsupported schema_version {v}"))),
            }
            Ok((conn, key))
        })();
        match built {
            Ok((conn, key)) => {
                tracing::debug!(account = %name, "cache opened");
                Ok(AccountCache {
                    id: id.clone(),
                    dir,
                    conn: Mutex::new(Some(conn)),
                    key,
                    open: self.open.clone(),
                })
            }
            Err(e) => {
                self.open
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&name);
                Err(e)
            }
        }
    }

    /// Delete an account's cache: rename `<id>` to `<id>.deleting-N`, then `remove_dir_all`. Refused with
    /// `CacheError::Locked` while an `AccountCache` for it is open (close it first). Leaves no WAL/SHM strays.
    ///
    /// If the rename succeeded but removal fails (e.g. a virus scanner holds a file), this still returns `Ok`:
    /// the account is gone from the cache's point of view and the next [`CacheRoot::sweep`] removes the rest.
    pub fn delete_account(&self, id: &AccountId) -> Result<(), CacheError> {
        let name = validate_id(id)?.to_string();
        // Hold the lock for the whole operation so nobody can open the account in between.
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if open.contains(&name) {
            return Err(CacheError::Locked);
        }
        let dir = self.dir.join(&name);
        if !dir.exists() {
            return Ok(());
        }
        let mut n = 0u32;
        let tomb = loop {
            let t = self.dir.join(format!("{name}.deleting-{n}"));
            if !t.exists() {
                break t;
            }
            n += 1;
        };
        std::fs::rename(&dir, &tomb).map_err(io)?;
        if let Err(e) = std::fs::remove_dir_all(&tomb) {
            tracing::warn!(account = %name, error = %e, "cache removal deferred to the next sweep");
        }
        drop(open);
        Ok(())
    }

    /// Startup sweep: remove leftover `*.deleting-N` directories. Returns how many were removed.
    pub fn sweep(&self) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(".deleting-")
                && std::fs::remove_dir_all(e.path()).is_ok()
            {
                removed += 1;
            }
        }
        removed
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32], CacheError> {
    if path.exists() {
        let blob = std::fs::read(path).map_err(io)?;
        let raw = win_dpapi::unprotect(&blob, KEY_ENTROPY).map_err(crypto)?;
        return raw
            .try_into()
            .map_err(|_| CacheError::Crypto("bad key length".into()));
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(crypto)?;
    let blob = win_dpapi::protect(&key, KEY_ENTROPY).map_err(crypto)?;
    let tmp = path.with_extension("dpapi.tmp");
    std::fs::write(&tmp, blob).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    Ok(key)
}

/// One account's open cache. Implements [`MailCache`]; `Send + Sync` (the connection sits behind a mutex).
pub struct AccountCache {
    id: AccountId,
    dir: PathBuf,
    conn: Mutex<Option<Connection>>,
    key: [u8; 32],
    open: Arc<Mutex<HashSet<String>>>,
}

impl Drop for AccountCache {
    fn drop(&mut self) {
        // Close the connection first, then release the account for deletion.
        if let Ok(mut g) = self.conn.lock()
            && let Some(c) = g.take()
        {
            let _ = c.close();
        }
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.id.as_str());
    }
}

/// Fields of `MessageMeta` that are encrypted.
fn meta_json(m: &MessageMeta) -> Vec<u8> {
    serde_json::json!({
        "from": m.from_raw, "to": m.to_raw, "subject": m.subject, "date": m.date_raw,
        "snippet": m.snippet_raw, "history_id": m.history_id.0,
    })
    .to_string()
    .into_bytes()
}

impl AccountCache {
    /// Checkpoint the WAL and close the connection (and mark the account as not open for deletion purposes).
    pub fn close(self) -> Result<(), CacheError> {
        let taken = self.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(c) = taken {
            let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
            c.close().map_err(|(_, e)| db(e))?;
        }
        // Drop releases the open-set slot.
        Ok(())
    }

    /// The account's directory (for diagnostics and tests).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<Connection>>, CacheError> {
        let g = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            return Err(CacheError::Db("cache is closed".into()));
        }
        Ok(g)
    }

    fn enc(&self, id: &str, plain: &[u8]) -> Result<Vec<u8>, CacheError> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(crypto)?;
        let c = ChaCha20Poly1305::new((&self.key).into());
        let mut out = nonce.to_vec();
        out.extend(
            c.encrypt(
                (&nonce).into(),
                Payload {
                    msg: plain,
                    aad: id.as_bytes(),
                },
            )
            .map_err(crypto)?,
        );
        Ok(out)
    }

    fn dec(&self, id: &str, blob: &[u8]) -> Result<Vec<u8>, CacheError> {
        if blob.len() < 12 + 16 {
            return Err(CacheError::Crypto("blob too short".into()));
        }
        let (nonce, ct) = blob.split_at(12);
        let c = ChaCha20Poly1305::new((&self.key).into());
        c.decrypt(
            nonce.into(),
            Payload {
                msg: ct,
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| CacheError::Crypto("decrypt failed (wrong key or tampered)".into()))
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_meta(
        &self,
        conn: &Connection,
        id: String,
        thread_id: String,
        internal_date_ms: i64,
        size_estimate: i64,
        mime_type: String,
        blob: Vec<u8>,
    ) -> Result<MessageMeta, CacheError> {
        let plain = self.dec(&id, &blob)?;
        let v: serde_json::Value = serde_json::from_slice(&plain).map_err(crypto)?;
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let label_ids = labels_of(conn, &id)?
            .into_iter()
            .map(LabelId::from)
            .collect();
        Ok(MessageMeta {
            id: MessageId::from(id),
            thread_id: ThreadId::from(thread_id),
            label_ids,
            internal_date_ms,
            size_estimate: size_estimate.max(0) as u64,
            history_id: HistoryId(v["history_id"].as_u64().unwrap_or(0)),
            mime_type,
            from_raw: s("from"),
            to_raw: s("to"),
            subject: s("subject"),
            date_raw: s("date"),
            snippet_raw: s("snippet"),
        })
    }

    /// `evict` with explicit caps (tests use small ones).
    pub(crate) fn evict_with(
        &self,
        now_ms: i64,
        body_cap: u64,
        rows_per_label: usize,
    ) -> Result<EvictReport, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let tx = conn.unchecked_transaction().map_err(db)?;
        let mut rep = EvictReport::default();

        // 1. Bodies: LRU by last-opened (else fetch time), skipping protected ones.
        let total: i64 = tx
            .query_row("SELECT COALESCE(SUM(LENGTH(raw)),0) FROM bodies", [], |r| {
                r.get(0)
            })
            .map_err(db)?;
        let mut total = total.max(0) as u64;
        if total > body_cap {
            let recent = now_ms - KEEP_OPENED_DAYS * DAY_MS;
            let mut st = tx
                .prepare(
                    "SELECT b.id, LENGTH(b.raw) FROM bodies b
                     WHERE COALESCE(b.last_opened, 0) < ?1
                       AND b.id NOT IN (
                           SELECT b2.id FROM bodies b2
                           JOIN msg_labels l ON l.id = b2.id AND l.label = 'INBOX'
                           JOIN msgs m ON m.id = b2.id
                           ORDER BY m.internal_date DESC LIMIT ?2)
                     ORDER BY COALESCE(b.last_opened, b.fetched_at) ASC, b.fetched_at ASC",
                )
                .map_err(db)?;
            let victims: Vec<(String, i64)> = st
                .query_map(params![recent, KEEP_NEWEST_INBOX as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(db)?
                .collect::<Result<_, _>>()
                .map_err(db)?;
            drop(st);
            for (id, len) in victims {
                if total <= body_cap {
                    break;
                }
                tx.execute("DELETE FROM bodies WHERE id=?", [&id])
                    .map_err(db)?;
                total = total.saturating_sub(len.max(0) as u64);
                rep.bodies_removed += 1;
            }
        }

        // 2. Rows: per label keep the newest `rows_per_label` (plus ties at the edge); move windows up.
        let labels: Vec<(String, i64)> = {
            let mut st = tx
                .prepare(
                    "SELECT label, COUNT(*) FROM msg_labels GROUP BY label HAVING COUNT(*) > ?",
                )
                .map_err(db)?;
            st.query_map([rows_per_label as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?
                .collect::<Result<_, _>>()
                .map_err(db)?
        };
        let mut edges: HashMap<String, i64> = HashMap::new(); // label -> newest deleted date
        let mut victims: BTreeSet<String> = BTreeSet::new();
        for (label, _) in &labels {
            let cutoff: i64 = tx
                .query_row(
                    "SELECT m.internal_date FROM msg_labels l JOIN msgs m ON m.id=l.id WHERE l.label=?
                     ORDER BY m.internal_date DESC LIMIT 1 OFFSET ?",
                    params![label, rows_per_label as i64 - 1],
                    |r| r.get(0),
                )
                .map_err(db)?;
            let mut st = tx
                .prepare(
                    "SELECT m.id, m.internal_date FROM msg_labels l JOIN msgs m ON m.id=l.id
                     WHERE l.label=? AND m.internal_date < ?",
                )
                .map_err(db)?;
            let old: Vec<(String, i64)> = st
                .query_map(params![label, cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?
                .collect::<Result<_, _>>()
                .map_err(db)?;
            drop(st);
            if old.is_empty() {
                continue;
            }
            let newest_gone = old.iter().map(|(_, d)| *d).max().unwrap_or(cutoff);
            edges.insert(label.clone(), cutoff - 1);
            // A deleted row leaves a hole in every other list it was in: record that too.
            for (id, d) in &old {
                for other in labels_of(&tx, id)? {
                    let e = edges.entry(other).or_insert(*d);
                    *e = (*e).max(*d);
                }
                victims.insert(id.clone());
            }
            let e = edges.entry(label.clone()).or_insert(newest_gone);
            *e = (*e).max(newest_gone);
        }
        for id in &victims {
            tx.execute("DELETE FROM msg_labels WHERE id=?", [id])
                .map_err(db)?;
            tx.execute("DELETE FROM bodies WHERE id=?", [id])
                .map_err(db)?;
            tx.execute("DELETE FROM msgs WHERE id=?", [id])
                .map_err(db)?;
        }
        rep.rows_removed = victims.len();
        for (label, newest_deleted) in edges {
            // Window now only guarantees rows strictly newer than the newest deleted one. The continuation
            // token no longer matches the edge, so it is dropped (callers must refill from the top).
            tx.execute(
                "UPDATE windows SET oldest_date=?2, complete=0, next_page_token=NULL
                 WHERE label=?1 AND oldest_date<=?3",
                params![label, newest_deleted + 1, newest_deleted],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        tracing::debug!(
            bodies = rep.bodies_removed,
            rows = rep.rows_removed,
            "cache evicted"
        );
        Ok(rep)
    }
}

fn labels_of(conn: &Connection, id: &str) -> Result<BTreeSet<String>, CacheError> {
    let mut st = conn
        .prepare_cached("SELECT label FROM msg_labels WHERE id=?")
        .map_err(db)?;
    st.query_map([id], |r| r.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<_, _>>()
        .map_err(db)
}

fn row_exists(conn: &Connection, id: &str) -> Result<bool, CacheError> {
    conn.prepare_cached("SELECT 1 FROM msgs WHERE id=?")
        .map_err(db)?
        .exists([id])
        .map_err(db)
}

fn window_of(conn: &Connection, label: &str) -> Result<Option<Window>, CacheError> {
    conn.prepare_cached(
        "SELECT oldest_date,complete,next_page_token,as_of FROM windows WHERE label=?",
    )
    .map_err(db)?
    .query_row([label], |r| {
        Ok(Window {
            label: LabelId::from(label),
            oldest_date: r.get(0)?,
            complete: r.get::<_, i64>(1)? != 0,
            next_page_token: r.get(2)?,
            as_of: r.get(3)?,
        })
    })
    .optional()
    .map_err(db)
}

/// Is there a window on any of `labels` (or on ALL)? Used to decide whether an uncached message matters.
fn any_window(conn: &Connection, labels: &BTreeSet<String>) -> Result<bool, CacheError> {
    for l in labels.iter().map(String::as_str).chain([ALL_MAIL]) {
        if window_of(conn, l)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// S4's admit rule: some label has a window that is complete or reaches back to `date`. `ALL` matches any labels.
fn in_window(conn: &Connection, labels: &[LabelId], date: i64) -> Result<bool, CacheError> {
    for l in labels.iter().map(LabelId::as_str).chain([ALL_MAIL]) {
        if let Some(w) = window_of(conn, l)?
            && (w.complete || date >= w.oldest_date)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl MailCache for AccountCache {
    fn cursor(&self) -> Result<Option<HistoryId>, CacheError> {
        let g = self.lock()?;
        let v: Option<String> = g
            .as_ref()
            .expect("checked")
            .query_row("SELECT v FROM meta WHERE k='history_id'", [], |r| r.get(0))
            .optional()
            .map_err(db)?;
        Ok(v.and_then(|v| v.parse::<u64>().ok()).map(HistoryId))
    }

    fn set_cursor(&self, cursor: HistoryId, synced_at_ms: i64) -> Result<(), CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let tx = conn.unchecked_transaction().map_err(db)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(k,v) VALUES('history_id',?)",
            [cursor.0.to_string()],
        )
        .map_err(db)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(k,v) VALUES('synced_at',?)",
            [synced_at_ms.to_string()],
        )
        .map_err(db)?;
        tx.commit().map_err(db)
    }

    fn upsert_meta(
        &self,
        metas: &[MessageMeta],
        now_ms: i64,
        mode: UpsertMode,
    ) -> Result<usize, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let tx = conn.unchecked_transaction().map_err(db)?;
        let mut stored = 0;
        for m in metas {
            if mode == UpsertMode::Admit
                && !row_exists(&tx, m.id.as_str())?
                && !in_window(&tx, &m.label_ids, m.internal_date_ms)?
            {
                continue;
            }
            let blob = self.enc(m.id.as_str(), &meta_json(m))?;
            tx.prepare_cached(
                "INSERT INTO msgs(id,thread_id,internal_date,size_estimate,mime_type,fetched_at,stale,meta)
                 VALUES(?,?,?,?,?,?,0,?)
                 ON CONFLICT(id) DO UPDATE SET thread_id=excluded.thread_id, internal_date=excluded.internal_date,
                   size_estimate=excluded.size_estimate, mime_type=excluded.mime_type,
                   fetched_at=excluded.fetched_at, stale=0, meta=excluded.meta",
            )
            .map_err(db)?
            .execute(params![
                m.id.as_str(),
                m.thread_id.as_str(),
                m.internal_date_ms,
                m.size_estimate as i64,
                m.mime_type,
                now_ms,
                blob
            ])
            .map_err(db)?;
            tx.prepare_cached("DELETE FROM msg_labels WHERE id=?")
                .map_err(db)?
                .execute([m.id.as_str()])
                .map_err(db)?;
            let mut ins = tx
                .prepare_cached("INSERT OR IGNORE INTO msg_labels(id,label) VALUES(?,?)")
                .map_err(db)?;
            for l in &m.label_ids {
                ins.execute([m.id.as_str(), l.as_str()]).map_err(db)?;
            }
            stored += 1;
        }
        tx.commit().map_err(db)?;
        Ok(stored)
    }

    fn rows(
        &self,
        label: &LabelId,
        before: Option<i64>,
        limit: usize,
        include_stale: bool,
    ) -> Result<Vec<MessageMeta>, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let before = before.unwrap_or(i64::MAX);
        let stale_ok = include_stale as i64;
        type Raw = (String, String, i64, i64, String, Vec<u8>);
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Raw> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        };
        let raw: Vec<Raw> = if label.as_str() == ALL_MAIL {
            let mut st = conn
                .prepare_cached(
                    "SELECT id,thread_id,internal_date,size_estimate,mime_type,meta FROM msgs
                     WHERE internal_date < ?1 AND (stale=0 OR ?2=1) ORDER BY internal_date DESC, id DESC LIMIT ?3",
                )
                .map_err(db)?;
            st.query_map(params![before, stale_ok, limit as i64], map)
                .map_err(db)?
                .collect::<Result<_, _>>()
                .map_err(db)?
        } else {
            let mut st = conn
                .prepare_cached(
                    "SELECT m.id,m.thread_id,m.internal_date,m.size_estimate,m.mime_type,m.meta
                     FROM msg_labels l JOIN msgs m ON m.id=l.id
                     WHERE l.label=?1 AND m.internal_date < ?2 AND (m.stale=0 OR ?3=1)
                     ORDER BY m.internal_date DESC, m.id DESC LIMIT ?4",
                )
                .map_err(db)?;
            st.query_map(params![label.as_str(), before, stale_ok, limit as i64], map)
                .map_err(db)?
                .collect::<Result<_, _>>()
                .map_err(db)?
        };
        raw.into_iter()
            .map(|(id, t, d, s, mt, b)| self.decode_meta(conn, id, t, d, s, mt, b))
            .collect()
    }

    fn has(&self, ids: &[MessageId]) -> Result<HashSet<MessageId>, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let mut out = HashSet::new();
        for id in ids {
            if row_exists(conn, id.as_str())? {
                out.insert(id.clone());
            }
        }
        Ok(out)
    }

    fn apply_events(&self, events: &[HistoryEvent]) -> Result<Vec<MessageId>, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let tx = conn.unchecked_transaction().map_err(db)?;
        let mut need: BTreeSet<String> = BTreeSet::new();
        let set_labels = |id: &str, labels: &BTreeSet<String>| -> Result<(), CacheError> {
            tx.prepare_cached("DELETE FROM msg_labels WHERE id=?")
                .map_err(db)?
                .execute([id])
                .map_err(db)?;
            let mut ins = tx
                .prepare_cached("INSERT INTO msg_labels(id,label) VALUES(?,?)")
                .map_err(db)?;
            for l in labels {
                ins.execute([id, l.as_str()]).map_err(db)?;
            }
            Ok(())
        };
        for e in events {
            match e {
                HistoryEvent::Added { id, label_ids, .. }
                | HistoryEvent::LabelsAdded { id, label_ids } => {
                    let labels: BTreeSet<String> =
                        label_ids.iter().map(|l| l.as_str().to_string()).collect();
                    if row_exists(&tx, id.as_str())? {
                        let mut cur = labels_of(&tx, id.as_str())?;
                        let before = cur.len();
                        cur.extend(labels);
                        if cur.len() != before {
                            set_labels(id.as_str(), &cur)?;
                        }
                    } else if any_window(&tx, &labels)? {
                        need.insert(id.as_str().to_string());
                    }
                }
                HistoryEvent::LabelsRemoved { id, label_ids } => {
                    if row_exists(&tx, id.as_str())? {
                        let mut cur = labels_of(&tx, id.as_str())?;
                        let before = cur.len();
                        for l in label_ids {
                            cur.remove(l.as_str());
                        }
                        if cur.len() != before {
                            set_labels(id.as_str(), &cur)?;
                        }
                    } else {
                        need.remove(id.as_str());
                    }
                }
                HistoryEvent::Deleted { id } => {
                    need.remove(id.as_str());
                    for t in ["msg_labels", "bodies", "msgs"] {
                        tx.execute(&format!("DELETE FROM {t} WHERE id=?"), [id.as_str()])
                            .map_err(db)?;
                    }
                }
            }
        }
        tx.commit().map_err(db)?;
        Ok(need.into_iter().map(MessageId::from).collect())
    }

    fn window(&self, label: &LabelId) -> Result<Option<Window>, CacheError> {
        let g = self.lock()?;
        window_of(g.as_ref().expect("checked"), label.as_str())
    }

    fn set_window(&self, w: &Window) -> Result<(), CacheError> {
        let g = self.lock()?;
        g.as_ref()
            .expect("checked")
            .execute(
                "INSERT OR REPLACE INTO windows(label,oldest_date,complete,next_page_token,as_of) VALUES(?,?,?,?,?)",
                params![w.label.as_str(), w.oldest_date, w.complete as i64, w.next_page_token, w.as_of],
            )
            .map_err(db)?;
        Ok(())
    }

    fn mark_all_stale_and_drop_windows(&self) -> Result<usize, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        let tx = conn.unchecked_transaction().map_err(db)?;
        let n = tx.execute("UPDATE msgs SET stale=1", []).map_err(db)?;
        tx.execute("DELETE FROM windows", []).map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(n)
    }

    fn body(&self, id: &MessageId) -> Result<Option<Vec<u8>>, CacheError> {
        let g = self.lock()?;
        let blob: Option<Vec<u8>> = g
            .as_ref()
            .expect("checked")
            .prepare_cached("SELECT raw FROM bodies WHERE id=?")
            .map_err(db)?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()
            .map_err(db)?;
        blob.map(|b| self.dec(id.as_str(), &b)).transpose()
    }

    fn put_body(&self, id: &MessageId, raw: &[u8], now_ms: i64) -> Result<bool, CacheError> {
        if raw.len() > MAX_CACHED_BODY {
            return Ok(false);
        }
        let blob = self.enc(id.as_str(), raw)?;
        let g = self.lock()?;
        // Keep an existing last_opened when a body is re-stored.
        g.as_ref()
            .expect("checked")
            .execute(
                "INSERT INTO bodies(id,raw,fetched_at,last_opened) VALUES(?1,?2,?3,NULL)
                 ON CONFLICT(id) DO UPDATE SET raw=excluded.raw, fetched_at=excluded.fetched_at",
                params![id.as_str(), blob, now_ms],
            )
            .map_err(db)?;
        Ok(true)
    }

    fn touch_opened(&self, id: &MessageId, now_ms: i64) -> Result<(), CacheError> {
        let g = self.lock()?;
        g.as_ref()
            .expect("checked")
            .execute(
                "UPDATE bodies SET last_opened=? WHERE id=?",
                params![now_ms, id.as_str()],
            )
            .map_err(db)?;
        Ok(())
    }

    fn label_ids(&self, id: &MessageId) -> Result<Option<Vec<LabelId>>, CacheError> {
        let g = self.lock()?;
        let conn = g.as_ref().expect("checked");
        if !row_exists(conn, id.as_str())? {
            return Ok(None);
        }
        Ok(Some(
            labels_of(conn, id.as_str())?
                .into_iter()
                .map(LabelId::from)
                .collect(),
        ))
    }

    fn evict(&self, now_ms: i64) -> Result<EvictReport, CacheError> {
        self.evict_with(now_ms, MAX_BODY_BYTES_PER_ACCOUNT, MAX_ROWS_PER_LABEL)
    }

    fn synced_at(&self) -> Result<Option<i64>, CacheError> {
        let g = self.lock()?;
        let v: Option<String> = g
            .as_ref()
            .expect("checked")
            .query_row("SELECT v FROM meta WHERE k='synced_at'", [], |r| r.get(0))
            .optional()
            .map_err(db)?;
        Ok(v.and_then(|v| v.parse().ok()))
    }
}
