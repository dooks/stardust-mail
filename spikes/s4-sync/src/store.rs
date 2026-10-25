//! SQLite cache. One DB per account. Optional SQLCipher key (feature `sqlcipher`) and an optional
//! per-row DPAPI codec for the "(b)" variant; ids, label ids and dates are always in clear.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Codec {
    Plain,
    /// meta and body blobs go through DPAPI `CryptProtectData`.
    Dpapi,
    /// ChaCha20-Poly1305 per blob (random nonce prepended, row id as AAD) under one random 256-bit
    /// master key that the caller keeps DPAPI-protected on disk.
    Aead([u8; 32]),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    pub id: String,
    pub thread_id: String,
    pub internal_date: i64,
    pub history_id: i64,
    pub size_estimate: i64,
    pub labels: BTreeSet<String>,
    /// Opaque (JSON of snippet + headers in the demo). Never printed.
    pub meta: Vec<u8>,
}

pub struct Store {
    conn: Connection,
    codec: Codec,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS msgs(
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    internal_date INTEGER NOT NULL,
    history_id INTEGER NOT NULL,
    size_estimate INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    meta BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS msgs_date ON msgs(internal_date DESC);
CREATE TABLE IF NOT EXISTS msg_labels(id TEXT NOT NULL, label TEXT NOT NULL, PRIMARY KEY(id, label)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS msg_labels_label ON msg_labels(label, id);
CREATE TABLE IF NOT EXISTS bodies(id TEXT PRIMARY KEY, raw BLOB NOT NULL, fetched_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS windows(label TEXT PRIMARY KEY, oldest_date INTEGER NOT NULL, complete INTEGER NOT NULL, as_of INTEGER NOT NULL);
";

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Store {
    pub fn open(path: &Path, key: Option<&[u8; 32]>, codec: Codec) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        if let Some(k) = key {
            // Raw 256-bit key: skips the PBKDF2 derivation (the key is random, not a password).
            conn.pragma_update(None, "key", format!("x'{}'", hex(k)))?;
        }
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn, codec })
    }

    fn enc(&self, id: &str, b: &[u8]) -> Vec<u8> {
        match self.codec {
            Codec::Plain => b.to_vec(),
            Codec::Dpapi => crate::dpapi::protect(b).expect("dpapi protect"),
            Codec::Aead(k) => {
                use chacha20poly1305::aead::{Aead, KeyInit, Payload};
                let mut nonce = [0u8; 12];
                getrandom::fill(&mut nonce).expect("rng");
                let c = chacha20poly1305::ChaCha20Poly1305::new((&k).into());
                let mut out = nonce.to_vec();
                out.extend(c.encrypt((&nonce).into(), Payload { msg: b, aad: id.as_bytes() }).expect("aead encrypt"));
                out
            }
        }
    }

    fn dec(&self, id: &str, b: Vec<u8>) -> Vec<u8> {
        match self.codec {
            Codec::Plain => b,
            Codec::Dpapi => crate::dpapi::unprotect(&b).expect("dpapi unprotect"),
            Codec::Aead(k) => {
                use chacha20poly1305::aead::{Aead, KeyInit, Payload};
                let c = chacha20poly1305::ChaCha20Poly1305::new((&k).into());
                let (nonce, ct) = b.split_at(12);
                c.decrypt(nonce.into(), Payload { msg: ct, aad: id.as_bytes() }).expect("aead decrypt (wrong key or tampered)")
            }
        }
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN")
    }

    pub fn commit(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("COMMIT")
    }

    pub fn upsert(&self, r: &Row, now: i64) -> rusqlite::Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO msgs(id,thread_id,internal_date,history_id,size_estimate,fetched_at,stale,meta) VALUES(?,?,?,?,?,?,0,?)
             ON CONFLICT(id) DO UPDATE SET thread_id=excluded.thread_id, internal_date=excluded.internal_date,
               history_id=excluded.history_id, size_estimate=excluded.size_estimate, fetched_at=excluded.fetched_at,
               stale=0, meta=excluded.meta",
        )?.execute(params![r.id, r.thread_id, r.internal_date, r.history_id, r.size_estimate, now, self.enc(&r.id, &r.meta)])?;
        self.set_labels(&r.id, &r.labels)
    }

    pub fn set_labels(&self, id: &str, labels: &BTreeSet<String>) -> rusqlite::Result<()> {
        self.conn.prepare_cached("DELETE FROM msg_labels WHERE id=?")?.execute([id])?;
        let mut ins = self.conn.prepare_cached("INSERT INTO msg_labels(id,label) VALUES(?,?)")?;
        for l in labels {
            ins.execute([id, l.as_str()])?;
        }
        Ok(())
    }

    pub fn labels(&self, id: &str) -> rusqlite::Result<Option<BTreeSet<String>>> {
        if !self.has(id)? {
            return Ok(None);
        }
        let mut st = self.conn.prepare_cached("SELECT label FROM msg_labels WHERE id=?")?;
        let v = st.query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Some(v))
    }

    pub fn has(&self, id: &str) -> rusqlite::Result<bool> {
        self.conn.prepare_cached("SELECT 1 FROM msgs WHERE id=?")?.exists([id])
    }

    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        self.conn.prepare_cached("DELETE FROM msg_labels WHERE id=?")?.execute([id])?;
        self.conn.prepare_cached("DELETE FROM bodies WHERE id=?")?.execute([id])?;
        Ok(self.conn.prepare_cached("DELETE FROM msgs WHERE id=?")?.execute([id])? > 0)
    }

    pub fn ids(&self) -> rusqlite::Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM msgs ORDER BY internal_date DESC")?;
        st.query_map([], |r| r.get(0))?.collect()
    }

    /// Newest `limit` rows carrying `label`, decoded.
    pub fn rows_for_label(&self, label: &str, limit: usize) -> rusqlite::Result<Vec<Row>> {
        let mut st = self.conn.prepare_cached(
            "SELECT m.id,m.thread_id,m.internal_date,m.history_id,m.size_estimate,m.meta FROM msg_labels l JOIN msgs m ON m.id=l.id
             WHERE l.label=? ORDER BY m.internal_date DESC LIMIT ?",
        )?;
        let raw: Vec<(String, String, i64, i64, i64, Vec<u8>)> = st
            .query_map(params![label, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(raw.len());
        for (id, thread_id, internal_date, history_id, size_estimate, meta) in raw {
            let labels = self.labels(&id)?.unwrap_or_default();
            let meta = self.dec(&id, meta);
            out.push(Row { id, thread_id, internal_date, history_id, size_estimate, labels, meta });
        }
        Ok(out)
    }

    pub fn put_body(&self, id: &str, raw: &[u8], now: i64) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO bodies(id,raw,fetched_at) VALUES(?,?,?)")?
            .execute(params![id, self.enc(id, raw), now])?;
        Ok(())
    }

    pub fn body(&self, id: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        let b: Option<Vec<u8>> = self.conn.prepare_cached("SELECT raw FROM bodies WHERE id=?")?.query_row([id], |r| r.get(0)).optional()?;
        Ok(b.map(|b| self.dec(id, b)))
    }

    pub fn body_ids(&self) -> rusqlite::Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM bodies")?;
        st.query_map([], |r| r.get(0))?.collect()
    }

    pub fn kv_get(&self, k: &str) -> rusqlite::Result<Option<String>> {
        self.conn.prepare_cached("SELECT v FROM meta WHERE k=?")?.query_row([k], |r| r.get(0)).optional()
    }

    pub fn kv_set(&self, k: &str, v: &str) -> rusqlite::Result<()> {
        self.conn.prepare_cached("INSERT OR REPLACE INTO meta(k,v) VALUES(?,?)")?.execute([k, v])?;
        Ok(())
    }

    /// Record that `label`'s cached list covers every message with `internal_date >= oldest_date`
    /// (`complete` = the listing reached the end, so it covers the whole label).
    pub fn set_window(&self, label: &str, oldest_date: i64, complete: bool, as_of: i64) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO windows(label,oldest_date,complete,as_of) VALUES(?,?,?,?)")?
            .execute(params![label, oldest_date, complete as i64, as_of])?;
        Ok(())
    }

    /// (oldest_date, complete, as_of)
    pub fn window(&self, label: &str) -> rusqlite::Result<Option<(i64, bool, i64)>> {
        self.conn
            .prepare_cached("SELECT oldest_date,complete,as_of FROM windows WHERE label=?")?
            .query_row([label], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?)))
            .optional()
    }

    pub fn drop_windows(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM windows")
    }

    pub fn mark_all_stale(&self) -> rusqlite::Result<usize> {
        self.conn.execute("UPDATE msgs SET stale=1", [])
    }

    pub fn count(&self, table: &str) -> rusqlite::Result<i64> {
        let t = match table {
            "msgs" | "bodies" | "msg_labels" => table,
            _ => "msgs",
        };
        self.conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
    }

    pub fn stale_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM msgs WHERE stale=1", [], |r| r.get(0))
    }

    /// Sum of stored body bytes (as stored, i.e. after the codec).
    pub fn body_bytes(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COALESCE(SUM(LENGTH(raw)),0) FROM bodies", [], |r| r.get(0))
    }

    pub fn any_window_label(&self, labels: &BTreeSet<String>) -> rusqlite::Result<bool> {
        for l in labels {
            if self.window(l)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fully close (checkpoint WAL, drop handles). After this the DB file has no -wal/-shm companions.
    pub fn close(self) -> rusqlite::Result<()> {
        let _ = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        self.conn.close().map_err(|(_, e)| e)
    }
}
