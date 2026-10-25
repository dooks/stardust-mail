//! Spike S4 CLI: freshness, sync and cache. Prints only ids, counts, sizes, label ids, timings, statuses.

mod api;
mod bench;
mod delta;
mod dpapi;
mod gate;
mod signout;
mod store;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use api::{Kind, Reply};
use base64::Engine;
use delta::Event;
use store::{Codec, Row, Store};

static START: OnceLock<Instant> = OnceLock::new();

fn since_start() -> f64 {
    START.get().map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out")
}

fn safe(email: &str) -> String {
    email.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

fn cache_root() -> PathBuf {
    out_dir().join("cache")
}

fn db_path(email: &str) -> PathBuf {
    cache_root().join(safe(email)).join("cache.db")
}

fn open_store(email: &str) -> Result<Store, String> {
    let p = db_path(email);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    Store::open(&p, None, Codec::Plain).map_err(|e| e.to_string())
}

fn parse_dur(s: &str) -> Duration {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("ms") {
        return Duration::from_millis(v.parse().unwrap_or(5000));
    }
    Duration::from_secs(s.trim_end_matches('s').parse().unwrap_or(5))
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn row_from(b: &serde_json::Value) -> Option<Row> {
    let id = b["id"].as_str()?.to_string();
    let labels = b["labelIds"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let meta = serde_json::json!({"snippet": b["snippet"], "headers": b["payload"]["headers"]});
    Some(Row {
        thread_id: b["threadId"].as_str().unwrap_or("").to_string(),
        internal_date: b["internalDate"].as_str()?.parse().ok()?,
        history_id: b["historyId"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
        size_estimate: b["sizeEstimate"].as_i64().unwrap_or(0),
        labels,
        meta: serde_json::to_vec(&meta).ok()?,
        id,
    })
}

async fn fetch_many(email: &str, ids: Vec<String>, raw: bool, conc: usize) -> Vec<(String, Result<Reply, String>)> {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(conc));
    let mut set = tokio::task::JoinSet::new();
    for id in ids {
        let (sem, email) = (sem.clone(), email.to_string());
        set.spawn(async move {
            let _p = sem.acquire().await.unwrap();
            let r = if raw { api::get_raw(&email, &id).await } else { api::get_metadata(&email, &id).await };
            (id, r.map_err(|e| e.to_string()))
        });
    }
    let mut out = Vec::new();
    while let Some(Ok(x)) = set.join_next().await {
        out.push(x);
    }
    out
}

#[derive(Default)]
struct Fill {
    profile_ms: u128,
    list_ms: u128,
    hydrate_ms: u128,
    rows: usize,
    hydrate_min: u128,
    hydrate_max: u128,
}

/// Cold fill: getProfile (cursor) first, then list INBOX, then hydrate metadata, then store.
async fn fill_inbox(store: &Store, email: &str, n: usize) -> Result<Fill, String> {
    let mut f = Fill::default();
    let p = api::profile(email).await.map_err(|e| e.to_string())?;
    f.profile_ms = p.ms;
    if p.kind != Kind::Ok {
        return Err(format!("profile status {}", p.status));
    }
    let cursor = p.body["historyId"].as_str().unwrap_or("").to_string();
    let l = api::list_inbox(email, n, None).await.map_err(|e| e.to_string())?;
    f.list_ms = l.ms;
    if l.kind != Kind::Ok {
        return Err(format!("list status {}", l.status));
    }
    let ids: Vec<String> = l.body["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    let complete = l.body["nextPageToken"].is_null();
    let t = Instant::now();
    let got = fetch_many(email, ids, false, 10).await;
    f.hydrate_ms = t.elapsed().as_millis();
    f.hydrate_min = u128::MAX;
    let now = now_ms();
    store.begin().map_err(|e| e.to_string())?;
    let mut oldest = i64::MAX;
    for (_, r) in got {
        let r = r?;
        f.hydrate_min = f.hydrate_min.min(r.ms);
        f.hydrate_max = f.hydrate_max.max(r.ms);
        if r.kind == Kind::Ok
            && let Some(row) = row_from(&r.body)
        {
            oldest = oldest.min(row.internal_date);
            store.upsert(&row, now).map_err(|e| e.to_string())?;
            f.rows += 1;
        }
    }
    store.set_window("INBOX", if f.rows == 0 { 0 } else { oldest }, complete, now).map_err(|e| e.to_string())?;
    store.kv_set("history_id", &cursor).map_err(|e| e.to_string())?;
    store.kv_set("synced_at", &now.to_string()).map_err(|e| e.to_string())?;
    store.commit().map_err(|e| e.to_string())?;
    Ok(f)
}

struct Sync {
    kind: Kind,
    status: u16,
    ms: u128,
    bytes: usize,
    events: Vec<Event>,
    applied: delta::Applied,
    fetched: Vec<(String, i64)>,
    admitted: usize,
}

/// One poll: history.list from the stored cursor (all pages), apply, hydrate new ids, advance the cursor.
async fn sync_once(store: &Store, email: &str) -> Result<Sync, String> {
    let t = Instant::now();
    let mut s = Sync { kind: Kind::Ok, status: 0, ms: 0, bytes: 0, events: vec![], applied: Default::default(), fetched: vec![], admitted: 0 };
    let Some(cursor) = store.kv_get("history_id").map_err(|e| e.to_string())? else {
        return Err("no cursor; run a fill first".into());
    };
    let mut page: Option<String> = None;
    let mut latest = cursor.clone();
    loop {
        let r = api::history(email, &cursor, page.as_deref()).await.map_err(|e| e.to_string())?;
        s.status = r.status;
        s.bytes += r.bytes;
        if r.kind != Kind::Ok {
            s.kind = r.kind;
            s.ms = t.elapsed().as_millis();
            return Ok(s);
        }
        s.events.extend(delta::parse(&r.body));
        if let Some(h) = r.body["historyId"].as_str() {
            latest = h.to_string();
        }
        page = r.body["nextPageToken"].as_str().map(String::from);
        if page.is_none() {
            break;
        }
    }
    s.applied = delta::apply(store, &s.events).map_err(|e| e.to_string())?;
    let ids: Vec<String> = s.applied.need_fetch.iter().cloned().collect();
    let now = now_ms();
    for (id, r) in fetch_many(email, ids, false, 10).await {
        let r = r?;
        s.bytes += r.bytes;
        if r.kind == Kind::Ok
            && let Some(row) = row_from(&r.body)
        {
            s.fetched.push((id, row.internal_date));
            if delta::admit(store, &row, now).map_err(|e| e.to_string())? {
                s.admitted += 1;
            }
        }
    }
    store.kv_set("history_id", &latest).map_err(|e| e.to_string())?;
    store.kv_set("synced_at", &now.to_string()).map_err(|e| e.to_string())?;
    s.ms = t.elapsed().as_millis();
    Ok(s)
}

/// Resync rule after 404 on history.list: keep rows and bodies, drop list windows, mark rows stale,
/// re-list the first page and take a fresh cursor.
async fn resync(store: &Store, email: &str, n: usize) -> Result<Fill, String> {
    store.drop_windows().map_err(|e| e.to_string())?;
    let stale = store.mark_all_stale().map_err(|e| e.to_string())?;
    let f = fill_inbox(store, email, n).await?;
    println!("  resync: {stale} cached rows marked stale, first page refilled ({} rows), {} still stale (revalidate on view)", f.rows, store.stale_count().unwrap_or(-1));
    Ok(f)
}

fn describe(s: &Sync) -> String {
    let mut c: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &s.events {
        *c.entry(e.name()).or_default() += 1;
    }
    format!(
        "{:?} http={} {}ms {}B events={:?} applied(deleted={} relabeled={} fetched={} admitted={} ignored={})",
        s.kind, s.status, s.ms, s.bytes, c, s.applied.deleted, s.applied.relabeled, s.fetched.len(), s.admitted, s.applied.ignored
    )
}

fn utc(ms: i64) -> String {
    gtoken::format_utc((ms / 1000).max(0) as u64)
}

// ---------------------------------------------------------------- commands

async fn cold_one(email: &str, empty: bool) -> Result<(), String> {
    if empty {
        let _ = std::fs::remove_dir_all(db_path(email).parent().unwrap());
    }
    let tag = email.split('@').next().unwrap_or("?").to_string();
    let store = open_store(email)?;
    let t_open = since_start();
    let warm = store.window("INBOX").map_err(|e| e.to_string())?.is_some();
    if warm {
        let rows = store.rows_for_label("INBOX", 20).map_err(|e| e.to_string())?;
        let t_rows = since_start();
        println!("[{tag}] WARM: db_open={t_open:.1}ms first {} rows readable from cache at t={t_rows:.1}ms (process start -> rows)", rows.len());
        let tt = Instant::now();
        let tok = gtoken::access_token(email).await.map_err(|e| e.to_string())?;
        drop(tok);
        let t_tok = tt.elapsed().as_millis();
        let s = sync_once(&store, email).await?;
        println!("[{tag}] WARM refresh behind rows: token={t_tok}ms poll+apply done at t={:.1}ms ({})", since_start(), describe(&s));
        if s.kind == Kind::Gone {
            resync(&store, email, 20).await?;
            println!("[{tag}] WARM after resync done at t={:.1}ms", since_start());
        }
    } else {
        let tt = Instant::now();
        let tok = gtoken::access_token(email).await.map_err(|e| e.to_string())?;
        drop(tok);
        let t_tok = tt.elapsed().as_millis();
        let f = fill_inbox(&store, email, 20).await?;
        println!(
            "[{tag}] EMPTY: db_open={t_open:.1}ms token_refresh={t_tok}ms getProfile={}ms list20={}ms hydrate20(10 parallel)={}ms (per-get min {}ms max {}ms) rows={} -> rows shown at t={:.1}ms",
            f.profile_ms, f.list_ms, f.hydrate_ms, f.hydrate_min, f.hydrate_max, f.rows, since_start()
        );
    }
    store.close().map_err(|e| e.to_string())
}

async fn cmd_cold(args: &[String]) -> Result<(), String> {
    let empty = has(args, "--empty");
    if has(args, "--all") {
        let accts = ["account-a@example.com", "account-b@example.com"];
        let (a, b) = tokio::join!(cold_one(accts[0], empty), cold_one(accts[1], empty));
        println!("ALL (parallel, 2 accounts): wall={:.1}ms from process start", since_start());
        a?;
        b
    } else {
        let email = args.first().ok_or("usage: cold <email>|--all [--empty]")?;
        cold_one(email, empty).await
    }
}

async fn cmd_watch(args: &[String]) -> Result<(), String> {
    let email = args.first().ok_or("usage: watch <email> [--every 5s[,10s,15s]] [--minutes 3]")?;
    let every: Vec<Duration> = flag(args, "--every").unwrap_or("5s").split(',').map(parse_dur).collect();
    let stage_min: f64 = flag(args, "--minutes").and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let p = api::profile(email).await.map_err(|e| e.to_string())?;
    let mut cursor = p.body["historyId"].as_str().ok_or_else(|| format!("no historyId (http {} {})", p.status, p.reason))?.to_string();
    println!("watch: start cursor taken; {} stage(s) of {stage_min} min; Ctrl-C stops", every.len());
    let mut polls: Vec<u128> = Vec::new();
    let mut lat: Vec<(u64, f64)> = Vec::new();
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let mut gate = gate::PollGate::default();
    let mut backoff = gate::Backoff::default();
    'stages: for (si, iv) in every.iter().enumerate() {
        println!("== stage {}/{}: interval {}s. A test mail may be sent NOW ==", si + 1, every.len(), iv.as_secs());
        let t_end = Instant::now() + Duration::from_secs_f64(stage_min * 60.0);
        let t0 = Instant::now();
        while Instant::now() < t_end {
            let _ = gate.request(gate::Trigger::Timer);
            let r = api::history(email, &cursor, None).await.map_err(|e| e.to_string())?;
            gate.finish();
            let seen = now_ms();
            let plan = backoff.next(&r.kind, *iv);
            let mut line = format!("[t+{:6.1}s] poll {:?} http={} {}ms {}B", t0.elapsed().as_secs_f64(), r.kind, r.status, r.ms, r.bytes);
            if r.kind == Kind::Ok {
                polls.push(r.ms);
                if let Some(h) = r.body["historyId"].as_str() {
                    cursor = h.to_string();
                }
                let evs = delta::parse(&r.body);
                if !evs.is_empty() {
                    line.push_str(&format!(" events={}", evs.len()));
                }
                println!("{line}");
                for e in &evs {
                    let mut extra = String::new();
                    if let Event::Added { id, .. } = e
                        && let Ok(m) = api::get_metadata(email, id).await
                        && let Some(row) = row_from(&m.body)
                    {
                        let l = (seen - row.internal_date) as f64 / 1000.0;
                        lat.push((iv.as_secs(), l));
                        extra = format!(" labels={:?} internalDate->seen={l:.1}s (interval {}s)", row.labels, iv.as_secs());
                    }
                    println!("   {} id={}{extra}", e.name(), e.id());
                }
            } else {
                println!("{line} reason={}", r.reason);
                if matches!(r.kind, Kind::Gone) {
                    return Err("history expired during watch".into());
                }
            }
            let Some(delay) = plan.delay else {
                println!("polling stopped for this account: ui={:?}", plan.ui);
                break 'stages;
            };
            if plan.ui != gate::Ui::Live {
                println!("   ui state: {:?}, next poll in {}s", plan.ui, delay.as_secs());
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = &mut ctrl_c => { println!("Ctrl-C"); break 'stages; }
            }
        }
    }
    polls.sort_unstable();
    if !polls.is_empty() {
        let q = |p: f64| polls[((polls.len() - 1) as f64 * p) as usize];
        println!("SUMMARY: {} ok polls, latency ms min={} p50={} p95={} max={}", polls.len(), polls[0], q(0.5), q(0.95), polls[polls.len() - 1]);
    }
    for (iv, l) in &lat {
        println!("SUMMARY: new mail seen {l:.1}s after internalDate at interval {iv}s");
    }
    Ok(())
}

async fn cmd_stale(args: &[String]) -> Result<(), String> {
    let (email, hid) = (args.first().ok_or("usage: stale <email> <historyId>")?, args.get(1).ok_or("need historyId")?);
    let r = api::history(email, hid, None).await.map_err(|e| e.to_string())?;
    println!("error.message={:?}", r.body["error"]["message"].as_str());
    println!("startHistoryId={hid}: http={} kind={:?} reason={:?} {}ms {}B events={} top.historyId={:?}", r.status, r.kind, r.reason, r.ms, r.bytes, delta::parse(&r.body).len(), r.body["historyId"].as_str());
    Ok(())
}

async fn cmd_probe_errors(args: &[String]) -> Result<(), String> {
    let email = args.first().ok_or("usage: probe-errors <email>")?;
    let r = api::get_with(api::BASE, "not-a-real-token", "/profile", &[]).await;
    println!("bad bearer token -> http={} kind={:?} reason={:?}", r.status, r.kind, r.reason);
    let r = api::get_with("http://127.0.0.1:9", "x", "/profile", &[]).await;
    println!("unreachable host (simulated no-network) -> http={} kind={:?} {}ms", r.status, r.kind, r.ms);
    let p = api::profile(email).await.map_err(|e| e.to_string())?;
    println!("current historyId is {} chars long", p.body["historyId"].as_str().map_or(0, str::len));
    for h in ["1", "99999999999999", "0", "abc"] {
        let r = api::history(email, h, None).await.map_err(|e| e.to_string())?;
        println!("startHistoryId={h} -> http={} kind={:?} reason={:?} {}ms", r.status, r.kind, r.reason, r.ms);
    }
    Ok(())
}

fn decode_raw(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).unwrap_or_default()
}

fn show_rows(store: &Store, banner: bool) -> Result<(), String> {
    let (oldest, complete, as_of) = store.window("INBOX").map_err(|e| e.to_string())?.ok_or("no INBOX window cached")?;
    let synced: i64 = store.kv_get("synced_at").map_err(|e| e.to_string())?.and_then(|s| s.parse().ok()).unwrap_or(0);
    if banner {
        println!("BANNER: offline - showing cached INBOX, as of {} UTC", utc(synced));
    }
    println!(
        "INBOX page: window oldest={} complete_to_end={complete} (list fetched {} UTC) -> {}",
        utc(oldest),
        utc(as_of),
        if complete { "COMPLETE: this is the whole label" } else { "PARTIAL: newer-than-edge only; older mail needs the network ('Load older' disabled offline)" }
    );
    for r in store.rows_for_label("INBOX", 50).map_err(|e| e.to_string())? {
        let body = store.body(&r.id).map_err(|e| e.to_string())?;
        println!("  id={} date={} labels={:?} size_est={} body_cached={}", r.id, r.internal_date, r.labels, r.size_estimate, body.map_or("no".to_string(), |b| format!("{}B", b.len())));
    }
    Ok(())
}

async fn cmd_cache_demo(args: &[String]) -> Result<(), String> {
    let email = args.first().ok_or("usage: cache-demo <email> [--offline] [--bodies N]")?;
    let store = open_store(email)?;
    if has(args, "--offline") {
        println!("OFFLINE: no network calls are made by this command");
        let t = Instant::now();
        show_rows(&store, true)?;
        println!("rows read in {:.2}ms", t.elapsed().as_secs_f64() * 1000.0);
        return store.close().map_err(|e| e.to_string());
    }
    let nb: usize = flag(args, "--bodies").and_then(|s| s.parse().ok()).unwrap_or(3);
    if store.kv_get("history_id").map_err(|e| e.to_string())?.is_none() {
        let f = fill_inbox(&store, email, 20).await?;
        println!("filled INBOX page: {} rows (profile {}ms, list {}ms, hydrate {}ms)", f.rows, f.profile_ms, f.list_ms, f.hydrate_ms);
    } else {
        let s = sync_once(&store, email).await?;
        println!("existing cache: delta applied: {}", describe(&s));
        if s.kind == Kind::Gone {
            resync(&store, email, 20).await?;
        }
    }
    let want: Vec<String> = store
        .rows_for_label("INBOX", 100)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| r.id)
        .filter(|id| store.body(id).ok().flatten().is_none())
        .take(nb)
        .collect();
    let t = Instant::now();
    let got = fetch_many(email, want, true, 4).await;
    let (mut raw_sizes, mut json_bytes) = (Vec::new(), 0usize);
    let now = now_ms();
    for (id, r) in got {
        let r = r?;
        if r.kind == Kind::Ok {
            json_bytes += r.bytes;
            let raw = decode_raw(r.body["raw"].as_str().unwrap_or(""));
            raw_sizes.push((raw.len(), r.body["sizeEstimate"].as_i64().unwrap_or(0)));
            store.put_body(&id, &raw, now).map_err(|e| e.to_string())?;
        }
    }
    println!("fetched {} raw bodies in {}ms: decoded sizes {:?} (raw, sizeEstimate); JSON-over-the-wire total {}B", raw_sizes.len(), t.elapsed().as_millis(), raw_sizes, json_bytes);
    let s = sync_once(&store, email).await?;
    println!("delta after fill: {}", describe(&s));
    show_rows(&store, false)?;
    let (m, b) = (store.count("msgs").unwrap_or(0), store.count("bodies").unwrap_or(0));
    let meta_bytes: i64 = store.rows_for_label("INBOX", 1000).map_err(|e| e.to_string())?.iter().map(|r| r.meta.len() as i64).sum();
    let dbp = db_path(email);
    let size = |s: &str| std::fs::metadata(format!("{}{s}", dbp.display())).map_or(0, |m| m.len());
    println!("cache stats: msgs={m} bodies={b} body_bytes={} meta_json_bytes_total={meta_bytes} (avg {}B/row) db_file={}B wal={}B", store.body_bytes().unwrap_or(0), meta_bytes / m.max(1), size(""), size("-wal"));
    store.close().map_err(|e| e.to_string())
}

async fn cmd_verify(args: &[String]) -> Result<(), String> {
    let email = args.first().ok_or("usage: verify-cache <email> [--no-apply]")?;
    let store = open_store(email)?;
    let ids = store.ids().map_err(|e| e.to_string())?;
    let mut pre = BTreeMap::new();
    for id in &ids {
        pre.insert(id.clone(), store.labels(id).map_err(|e| e.to_string())?.unwrap_or_default());
    }
    if !has(args, "--no-apply") {
        let s = sync_once(&store, email).await?;
        println!("delta applied: {}", describe(&s));
        if s.kind == Kind::Gone {
            resync(&store, email, 20).await?;
        }
    }
    let live = fetch_many(email, ids.clone(), false, 8).await;
    let (mut match_pre, mut match_post, mut mism_post, mut gone_cached, mut gone_cache_dropped, mut err) = (0, 0, 0, 0, 0, 0);
    for (id, r) in live {
        let r = r?;
        let post = store.labels(&id).map_err(|e| e.to_string())?;
        match r.kind {
            Kind::Ok => {
                let live_labels = row_from(&r.body).map(|r| r.labels).unwrap_or_default();
                if pre.get(&id) == Some(&live_labels) {
                    match_pre += 1;
                }
                if post.as_ref() == Some(&live_labels) {
                    match_post += 1;
                } else {
                    mism_post += 1;
                }
            }
            Kind::Gone => {
                if post.is_some() {
                    gone_cached += 1;
                } else {
                    gone_cache_dropped += 1;
                }
            }
            _ => err += 1,
        }
    }
    println!("label state for {} cached ids vs live: match_before_apply={match_pre} match_after_apply={match_post} mismatch_after_apply={mism_post} deleted_live_but_cached={gone_cached} deleted_live_and_dropped_from_cache={gone_cache_dropped} fetch_errors={err}", ids.len());
    // Window check: are the cached INBOX rows exactly the live newest-N INBOX ids?
    let cached_inbox: Vec<String> = store.rows_for_label("INBOX", 1000).map_err(|e| e.to_string())?.into_iter().map(|r| r.id).collect();
    let k = (cached_inbox.len() + 10).to_string();
    let l = api::get(email, "/messages", &[("labelIds", "INBOX"), ("maxResults", &k)]).await.map_err(|e| e.to_string())?;
    let live_ids: Vec<String> = l.body["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    let live_top: Vec<&String> = live_ids.iter().take(cached_inbox.len()).collect();
    let missing = live_top.iter().filter(|i| !cached_inbox.contains(i)).count();
    let extra = cached_inbox.iter().filter(|i| !live_ids.contains(i)).count();
    println!("INBOX window: cached_rows={} live_newest_same_count_missing_from_cache={missing} cached_not_in_live_inbox={extra}", cached_inbox.len());
    store.close().map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() {
    START.set(Instant::now()).ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("help", &[][..]),
    };
    let res = match cmd {
        "cold" => cmd_cold(rest).await,
        "watch" => cmd_watch(rest).await,
        "stale" => cmd_stale(rest).await,
        "probe-errors" => cmd_probe_errors(rest).await,
        "cache-demo" => cmd_cache_demo(rest).await,
        "verify-cache" => cmd_verify(rest).await,
        "bench-enc" => {
            let mode = rest.first().map_or("plain", String::as_str);
            bench::run(mode, &out_dir().join("bench")).map(|_| ())
        }
        "signout-test" => {
            let ok = signout::demo(&out_dir().join("signout"));
            println!("signout-test: {}", if ok { "PASS" } else { "FAIL" });
            Ok(())
        }
        "set-cursor" => {
            // Local-only: overwrite the cache cursor to simulate an expired history id (resync test).
            match (rest.first(), rest.get(1)) {
                (Some(e), Some(h)) => open_store(e).and_then(|s| s.kv_set("history_id", h).map_err(|e| e.to_string()).and_then(|_| s.close().map_err(|e| e.to_string()))),
                _ => Err("usage: set-cursor <email> <historyId>".into()),
            }
        }
        "accounts" => gtoken::accounts().map(|a| a.iter().for_each(|x| println!("{}", x.email))).map_err(|e| e.to_string()),
        _ => {
            println!("commands: cold <email>|--all [--empty] | watch <email> [--every 5s,10s,15s] [--minutes N] | stale <email> <historyId> | probe-errors <email> | cache-demo <email> [--offline] [--bodies N] | verify-cache <email> [--no-apply] | bench-enc plain|rowdpapi|sqlcipher | signout-test | set-cursor <email> <id> | accounts");
            Ok(())
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
