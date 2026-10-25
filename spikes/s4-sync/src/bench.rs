//! Q5: insert/query speed and plaintext-leak check for (a) SQLCipher, (b) per-row DPAPI, (c) plain.
//! Synthetic data only (a marker string stands in for mail content).

use std::path::Path;
use std::time::Instant;

use crate::store::{Codec, Row, Store};

const MARKER: &str = "SYNTHETIC-MARKER-7f3a";
const ROWS: usize = 1000;
const BODIES: usize = 200;
const BODY_BYTES: usize = 30_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn text(&mut self, n: usize) -> Vec<u8> {
        const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.,\n";
        (0..n).map(|_| A[(self.next() % A.len() as u64) as usize]).collect()
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn file_bytes(db: &Path) -> u64 {
    ["", "-wal", "-shm"].iter().map(|s| std::fs::metadata(format!("{}{s}", db.display())).map_or(0, |m| m.len())).sum()
}

fn leaks(db: &Path) -> bool {
    std::fs::read(db).map(|b| b.windows(MARKER.len()).any(|w| w == MARKER.as_bytes())).unwrap_or(false)
}

/// Returns false if the mode is not built into this binary.
pub fn run(mode: &str, dir: &Path) -> Result<bool, String> {
    let (mut codec, cipher) = match mode {
        "plain" => (Codec::Plain, false),
        "rowdpapi" => (Codec::Dpapi, false),
        "sqlcipher" => (Codec::Plain, true),
        "aead" => (Codec::Aead([0; 32]), false),
        _ => return Err("mode must be plain|rowdpapi|aead|sqlcipher".into()),
    };
    if cipher && !cfg!(feature = "sqlcipher") {
        println!("mode=sqlcipher: NOT BUILT in this binary (build with --no-default-features --features sqlcipher)");
        return Ok(false);
    }
    if !cipher && cfg!(feature = "sqlcipher") {
        println!("mode={mode}: skipped in the sqlcipher build (it links SQLCipher for every connection)");
        return Ok(false);
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let db = dir.join(format!("bench-{mode}.db"));
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", db.display()));
    }
    let key_file = dir.join(format!("bench-{mode}.key.dpapi"));
    let mut key = [0u8; 32];
    let keyed = cipher || mode == "aead";
    if keyed {
        getrandom::fill(&mut key).map_err(|e| e.to_string())?;
        std::fs::write(&key_file, crate::dpapi::protect(&key).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if mode == "aead" {
            codec = Codec::Aead(key);
        }
    }

    let mut rng = Rng(0x9E3779B97F4A7C15);
    let t = Instant::now();
    let st = Store::open(&db, cipher.then_some(&key), codec).map_err(|e| e.to_string())?;
    let t_open_new = ms(t);

    let t = Instant::now();
    st.begin().map_err(|e| e.to_string())?;
    for i in 0..ROWS {
        let mut meta = format!("{{\"m\":\"{MARKER}\",\"snippet\":\"").into_bytes();
        meta.extend(rng.text(600));
        meta.extend(b"\"}");
        let labels = if i % 7 == 0 { vec!["INBOX", "UNREAD"] } else { vec!["INBOX"] };
        let row = Row {
            id: format!("{:016x}", i as u64 * 7919),
            thread_id: format!("{:016x}", i as u64 * 7919),
            internal_date: 1_700_000_000_000 + i as i64 * 60_000,
            history_id: 1000 + i as i64,
            size_estimate: 30_000,
            labels: labels.into_iter().map(String::from).collect(),
            meta,
        };
        st.upsert(&row, 0).map_err(|e| e.to_string())?;
        if i < BODIES {
            let mut body = format!("{MARKER}\r\n").into_bytes();
            body.extend(rng.text(BODY_BYTES));
            st.put_body(&row.id, &body, 0).map_err(|e| e.to_string())?;
        }
    }
    st.commit().map_err(|e| e.to_string())?;
    let t_insert = ms(t);
    st.close().map_err(|e| e.to_string())?;

    // Reopen (includes DPAPI key unprotect for SQLCipher).
    let t = Instant::now();
    let k2 = if keyed {
        let enc = std::fs::read(&key_file).map_err(|e| e.to_string())?;
        let v = crate::dpapi::unprotect(&enc).map_err(|e| e.to_string())?;
        let mut k = [0u8; 32];
        k.copy_from_slice(&v);
        if mode == "aead" {
            codec = Codec::Aead(k);
        }
        Some(k)
    } else {
        None
    };
    let st = Store::open(&db, k2.as_ref().filter(|_| cipher), codec).map_err(|e| e.to_string())?;
    let t_reopen = ms(t);

    let reps = 100;
    let t = Instant::now();
    let mut n = 0;
    for _ in 0..reps {
        n += st.rows_for_label("INBOX", 50).map_err(|e| e.to_string())?.len();
    }
    let t_list50 = ms(t) / reps as f64;
    assert_eq!(n, 50 * reps);

    let t = Instant::now();
    let all = st.rows_for_label("INBOX", ROWS).map_err(|e| e.to_string())?;
    let t_scan = ms(t);
    assert_eq!(all.len(), ROWS);

    let ids = st.body_ids().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let mut bytes = 0;
    for id in ids.iter().cycle().take(reps) {
        bytes += st.body(id).map_err(|e| e.to_string())?.map_or(0, |b| b.len());
    }
    let t_body = ms(t) / reps as f64;
    assert!(bytes > 0);

    // Single-row autocommit insert (the poll path writes one message at a time).
    let t = Instant::now();
    for i in 0..50 {
        let row = Row {
            id: format!("zz{i:014x}"),
            thread_id: "t".into(),
            internal_date: 1_800_000_000_000 + i,
            history_id: 5000 + i,
            size_estimate: 1,
            labels: ["INBOX".to_string()].into(),
            meta: rng.text(700),
        };
        st.upsert(&row, 0).map_err(|e| e.to_string())?;
    }
    let t_single = ms(t) / 50.0;
    st.close().map_err(|e| e.to_string())?;

    println!(
        "mode={mode:9} rows={ROWS} bodies={BODIES}x{}KB | open_new={t_open_new:.1}ms insert_all={t_insert:.0}ms ({:.2}ms/row) single_insert={t_single:.2}ms reopen={t_reopen:.1}ms | list50={t_list50:.3}ms scan1000={t_scan:.1}ms body_get={t_body:.3}ms | file={}KB plaintext_marker_in_file={}",
        BODY_BYTES / 1000,
        t_insert / ROWS as f64,
        file_bytes(&db) / 1024,
        leaks(&db)
    );
    Ok(true)
}
