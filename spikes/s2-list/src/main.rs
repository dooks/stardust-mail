//! Spike S2 CLI: labels and message lists. GET only (plus the batch POST carrying GET sub-requests).
//! Output is content-free: ids, counts, sizes, system label ids, timings, status codes.

use s2lib::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const BATCH: &str = "https://gmail.googleapis.com/batch/gmail/v1";
const HDRS: [&str; 4] = ["From", "To", "Subject", "Date"];
const FIELDS_MSG: &str = "id,threadId,labelIds,snippet,internalDate,sizeEstimate,payload(mimeType,filename,headers)";
const FIELDS_THR: &str = "id,historyId,messages(id,threadId,labelIds,snippet,internalDate,sizeEstimate,payload(mimeType,filename,headers))";

// ---------------------------------------------------------------------------------------------
// API plumbing
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    units: AtomicU64,
    calls: AtomicU64,
    s429: AtomicU64,
    s403: AtomicU64,
    s5xx: AtomicU64,
    other: AtomicU64,
    bytes: AtomicU64,
    reasons: std::sync::Mutex<BTreeMap<String, u64>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Snap {
    units: u64,
    calls: u64,
    s429: u64,
    errs: u64,
    bytes: u64,
}

impl Stats {
    fn note(&self, status: u16, reason: &str) {
        *self.reasons.lock().expect("lock").entry(format!("{status} {reason}")).or_default() += 1;
    }

    fn snap(&self) -> Snap {
        Snap {
            units: self.units.load(Relaxed),
            calls: self.calls.load(Relaxed),
            s429: self.s429.load(Relaxed),
            errs: self.s403.load(Relaxed) + self.s5xx.load(Relaxed) + self.other.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
        }
    }
}

impl Snap {
    fn since(self, o: Snap) -> Snap {
        Snap { units: self.units - o.units, calls: self.calls - o.calls, s429: self.s429 - o.s429, errs: self.errs - o.errs, bytes: self.bytes - o.bytes }
    }
}

#[derive(Debug, Clone)]
struct ApiErr {
    status: u16,
    reason: String,
}

impl fmt::Display for ApiErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}", self.status, self.reason)
    }
}

type R<T> = Result<T, ApiErr>;

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    tok: Arc<String>,
    st: Arc<Stats>,
}

struct Resp {
    status: u16,
    body: Vec<u8>,
    dur: Duration,
}

fn reason_of(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    v["error"]["errors"][0]["reason"].as_str().or(v["error"]["status"].as_str()).unwrap_or("unknown").to_string()
}

impl Api {
    async fn new(email: &str) -> Api {
        let tok = gtoken::access_token(email).await.unwrap_or_else(|e| die(&format!("token: {e}")));
        Api { http: gtoken::http(), tok: Arc::new(tok), st: Arc::new(Stats::default()) }
    }

    fn with_http(&self, http: reqwest::Client) -> Api {
        Api { http, tok: self.tok.clone(), st: self.st.clone() }
    }

    fn account(&self, status: u16, len: usize) {
        self.st.calls.fetch_add(1, Relaxed);
        self.st.bytes.fetch_add(len as u64, Relaxed);
        match status {
            200 => {}
            429 => {
                self.st.s429.fetch_add(1, Relaxed);
            }
            403 => {
                self.st.s403.fetch_add(1, Relaxed);
            }
            500..=599 => {
                self.st.s5xx.fetch_add(1, Relaxed);
            }
            _ => {
                self.st.other.fetch_add(1, Relaxed);
            }
        }
    }

    async fn raw(&self, path: &str, q: &[(&str, &str)]) -> R<Resp> {
        let url = format!("{BASE}/{path}");
        let t = Instant::now();
        let r = self
            .http
            .get(&url)
            .bearer_auth(self.tok.as_str())
            .query(q)
            .send()
            .await
            .map_err(|e| ApiErr { status: 0, reason: format!("transport: {}", e.without_url()) })?;
        let status = r.status().as_u16();
        let body = r.bytes().await.map_err(|e| ApiErr { status: 0, reason: format!("body: {}", e.without_url()) })?.to_vec();
        self.st.units.fetch_add(unit_cost(path), Relaxed);
        self.account(status, body.len());
        Ok(Resp { status, body, dur: t.elapsed() })
    }

    async fn json(&self, path: &str, q: &[(&str, &str)]) -> R<Value> {
        let r = self.raw(path, q).await?;
        if r.status != 200 {
            self.st.note(r.status, &reason_of(&r.body));
            return Err(ApiErr { status: r.status, reason: reason_of(&r.body) });
        }
        serde_json::from_slice(&r.body).map_err(|_| ApiErr { status: 200, reason: "bad json".into() })
    }

    /// Like `json` but retries 429/5xx up to 4 times (used outside the benchmark).
    async fn jr(&self, path: &str, q: &[(&str, &str)]) -> R<Value> {
        let mut last = ApiErr { status: 0, reason: String::new() };
        for attempt in 0..6u64 {
            match self.json(path, q).await {
                Ok(v) => return Ok(v),
                Err(e) if e.status == 429 || e.status >= 500 || e.status == 0 || (e.status == 403 && e.reason.contains("ate")) => {
                    last = e;
                    tokio::time::sleep(Duration::from_millis(1500 * (attempt + 1))).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

async fn par<T, F, Fut>(items: Vec<String>, conc: usize, f: F) -> Vec<T>
where
    F: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let sem = Arc::new(Semaphore::new(conc.max(1)));
    let mut set = JoinSet::new();
    for (i, it) in items.into_iter().enumerate() {
        let sem = sem.clone();
        let f = f.clone();
        set.spawn(async move {
            let _p = sem.acquire_owned().await.expect("semaphore");
            (i, f(it).await)
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.push(r.expect("task"));
    }
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, t)| t).collect()
}

fn save(name: &str, v: &Value) {
    let dir = std::path::Path::new("out");
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(name), serde_json::to_vec(v).unwrap_or_default());
}

fn tag(email: &str) -> String {
    email.split('@').next().unwrap_or("acct").chars().take(6).collect()
}

// ---------------------------------------------------------------------------------------------
// Listing and hydrating
// ---------------------------------------------------------------------------------------------

struct Page {
    ids: Vec<(String, String)>,
    next: Option<String>,
    est: Option<u64>,
}

async fn list_page(api: &Api, params: &[(&str, &str)]) -> R<Page> {
    let v = api.jr("messages", params).await?;
    let ids = v["messages"]
        .as_array()
        .map(|a| a.iter().map(|m| (m["id"].as_str().unwrap_or("").to_string(), m["threadId"].as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    Ok(Page { ids, next: v["nextPageToken"].as_str().map(String::from), est: v["resultSizeEstimate"].as_u64() })
}

/// Enumerate up to `max_pages` pages of 500 ids. Returns (ids, pages, est of first page, complete).
async fn enumerate(api: &Api, base: &[(&str, &str)], max_pages: usize) -> R<(Vec<String>, usize, Option<u64>, bool)> {
    let mut ids = Vec::new();
    let mut tok: Option<String> = None;
    let mut est = None;
    let mut pages = 0;
    loop {
        let mut q: Vec<(&str, &str)> = base.to_vec();
        q.push(("maxResults", "500"));
        if let Some(t) = tok.as_deref() {
            q.push(("pageToken", t));
        }
        let p = list_page(api, &q).await?;
        if pages == 0 {
            est = p.est;
        }
        pages += 1;
        ids.extend(p.ids.into_iter().map(|(i, _)| i));
        tok = p.next;
        if tok.is_none() {
            return Ok((ids, pages, est, true));
        }
        if pages >= max_pages {
            return Ok((ids, pages, est, false));
        }
    }
}

fn meta_q(fields: Option<&'static str>) -> Vec<(&'static str, &'static str)> {
    let mut q = vec![("format", "metadata")];
    for h in HDRS {
        q.push(("metadataHeaders", h));
    }
    if let Some(f) = fields {
        q.push(("fields", f));
    }
    q
}

async fn hydrate_get(api: &Api, ids: &[String], conc: usize, fields: Option<&'static str>) -> Vec<R<Value>> {
    let api = api.clone();
    par(ids.to_vec(), conc, move |id| {
        let api = api.clone();
        async move { api.json(&format!("messages/{id}"), &meta_q(fields)).await }
    })
    .await
}

fn enc_q(fields: Option<&str>) -> String {
    let mut s = String::from("format=metadata");
    for h in HDRS {
        s.push_str(&format!("&metadataHeaders={h}"));
    }
    if let Some(f) = fields {
        s.push_str(&format!("&fields={f}"));
    }
    s
}

async fn hydrate_batch(api: &Api, ids: &[String], fields: Option<&str>) -> R<Vec<R<Value>>> {
    let boundary = "s2_batch_boundary_7f3a";
    let q = enc_q(fields);
    let gets: Vec<String> = ids.iter().map(|id| format!("/gmail/v1/users/me/messages/{id}?{q}")).collect();
    let body = build_batch_body(boundary, &gets);
    let r = api
        .http
        .post(BATCH)
        .bearer_auth(api.tok.as_str())
        .header("Content-Type", format!("multipart/mixed; boundary={boundary}"))
        .body(body)
        .send()
        .await
        .map_err(|e| ApiErr { status: 0, reason: format!("transport: {}", e.without_url()) })?;
    let status = r.status().as_u16();
    let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let text = r.text().await.map_err(|e| ApiErr { status: 0, reason: format!("body: {}", e.without_url()) })?;
    api.st.units.fetch_add(20 * ids.len() as u64, Relaxed);
    api.account(status, text.len());
    if status != 200 {
        return Err(ApiErr { status, reason: reason_of(text.as_bytes()) });
    }
    let rb = boundary_of(&ct).unwrap_or_default();
    let parts = parse_batch_response(&text, &rb);
    // Sub-request failures are counted as errors / 429 by sub-status.
    Ok(parts
        .into_iter()
        .map(|(s, v)| {
            if s != 200 {
                if s == 429 {
                    api.st.s429.fetch_add(1, Relaxed);
                } else {
                    api.st.other.fetch_add(1, Relaxed);
                }
                Err(ApiErr { status: s, reason: v.as_ref().map(|v| reason_of(v.to_string().as_bytes())).unwrap_or_default() })
            } else {
                v.ok_or(ApiErr { status: 200, reason: "unparsed part".into() })
            }
        })
        .collect())
}

async fn hydrate_get_r(api: &Api, ids: &[String], conc: usize, fields: Option<&'static str>) -> Vec<R<Value>> {
    let api = api.clone();
    par(ids.to_vec(), conc, move |id| {
        let api = api.clone();
        async move { api.jr(&format!("messages/{id}"), &meta_q(fields)).await }
    })
    .await
}

async fn hydrate_threads(api: &Api, ids: &[String], conc: usize, fields: Option<&'static str>) -> Vec<R<Value>> {
    let api = api.clone();
    par(ids.to_vec(), conc, move |id| {
        let api = api.clone();
        async move { api.jr(&format!("threads/{id}"), &meta_q(fields)).await }
    })
    .await
}

fn ok_count(v: &[R<Value>]) -> usize {
    v.iter().filter(|r| r.is_ok()).count()
}

fn is_system(id: &str) -> bool {
    matches!(id, "INBOX" | "SENT" | "DRAFT" | "SPAM" | "TRASH" | "STARRED" | "IMPORTANT" | "UNREAD" | "CHAT")
        || id.starts_with("CATEGORY_")
}

fn sys_flags(m: &Value) -> String {
    let mut f: Vec<&str> = m["labelIds"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).filter(|s| is_system(s)).collect()).unwrap_or_default();
    f.sort_unstable();
    f.join(",")
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let email = args.get(1).cloned().unwrap_or_default();
    if matches!(email.as_str(), "account-c@example.com") {
        die("this account is reserved for another spike");
    }
    let flag = |name: &str| -> Option<String> { args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned()) };
    match cmd {
        "labels" => cmd_labels(&email).await,
        "list" => {
            let label = args.get(2).cloned().unwrap_or_else(|| "INBOX".into());
            let n = flag("--n").and_then(|s| s.parse().ok()).unwrap_or(50);
            cmd_list(&email, &label, n, flag("--q"), args.iter().any(|a| a == "--spam")).await
        }
        "semantics" => cmd_semantics(&email).await,
        "bench" => {
            let runs = flag("--runs").and_then(|s| s.parse().ok()).unwrap_or(6);
            cmd_bench(&email, runs, args.iter().any(|a| a == "--burst")).await
        }
        "threads" => cmd_threads(&email).await,
        "rows" => cmd_rows(&email).await,
        "cold" => cmd_cold(&email, flag("--mode").as_deref().unwrap_or("messages")).await,
        _ => die("usage: s2 labels|list|semantics|bench|threads|rows|cold <email> [...]"),
    }
}

// ---- labels ---------------------------------------------------------------------------------

async fn cmd_labels(email: &str) {
    let api = Api::new(email).await;
    let s0 = api.st.snap();
    let t = Instant::now();
    let r = api.raw("labels", &[]).await.unwrap_or_else(|e| die(&e.to_string()));
    let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    println!("[{}] labels.list: status {} {:.0} ms, {} bytes, 1 unit", tag(email), r.status, secs(r.dur) * 1000.0, r.body.len());
    save(&format!("{}-labels.json", tag(email)), &v);
    let labels = v["labels"].as_array().cloned().unwrap_or_default();
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    for l in &labels {
        if let Some(o) = l.as_object() {
            for k in o.keys() {
                *keys.entry(k.clone()).or_default() += 1;
            }
        }
    }
    println!("labels: {} total; keys present in labels.list entries: {keys:?}", labels.len());
    let sys: Vec<&Value> = labels.iter().filter(|l| l["type"] == "system").collect();
    let usr: Vec<&Value> = labels.iter().filter(|l| l["type"] == "user").collect();
    println!("system: {}, user: {}", sys.len(), usr.len());
    let mut vis: BTreeMap<String, usize> = BTreeMap::new();
    for l in &labels {
        let k = format!("{} llv={} mlv={}", l["type"].as_str().unwrap_or("?"), l["labelListVisibility"].as_str().unwrap_or("-"), l["messageListVisibility"].as_str().unwrap_or("-"));
        *vis.entry(k).or_default() += 1;
    }
    println!("type/visibility combos: {vis:?}");
    let colored = labels.iter().filter(|l| l.get("color").is_some()).count();
    let colored_sys = sys.iter().filter(|l| l.get("color").is_some()).count();
    let bgs: HashSet<&str> = labels.iter().filter_map(|l| l["color"]["backgroundColor"].as_str()).collect();
    println!("labels with color: {colored} (system with color: {colored_sys}), distinct backgroundColor values: {}", bgs.len());

    // Tree shape (user labels only; names never printed).
    let pairs: Vec<(String, String)> = usr.iter().map(|l| (l["id"].as_str().unwrap_or("").to_string(), l["name"].as_str().unwrap_or("").to_string())).collect();
    let tree = build_tree(&pairs);
    let mut by_depth: BTreeMap<usize, usize> = BTreeMap::new();
    for n in &tree {
        *by_depth.entry(n.depth).or_default() += 1;
    }
    let synth = tree.iter().filter(|n| n.is_synthetic()).count();
    let with_slash = pairs.iter().filter(|(_, n)| n.contains('/')).count();
    println!("user labels containing '/': {with_slash}; tree nodes by depth: {by_depth:?}; synthetic parents (no own label): {synth}; max depth {}", tree.iter().map(|n| n.depth).max().unwrap_or(0));
    let sys_in_names = sys.iter().filter(|l| l["name"].as_str().is_some_and(|n| n.contains('/'))).count();
    println!("system labels with '/' in name: {sys_in_names}");
    let ids: Vec<String> = labels.iter().filter_map(|l| l["id"].as_str().map(String::from)).collect();
    let user_ids_pattern = usr.iter().filter(|l| l["id"].as_str().is_some_and(|i| i.starts_with("Label_"))).count();
    println!("user label ids of form Label_N: {user_ids_pattern}/{}", usr.len());

    // labels.get for everyone.
    let before = api.st.snap();
    let tg = Instant::now();
    let a2 = api.clone();
    let got = par(ids.clone(), 10, move |id| {
        let a = a2.clone();
        async move { (id.clone(), a.jr(&format!("labels/{id}"), &[]).await) }
    })
    .await;
    let d = api.st.snap().since(before);
    println!("labels.get x{} at concurrency 10: {:.0} ms total, {} units, {} calls, 429s {}, errors {}", ids.len(), secs(tg.elapsed()) * 1000.0, d.units, d.calls, d.s429, d.errs);
    // sequential latency sample
    let mut seq = Vec::new();
    for id in ids.iter().take(5) {
        let t = Instant::now();
        let _ = api.raw(&format!("labels/{id}"), &[]).await;
        seq.push(secs(t.elapsed()) * 1000.0);
    }
    println!("sequential labels.get latency sample (5): p50 {:.0} ms, max {:.0} ms", percentile(&seq, 50.0), percentile(&seq, 100.0));
    let mut gets: BTreeMap<String, Value> = BTreeMap::new();
    for (id, r) in got {
        if let Ok(v) = r {
            gets.insert(id, v);
        }
    }
    save(&format!("{}-labels-get.json", tag(email)), &serde_json::to_value(&gets).unwrap_or(Value::Null));
    let mut gkeys: BTreeMap<String, usize> = BTreeMap::new();
    for v in gets.values() {
        if let Some(o) = v.as_object() {
            for k in o.keys() {
                *gkeys.entry(k.clone()).or_default() += 1;
            }
        }
    }
    println!("keys present in labels.get entries: {gkeys:?}");
    println!("\nsystem labels (id type llv mlv | messagesTotal messagesUnread threadsTotal threadsUnread):");
    for l in &sys {
        let id = l["id"].as_str().unwrap_or("");
        let g = gets.get(id);
        let n = |k: &str| g.and_then(|g| g[k].as_u64()).map_or("-".to_string(), |x| x.to_string());
        println!(
            "  {id:<22} {} llv={} mlv={} | {} {} {} {}",
            l["type"].as_str().unwrap_or("?"),
            l["labelListVisibility"].as_str().unwrap_or("-"),
            l["messageListVisibility"].as_str().unwrap_or("-"),
            n("messagesTotal"),
            n("messagesUnread"),
            n("threadsTotal"),
            n("threadsUnread")
        );
    }
    println!("\nuser labels (index, depth, own-label, | messagesTotal messagesUnread threadsTotal):");
    let by_id: BTreeMap<&str, &str> = pairs.iter().map(|(i, n)| (i.as_str(), n.as_str())).collect();
    let mut k = 0;
    let mut sum_total = 0u64;
    let mut zero = 0;
    for n in tree.iter() {
        k += 1;
        if let Some(id) = &n.label_id {
            let g = gets.get(id.as_str());
            let num = |key: &str| g.and_then(|g| g[key].as_u64()).unwrap_or(0);
            sum_total += num("messagesTotal");
            if num("messagesTotal") == 0 {
                zero += 1;
            }
            println!("  user#{k} depth {} | {} {} {}", n.depth, num("messagesTotal"), num("messagesUnread"), num("threadsTotal"));
        } else {
            println!("  user#{k} depth {} synthetic (grouping only)", n.depth);
        }
    }
    let _ = by_id;
    println!("user labels with zero messages: {zero}; sum of per-label messagesTotal: {sum_total}");
    let p = api.jr("profile", &[]).await;
    if let Ok(p) = p {
        println!("getProfile: messagesTotal {} threadsTotal {}", p["messagesTotal"], p["threadsTotal"]);
    }
    let all = api.st.snap().since(s0);
    println!("\nTOTAL for this command: {} units, {} calls, wall {:.1} s, 429s {}, errors {}", all.units, all.calls, secs(t.elapsed()), all.s429, all.errs);
    println!("non-200 reasons: {:?}", api.st.reasons.lock().expect("lock"));
}

// ---- list -----------------------------------------------------------------------------------

async fn cmd_list(email: &str, label: &str, n: usize, q: Option<String>, spam: bool) {
    let api = Api::new(email).await;
    let max = n.to_string();
    let mut params: Vec<(&str, &str)> = vec![("maxResults", &max)];
    if label != "ALL" {
        params.push(("labelIds", label));
    }
    if let Some(q) = q.as_deref() {
        params.push(("q", q));
    }
    if spam {
        params.push(("includeSpamTrash", "true"));
    }
    let t = Instant::now();
    let page = list_page(&api, &params).await.unwrap_or_else(|e| die(&e.to_string()));
    println!("messages.list: {} ids, estimate {:?}, next page: {}, {:.0} ms", page.ids.len(), page.est, page.next.is_some(), secs(t.elapsed()) * 1000.0);
    let ids: Vec<String> = page.ids.iter().map(|(i, _)| i.clone()).collect();
    let t = Instant::now();
    let rows = hydrate_get_r(&api, &ids, 10, Some(FIELDS_MSG)).await;
    println!("hydrate c10: {} ok / {} in {:.0} ms", ok_count(&rows), rows.len(), secs(t.elapsed()) * 1000.0);
    let mut prev = i64::MAX;
    let mut inversions = 0;
    for (i, r) in rows.iter().enumerate() {
        if let Ok(m) = r {
            let d = internal_date(m).unwrap_or(0);
            if d > prev {
                inversions += 1;
            }
            prev = d;
            println!(
                "{i:>3} {} thr={} {} size={} [{}] mime={}",
                m["id"].as_str().unwrap_or(""),
                m["threadId"].as_str().unwrap_or(""),
                gtoken::format_utc((d / 1000) as u64),
                m["sizeEstimate"],
                sys_flags(m),
                m["payload"]["mimeType"].as_str().unwrap_or("?")
            );
        }
    }
    println!("order inversions (newer after older): {inversions}");
}

// ---- semantics (Q2) -------------------------------------------------------------------------

async fn dates_of(api: &Api, ids: &[String]) -> Vec<Option<(i64, Vec<String>)>> {
    hydrate_get_r(api, ids, 10, Some("id,internalDate,labelIds")).await
        .into_iter()
        .map(|r| {
            r.ok().and_then(|m| {
                let d = internal_date(&m)?;
                let l = m["labelIds"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                Some((d, l))
            })
        })
        .collect()
}

async fn order_check(api: &Api, name: &str, base: &[(&str, &str)], pages: usize) {
    let mut ids = Vec::new();
    let mut tok: Option<String> = None;
    for _ in 0..pages {
        let mut q: Vec<(&str, &str)> = base.to_vec();
        q.push(("maxResults", "100"));
        if let Some(t) = tok.as_deref() {
            q.push(("pageToken", t));
        }
        let Ok(p) = list_page(api, &q).await else { break };
        ids.extend(p.ids.into_iter().map(|(i, _)| i));
        tok = p.next;
        if tok.is_none() {
            break;
        }
    }
    let ds = dates_of(api, &ids).await;
    let ds: Vec<i64> = ds.into_iter().flatten().map(|x| x.0).collect();
    let inv = ds.windows(2).filter(|w| w[1] > w[0]).count();
    {
        let mut det = Vec::new();
        for (i, w) in ds.windows(2).enumerate() {
            if w[1] > w[0] {
                det.push(format!("pos{}:+{}s", i + 1, (w[1] - w[0]) / 1000));
            }
        }
        det.truncate(12);
        println!("    inversion detail (position: how much newer than predecessor): {det:?}");
    }
    let ties = ds.windows(2).filter(|w| w[1] == w[0]).count();
    let boundary_inv = (1..ds.len() / 100).filter(|k| ds.get(k * 100).zip(ds.get(k * 100 - 1)).is_some_and(|(a, b)| a > b)).count();
    println!("  order[{name}]: {} msgs over {} pages: inversions {inv}, equal-ms ties {ties}, page-boundary inversions {boundary_inv}", ds.len(), ids.len().div_ceil(100));
}

async fn cmd_semantics(email: &str) {
    let api = Api::new(email).await;
    let s0 = api.st.snap();
    println!("== semantics for {} ==", tag(email));
    // labels
    let lv = api.jr("labels", &[]).await.unwrap_or_else(|e| die(&e.to_string()));
    let labels = lv["labels"].as_array().cloned().unwrap_or_default();
    let prof = api.jr("profile", &[]).await.unwrap_or(Value::Null);
    println!("getProfile messagesTotal {} threadsTotal {}", prof["messagesTotal"], prof["threadsTotal"]);

    // A. page sizes
    for mr in [None, Some("1"), Some("500"), Some("501"), Some("1000")] {
        let mut q = vec![("labelIds", "INBOX")];
        if let Some(m) = mr {
            q.push(("maxResults", m));
        }
        match list_page(&api, &q).await {
            Ok(p) => println!("A maxResults={:?}: {} ids, next={}, est={:?}", mr, p.ids.len(), p.next.is_some(), p.est),
            Err(e) => println!("A maxResults={:?}: {e}", mr),
        }
    }
    let bad = api.raw("messages", &[("labelIds", "INBOX"), ("pageToken", "garbage")]).await.map(|r| (r.status, reason_of(&r.body)));
    println!("A invalid pageToken: {:?}", bad.map_err(|e| e.to_string()));
    let bad = api.raw("messages", &[("labelIds", "NO_SUCH_LABEL_XYZ")]).await.map(|r| (r.status, reason_of(&r.body)));
    println!("A unknown labelId: {:?}", bad.map_err(|e| e.to_string()));

    // B/C. Per label: estimate vs get vs enumeration.
    println!("\nB/C per label: id | labels.get messagesTotal | est(maxResults=1) | est(maxResults=500) | enumerated | pages | complete");
    let mut targets: Vec<(String, String)> = Vec::new();
    let mut ucount = 0;
    for l in &labels {
        let id = l["id"].as_str().unwrap_or("").to_string();
        let shown = if l["type"] == "system" {
            id.clone()
        } else {
            ucount += 1;
            format!("user#{ucount}")
        };
        targets.push((id, shown));
    }
    for (id, shown) in &targets {
        let g = api.jr(&format!("labels/{id}"), &[]).await.unwrap_or(Value::Null);
        let total = g["messagesTotal"].as_u64();
        let spam = id == "SPAM" || id == "TRASH";
        let est1 = list_page(&api, &[("labelIds", id), ("maxResults", "1")]).await.ok().and_then(|p| p.est);
        let est1_incl = if spam { list_page(&api, &[("labelIds", id), ("maxResults", "1"), ("includeSpamTrash", "true")]).await.ok().and_then(|p| p.est) } else { None };
        let en = enumerate(&api, &[("labelIds", id)], 8).await;
        let en_incl = if spam { enumerate(&api, &[("labelIds", id), ("includeSpamTrash", "true")], 8).await.ok() } else { None };
        match en {
            Ok((ids, pages, est500, complete)) => {
                println!("  {shown:<22} | {:?} | {:?} | {:?} | {} | {} | {}{}", total, est1, est500, ids.len(), pages, complete,
                    if spam { format!("   [includeSpamTrash=true: est1 {:?}, enumerated {:?}]", est1_incl, en_incl.map(|x| x.0.len())) } else { String::new() });
            }
            Err(e) => println!("  {shown:<22} | {:?} | {:?} | error {e}", total, est1),
        }
    }

    // D. All mail
    println!("\nD all mail (no labelIds):");
    for incl in [false, true] {
        let base: Vec<(&str, &str)> = if incl { vec![("includeSpamTrash", "true")] } else { vec![] };
        let est1 = list_page(&api, &[base.clone(), vec![("maxResults", "1")]].concat()).await.ok().and_then(|p| p.est);
        match enumerate(&api, &base, 60).await {
            Ok((ids, pages, _, c)) => println!("  includeSpamTrash={incl}: est1 {:?}, enumerated {} in {} pages (complete {c}), profile.messagesTotal {}", est1, ids.len(), pages, prof["messagesTotal"]),
            Err(e) => println!("  includeSpamTrash={incl}: {e}"),
        }
    }

    // E. q and AND semantics
    println!("\nE queries:");
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("INBOX + q=newer_than:1d", vec![("labelIds", "INBOX"), ("q", "newer_than:1d")]),
        ("INBOX + q=category:primary", vec![("labelIds", "INBOX"), ("q", "category:primary")]),
        ("q=category:primary (no labelIds)", vec![("q", "category:primary")]),
        ("q=in:inbox newer_than:7d", vec![("q", "in:inbox newer_than:7d")]),
        ("labelIds=INBOX&labelIds=UNREAD", vec![("labelIds", "INBOX"), ("labelIds", "UNREAD")]),
        ("q=is:unread in:inbox", vec![("q", "is:unread in:inbox")]),
        ("q=category:personal", vec![("q", "category:personal")]),
        ("q=category:updates", vec![("q", "category:updates")]),
        ("q=in:inbox category:primary", vec![("q", "in:inbox category:primary")]),
        ("labelIds=CATEGORY_PROMOTIONS", vec![("labelIds", "CATEGORY_PROMOTIONS")]),
    ];
    for (name, base) in &cases {
        let est1 = list_page(&api, &[base.clone(), vec![("maxResults", "1")]].concat()).await.ok().and_then(|p| p.est);
        match enumerate(&api, base, 6).await {
            Ok((ids, pages, _, c)) => {
                let sample: Vec<String> = ids.iter().take(30).cloned().collect();
                let d = dates_of(&api, &sample).await;
                let in_1d = d.iter().flatten().filter(|(t, _)| now_ms - *t <= 86_400_000 + 3_600_000).count();
                let prim = d.iter().flatten().filter(|(_, l)| l.iter().any(|x| x == "CATEGORY_PERSONAL")).count();
                let both = d.iter().flatten().filter(|(_, l)| l.iter().any(|x| x == "INBOX") && l.iter().any(|x| x == "UNREAD")).count();
                let inbox = d.iter().flatten().filter(|(_, l)| l.iter().any(|x| x == "INBOX")).count();
                println!("  {name}: est1 {:?}, enumerated {} ({} pages, complete {c}); of first {} sampled: within ~25h {in_1d}, CATEGORY_PERSONAL {prim}, INBOX {inbox}, INBOX+UNREAD {both}", est1, ids.len(), pages, d.len());
            }
            Err(e) => println!("  {name}: {e}"),
        }
    }

    // F. ordering
    println!("\nF ordering (internalDate, 3 pages of 100 each):");
    order_check(&api, "INBOX", &[("labelIds", "INBOX")], 3).await;
    order_check(&api, "SENT", &[("labelIds", "SENT")], 3).await;
    order_check(&api, "all mail", &[], 3).await;
    order_check(&api, "q newer_than:30d", &[("q", "newer_than:30d")], 3).await;
    order_check(&api, "TRASH+includeSpamTrash", &[("labelIds", "TRASH"), ("includeSpamTrash", "true")], 3).await;
    order_check(&api, "CATEGORY_PROMOTIONS", &[("labelIds", "CATEGORY_PROMOTIONS")], 3).await;
    // pagination integrity
    if let Ok((ids, pages, _, c)) = enumerate(&api, &[("labelIds", "INBOX")], 80).await {
        let uniq: HashSet<&String> = ids.iter().collect();
        println!("  pagination INBOX: {} ids over {} pages (complete {c}), duplicates {}", ids.len(), pages, ids.len() - uniq.len());
    }
    let all = api.st.snap().since(s0);
    println!("\nTOTAL: {} units, {} calls, 429s {}, errors {}", all.units, all.calls, all.s429, all.errs);
    println!("non-200 reasons: {:?}", api.st.reasons.lock().expect("lock"));
}

// ---- bench (Q3) -----------------------------------------------------------------------------

struct MethodStat {
    name: &'static str,
    times: Vec<f64>,
    list_times: Vec<f64>,
    units: u64,
    s429: u64,
    errs: u64,
    bytes: u64,
    runs: u64,
    ok_msgs: usize,
    want_msgs: usize,
}

async fn cmd_bench(email: &str, runs: usize, burst: bool) {
    let sleep_ms: u64 = std::env::var("S2_SLEEP_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let api = Api::new(email).await;
    let h1 = api.with_http(reqwest::Client::builder().http1_only().timeout(Duration::from_secs(30)).build().expect("h1 client"));
    println!("== bench for {} ({runs} runs, page size 50, INBOX) ==", tag(email));
    // Pre-collect page tokens for messages and threads.
    let mut mtoks: Vec<Option<String>> = vec![None];
    let mut ttoks: Vec<Option<String>> = vec![None];
    for _ in 1..runs {
        let m = mtoks.last().cloned().flatten();
        let mut q = vec![("labelIds", "INBOX"), ("maxResults", "50")];
        if let Some(t) = m.as_deref() {
            q.push(("pageToken", t));
        }
        mtoks.push(list_page(&api, &q).await.ok().and_then(|p| p.next));
        let tt = ttoks.last().cloned().flatten();
        let mut q = vec![("labelIds", "INBOX"), ("maxResults", "50")];
        if let Some(t) = tt.as_deref() {
            q.push(("pageToken", t));
        }
        let nt = api.jr("threads", &q).await.ok().and_then(|v| v["nextPageToken"].as_str().map(String::from));
        ttoks.push(nt);
    }
    let names: [&'static str; 9] = ["msg c5", "msg c10", "msg c20", "msg c20 +fields", "msg c10 (HTTP/1.1)", "batch", "batch +fields", "threads c10", "threads c10 +fields"];
    let mut stats: Vec<MethodStat> = names
        .iter()
        .map(|n| MethodStat { name: n, times: vec![], list_times: vec![], units: 0, s429: 0, errs: 0, bytes: 0, runs: 0, ok_msgs: 0, want_msgs: 0 })
        .collect();
    for r in 0..runs {
        // messages.list for this run's page (timed, shared by all message-based methods)
        let mut q = vec![("labelIds", "INBOX"), ("maxResults", "50")];
        if let Some(t) = mtoks[r].as_deref() {
            q.push(("pageToken", t));
        }
        let tl = Instant::now();
        let page = list_page(&api, &q).await;
        let list_ms = secs(tl.elapsed()) * 1000.0;
        let Ok(page) = page else {
            println!("run {r}: list failed");
            continue;
        };
        let ids: Vec<String> = page.ids.iter().map(|(i, _)| i.clone()).collect();
        // rotate method order per run
        for k in 0..names.len() {
            let mi = (k + r) % names.len();
            let before = api.st.snap();
            let t = Instant::now();
            let (ok, want): (usize, usize);
            let mut list_extra = None;
            match mi {
                0 => {
                    let v = hydrate_get(&api, &ids, 5, None).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
                1 => {
                    let v = hydrate_get(&api, &ids, 10, None).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
                2 => {
                    let v = hydrate_get(&api, &ids, 20, None).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
                3 => {
                    let v = hydrate_get(&api, &ids, 20, Some(FIELDS_MSG)).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
                4 => {
                    let v = hydrate_get(&h1, &ids, 10, None).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
                5 | 6 => {
                    let f = if mi == 6 { Some(FIELDS_MSG) } else { None };
                    match hydrate_batch(&api, &ids, f).await {
                        Ok(v) => (ok, want) = (ok_count(&v), ids.len()),
                        Err(e) => {
                            println!("  batch error: {e}");
                            (ok, want) = (0, ids.len());
                        }
                    }
                }
                _ => {
                    let mut q = vec![("labelIds", "INBOX"), ("maxResults", "50")];
                    if let Some(t) = ttoks[r].as_deref() {
                        q.push(("pageToken", t));
                    }
                    let tl = Instant::now();
                    let tv = api.jr("threads", &q).await;
                    list_extra = Some(secs(tl.elapsed()) * 1000.0);
                    let tids: Vec<String> = tv.map(|v| v["threads"].as_array().map(|a| a.iter().filter_map(|t| t["id"].as_str().map(String::from)).collect()).unwrap_or_default()).unwrap_or_default();
                    let f = if mi == 8 { Some(FIELDS_THR) } else { None };
                    let v = hydrate_threads(&api, &tids, 10, f).await;
                    (ok, want) = (ok_count(&v), v.len());
                }
            }
            let ms = secs(t.elapsed()) * 1000.0;
            let d = api.st.snap().since(before);
            let s = &mut stats[mi];
            s.times.push(ms);
            s.list_times.push(list_extra.unwrap_or(list_ms));
            s.units += d.units;
            s.s429 += d.s429;
            s.errs += d.errs;
            s.bytes += d.bytes;
            s.runs += 1;
            s.ok_msgs += ok;
            s.want_msgs += want;
            tokio::time::sleep(Duration::from_millis(sleep_ms.max(d.units * 10 + 1000))).await;
        }
        println!("run {r} done (list {list_ms:.0} ms)");
    }
    println!("\nmethod                | n | hydrate p50 ms | p95 ms | min | first run | list call p50 ms | units/run (hydrate only) | KB/run | ok/want | 429 | other errs");
    for s in &stats {
        let n = s.runs.max(1);
        println!(
            "{:<22}| {} | {:>7.0} | {:>7.0} | {:>5.0} | {:>6.0} | {:>6.0} | {:>5} | {:>6.0} | {}/{} | {} | {}",
            s.name,
            s.times.len(),
            percentile(&s.times, 50.0),
            percentile(&s.times, 95.0),
            s.times.iter().cloned().fold(f64::MAX, f64::min),
            s.times.first().copied().unwrap_or(0.0),
            percentile(&s.list_times, 50.0),
            s.units / n,
            s.bytes as f64 / n as f64 / 1024.0,
            s.ok_msgs,
            s.want_msgs,
            s.s429,
            s.errs
        );
    }
    if burst {
        println!("\nburst test: 100 messages.get, concurrency 100 (500 units in well under a second)");
        let mut ids = Vec::new();
        if let Ok(p) = list_page(&api, &[("labelIds", "INBOX"), ("maxResults", "100")]).await {
            ids = p.ids.into_iter().map(|(i, _)| i).collect();
        }
        let before = api.st.snap();
        let t = Instant::now();
        let v = hydrate_get(&api, &ids, 100, Some("id,internalDate")).await;
        let d = api.st.snap().since(before);
        let mut codes: BTreeMap<u16, usize> = BTreeMap::new();
        for r in &v {
            *codes.entry(r.as_ref().err().map_or(200, |e| e.status)).or_default() += 1;
        }
        println!("  {} calls in {:.0} ms, status counts {codes:?}, 429s {}", v.len(), secs(t.elapsed()) * 1000.0, d.s429);
        let reasons: HashSet<String> = v.iter().filter_map(|r| r.as_ref().err().map(|e| e.reason.clone())).collect();
        println!("  error reasons: {reasons:?}");
    }
}

// ---- threads (Q4) ---------------------------------------------------------------------------

async fn cmd_threads(email: &str) {
    let api = Api::new(email).await;
    let s0 = api.st.snap();
    println!("== threads for {} ==", tag(email));
    let tv = api.jr("threads", &[("labelIds", "INBOX"), ("maxResults", "200")]).await.unwrap_or_else(|e| die(&e.to_string()));
    let threads = tv["threads"].as_array().cloned().unwrap_or_default();
    println!("threads.list INBOX maxResults=200: {} threads, estimate {}, keys of a thread entry: {:?}", threads.len(), tv["resultSizeEstimate"], threads.first().and_then(|t| t.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()));
    let ids: Vec<String> = threads.iter().filter_map(|t| t["id"].as_str().map(String::from)).collect();
    let t = Instant::now();
    let got = hydrate_threads(&api, &ids, 10, Some(FIELDS_THR)).await;
    println!("threads.get x{} c10 (fields-trimmed): {} ok, {:.0} ms", ids.len(), ok_count(&got), secs(t.elapsed()) * 1000.0);
    let mut dist: BTreeMap<&str, usize> = BTreeMap::new();
    let mut multi = 0;
    let mut n = 0;
    let mut newest_in_inbox = 0;
    let mut multi_newest_in_inbox = 0;
    let mut snippet_eq_last = 0;
    let mut snippet_eq_inbox_last = 0;
    let mut order_by_newest_all = 0;
    let mut maxmsgs = 0;
    let mut prev_newest = i64::MAX;
    let mut multi_unread_newest = 0;
    let mut multi_with_non_inbox = 0;
    for (tl, r) in threads.iter().zip(got.iter()) {
        let Ok(th) = r else { continue };
        let msgs = th["messages"].as_array().cloned().unwrap_or_default();
        if msgs.is_empty() {
            continue;
        }
        n += 1;
        maxmsgs = maxmsgs.max(msgs.len());
        let bucket = match msgs.len() {
            1 => "1",
            2 => "2",
            3..=5 => "3-5",
            _ => "6+",
        };
        *dist.entry(bucket).or_default() += 1;
        let newest = msgs.iter().max_by_key(|m| internal_date(m).unwrap_or(0)).expect("non-empty");
        let has_label = |m: &Value, l: &str| m["labelIds"].as_array().is_some_and(|a| a.iter().any(|x| x == l));
        let nd = internal_date(newest).unwrap_or(0);
        if nd <= prev_newest {
            order_by_newest_all += 1;
        }
        prev_newest = nd;
        let newest_inbox = has_label(newest, "INBOX");
        if newest_inbox {
            newest_in_inbox += 1;
        }
        if tl["snippet"] == newest["snippet"] {
            snippet_eq_last += 1;
        }
        let newest_inbox_msg = msgs.iter().filter(|m| has_label(m, "INBOX")).max_by_key(|m| internal_date(m).unwrap_or(0));
        if newest_inbox_msg.is_some_and(|m| tl["snippet"] == m["snippet"]) {
            snippet_eq_inbox_last += 1;
        }
        if msgs.len() > 1 {
            multi += 1;
            if newest_inbox {
                multi_newest_in_inbox += 1;
            }
            if has_label(newest, "UNREAD") {
                multi_unread_newest += 1;
            }
            if msgs.iter().any(|m| !has_label(m, "INBOX")) {
                multi_with_non_inbox += 1;
            }
        }
    }
    println!("threads hydrated: {n}; message-count distribution {dist:?}; max messages in a thread {maxmsgs}");
    println!("threads with >1 message: {multi}/{n} = {:.1}%", 100.0 * multi as f64 / n.max(1) as f64);
    println!("newest message of thread carries INBOX: {newest_in_inbox}/{n}; among multi-message threads: {multi_newest_in_inbox}/{multi}; multi threads whose newest is UNREAD: {multi_unread_newest}; multi threads with >=1 non-INBOX message (e.g. sent reply): {multi_with_non_inbox}");
    println!("threads.list snippet == newest message snippet: {snippet_eq_last}/{n}; == newest INBOX message snippet: {snippet_eq_inbox_last}/{n}");
    println!("threads.list order consistent with newest-message-date desc: {order_by_newest_all}/{n} adjacent-or-better");

    // message list: how many of the newest 200 INBOX message rows belong to an already-seen thread?
    let mut seen: HashSet<String> = HashSet::new();
    let mut dup = 0;
    let mut tot = 0;
    for page in 0..1 {
        let _ = page;
        let p = list_page(&api, &[("labelIds", "INBOX"), ("maxResults", "200")]).await.unwrap_or_else(|e| die(&e.to_string()));
        for (_, tid) in p.ids {
            tot += 1;
            if !seen.insert(tid) {
                dup += 1;
            }
        }
    }
    println!("newest {tot} INBOX messages: {} distinct threads; {dup} rows ({:.1}%) belong to an already-listed thread", seen.len(), 100.0 * dup as f64 / tot.max(1) as f64);
    let all = api.st.snap().since(s0);
    println!("TOTAL: {} units, {} calls, 429s {}, errors {}", all.units, all.calls, all.s429, all.errs);
    println!("non-200 reasons: {:?}", api.st.reasons.lock().expect("lock"));
}

// ---- rows (Q5) ------------------------------------------------------------------------------

fn median_u64(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn analyse(name: &str, msgs: &[Value]) {
    let n = msgs.len().max(1);
    let mut from_enc = 0;
    let mut from_nonascii = 0;
    let mut from_changed = 0;
    let mut from_unparsed = 0;
    let mut from_noname = 0;
    let mut from_quoted = 0;
    let mut subj_enc = 0;
    let mut subj_nonascii = 0;
    let mut subj_changed = 0;
    let mut subj_missing = 0;
    let mut to_missing = 0;
    let mut date_missing = 0;
    let mut date_bad = 0;
    let (mut d_1m, mut d_1h, mut d_1d) = (0, 0, 0);
    let (mut d_hdr_later, mut d_hdr_earlier) = (0, 0);
    let mut dup_hdr = 0;
    let mut names: HashSet<String> = HashSet::new();
    let mut ent = BTreeMap::<&str, usize>::new();
    let mut flags = BTreeMap::<String, usize>::new();
    let mut mimes = BTreeMap::<String, usize>::new();
    let mut with_parts = 0;
    let mut with_filename = 0;
    let mut sizes = Vec::new();
    let mut snip_len = Vec::new();
    let mut payload_keys = BTreeMap::<String, usize>::new();
    let mut msg_keys = BTreeMap::<String, usize>::new();
    for m in msgs {
        if let Some(o) = m.as_object() {
            for k in o.keys() {
                *msg_keys.entry(k.clone()).or_default() += 1;
            }
        }
        if let Some(o) = m["payload"].as_object() {
            for k in o.keys() {
                *payload_keys.entry(k.clone()).or_default() += 1;
            }
        }
        for h in m["payload"]["headers"].as_array().into_iter().flatten() {
            if let Some(nm) = h["name"].as_str() {
                names.insert(nm.to_string());
            }
        }
        for h in HDRS {
            if header_count(m, h) > 1 {
                dup_hdr += 1;
            }
        }
        match header(m, "From") {
            Some(f) => {
                if f.contains("=?") {
                    from_enc += 1;
                }
                if !f.is_ascii() {
                    from_nonascii += 1;
                }
                if decode_rfc2047(f) != f {
                    from_changed += 1;
                }
                match parse_first_address(f) {
                    Some(a) => {
                        if a.name.is_none() {
                            from_noname += 1;
                        }
                        if !a.addr.contains('@') {
                            from_unparsed += 1;
                        }
                    }
                    None => from_unparsed += 1,
                }
                if f.trim_start().starts_with('"') {
                    from_quoted += 1;
                }
            }
            None => from_unparsed += 1,
        }
        match header(m, "Subject") {
            Some(s) => {
                if s.contains("=?") {
                    subj_enc += 1;
                }
                if !s.is_ascii() {
                    subj_nonascii += 1;
                }
                if decode_rfc2047(s) != s {
                    subj_changed += 1;
                }
            }
            None => subj_missing += 1,
        }
        if header(m, "To").is_none() {
            to_missing += 1;
        }
        match header(m, "Date") {
            None => date_missing += 1,
            Some(d) => match chrono::DateTime::parse_from_rfc2822(d.trim()) {
                Ok(dt) => {
                    let diff = dt.timestamp_millis() - internal_date(m).unwrap_or(0);
                    let a = diff.abs();
                    if a > 60_000 {
                        d_1m += 1;
                    }
                    if a > 3_600_000 {
                        d_1h += 1;
                        if diff > 0 {
                            d_hdr_later += 1;
                        } else {
                            d_hdr_earlier += 1;
                        }
                    }
                    if a > 86_400_000 {
                        d_1d += 1;
                    }
                }
                Err(_) => date_bad += 1,
            },
        }
        let sn = m["snippet"].as_str().unwrap_or("");
        snip_len.push(sn.chars().count() as u64);
        for (k, pat) in [("&#39;", "&#39;"), ("&amp;", "&amp;"), ("&quot;", "&quot;"), ("&lt;", "&lt;"), ("&gt;", "&gt;")] {
            if sn.contains(pat) {
                *ent.entry(k).or_default() += 1;
            }
        }
        if has_html_entity(sn) {
            *ent.entry("any entity").or_default() += 1;
        }
        if sn.contains('\'') {
            *ent.entry("literal '").or_default() += 1;
        }
        if sn.contains('&') {
            *ent.entry("literal &").or_default() += 1;
        }
        if sn.contains('<') {
            *ent.entry("literal <").or_default() += 1;
        }
        if !sn.is_ascii() {
            *ent.entry("non-ascii").or_default() += 1;
        }
        for l in m["labelIds"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            if is_system(l) {
                *flags.entry(l.to_string()).or_default() += 1;
            } else {
                *flags.entry("(user label)".into()).or_default() += 1;
            }
        }
        *mimes.entry(m["payload"]["mimeType"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        if m["payload"].get("parts").is_some() {
            with_parts += 1;
        }
        if m["payload"]["filename"].as_str().is_some_and(|f| !f.is_empty()) {
            with_filename += 1;
        }
        if let Some(s) = m["sizeEstimate"].as_u64() {
            sizes.push(s);
        }
    }
    {
        let idates: Vec<i64> = msgs.iter().filter_map(internal_date).collect();
        let hdates: Vec<i64> = msgs.iter().filter_map(|m| header(m, "Date").and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok()).map(|d| d.timestamp_millis())).collect();
        let inv = |v: &[i64]| v.windows(2).filter(|w| w[1] > w[0]).count();
        println!("list-order inversions: by internalDate {} (of {}), by Date header {} (of {})", inv(&idates), idates.len(), inv(&hdates), hdates.len());
    }
    println!("\n-- {name}: {} messages --", msgs.len());
    println!("msg keys: {msg_keys:?}; payload keys: {payload_keys:?}");
    println!("header names seen (casing as returned): {} distinct; duplicate-valued requested headers: {dup_hdr}", names.len());
    println!("From: contains '=?' {from_enc}, non-ASCII {from_nonascii}, rfc2047-decode changed {from_changed}, no display name {from_noname}, starts with quote {from_quoted}, unparsed {from_unparsed}");
    println!("Subject: contains '=?' {subj_enc}, non-ASCII {subj_nonascii}, rfc2047-decode changed {subj_changed}, missing {subj_missing}; To missing {to_missing}");
    println!("Date header: missing {date_missing}, unparsable {date_bad}; |Date - internalDate| >1min {d_1m}, >1h {d_1h} (header later {d_hdr_later}, header earlier {d_hdr_earlier}), >1d {d_1d}  [of {n}]", n = n);
    println!("snippet length: p50 {} max {}; patterns: {ent:?}", median_u64(&mut snip_len.clone()), snip_len.iter().max().copied().unwrap_or(0));
    println!("labels: {flags:?}");
    println!("payload.mimeType: {mimes:?}; payload.parts present {with_parts}; payload.filename non-empty {with_filename}");
    let mut s2 = sizes.clone();
    println!("sizeEstimate: p50 {} max {} min {}", median_u64(&mut s2), sizes.iter().max().copied().unwrap_or(0), sizes.iter().min().copied().unwrap_or(0));
}

async fn cmd_rows(email: &str) {
    let api = Api::new(email).await;
    let s0 = api.st.snap();
    println!("== rows for {} ==", tag(email));
    // Recent 200 + a deeper 100 from INBOX (page 3 of 500), plus SENT 50.
    let recent = list_page(&api, &[("labelIds", "INBOX"), ("maxResults", "200")]).await.unwrap_or_else(|e| die(&e.to_string()));
    let rids: Vec<String> = recent.ids.iter().map(|(i, _)| i.clone()).collect();
    let mut deep: Vec<String> = Vec::new();
    let mut tok: Option<String> = None;
    for p in 0..3 {
        let mut q = vec![("labelIds", "INBOX"), ("maxResults", "500")];
        if let Some(t) = tok.as_deref() {
            q.push(("pageToken", t));
        }
        let Ok(pg) = list_page(&api, &q).await else { break };
        tok = pg.next.clone();
        if p == 2 {
            deep = pg.ids.into_iter().take(100).map(|(i, _)| i).collect();
        }
        if tok.is_none() {
            break;
        }
    }
    let sent = list_page(&api, &[("labelIds", "SENT"), ("maxResults", "50")]).await.map(|p| p.ids.into_iter().map(|(i, _)| i).collect::<Vec<_>>()).unwrap_or_default();
    let att = list_page(&api, &[("labelIds", "INBOX"), ("q", "has:attachment"), ("maxResults", "60")]).await.map(|p| p.ids.into_iter().map(|(i, _)| i).collect::<Vec<_>>()).unwrap_or_default();
    let noatt = list_page(&api, &[("labelIds", "INBOX"), ("q", "-has:attachment"), ("maxResults", "60")]).await.map(|p| p.ids.into_iter().map(|(i, _)| i).collect::<Vec<_>>()).unwrap_or_default();
    for (name, ids) in [("recent INBOX 200", rids), ("deeper INBOX (page 3)", deep), ("SENT 50", sent), ("INBOX has:attachment", att), ("INBOX -has:attachment", noatt)] {
        if ids.is_empty() {
            println!("\n-- {name}: no ids --");
            continue;
        }
        let rows = hydrate_get_r(&api, &ids, 10, None).await;
        let ok: Vec<Value> = rows.into_iter().filter_map(Result::ok).collect();
        save(&format!("{}-rows-{}.json", tag(email), name.replace([' ', ':', '(', ')'], "_")), &Value::Array(ok.clone()));
        analyse(name, &ok);
    }
    let all = api.st.snap().since(s0);
    println!("\nTOTAL: {} units, {} calls, 429s {}, errors {}", all.units, all.calls, all.s429, all.errs);
    println!("non-200 reasons: {:?}", api.st.reasons.lock().expect("lock"));
}

// ---- cold (Q6) ------------------------------------------------------------------------------

async fn cmd_cold(email: &str, mode: &str) {
    let p0 = Instant::now();
    let tok_t = Instant::now();
    let tok = gtoken::access_token(email).await.unwrap_or_else(|e| die(&format!("token: {e}")));
    let token_ms = secs(tok_t.elapsed()) * 1000.0;
    // fresh client => fresh TLS/HTTP2 connection, as on app start
    let api = Api { http: gtoken::http(), tok: Arc::new(tok), st: Arc::new(Stats::default()) };
    println!("== cold open for {} (mode {mode}) ==", tag(email));
    println!("token refresh: {token_ms:.0} ms (includes first-use cost in this process)");
    let t = Instant::now();
    // Stage 1: labels.list and the unread counters for the folders we display, plus first list page, in parallel.
    let a1 = api.clone();
    let labels = tokio::spawn(async move { a1.jr("labels", &[]).await });
    let a2 = api.clone();
    let counts = tokio::spawn(async move {
        let ids: Vec<String> = ["INBOX", "STARRED", "DRAFT", "SPAM", "TRASH", "SENT", "IMPORTANT"].iter().map(|s| s.to_string()).collect();
        par(ids, 7, move |id| {
            let a = a2.clone();
            async move { a.jr(&format!("labels/{id}"), &[]).await }
        })
        .await
    });
    
    let t_list = Instant::now();
    let ids: Vec<String> = if mode == "threads" {
        let v = api.jr("threads", &[("labelIds", "INBOX"), ("maxResults", "50")]).await.unwrap_or_else(|e| die(&e.to_string()));
        v["threads"].as_array().map(|a| a.iter().filter_map(|t| t["id"].as_str().map(String::from)).collect()).unwrap_or_default()
    } else {
        let p = list_page(&api, &[("labelIds", "INBOX"), ("maxResults", "50")]).await.unwrap_or_else(|e| die(&e.to_string()));
        p.ids.into_iter().map(|(i, _)| i).collect()
    };
    let list_ms = secs(t_list.elapsed()) * 1000.0;
    let t_h = Instant::now();
    let rows = if mode == "threads" { hydrate_threads(&api, &ids, 10, Some(FIELDS_THR)).await } else if mode == "batch" { hydrate_batch(&api, &ids, Some(FIELDS_MSG)).await.unwrap_or_default() } else { hydrate_get(&api, &ids, 10, Some(FIELDS_MSG)).await };
    let hyd_ms = secs(t_h.elapsed()) * 1000.0;
    let first20_ms = secs(p0.elapsed()) * 1000.0;
    let lab = labels.await.expect("labels task");
    let cnt = counts.await.expect("counts task");
    let all_ms = secs(t.elapsed()) * 1000.0;
    let st = api.st.snap();
    println!("list: {list_ms:.0} ms ({} ids); hydrate: {hyd_ms:.0} ms ({}/{} ok)", ids.len(), ok_count(&rows), rows.len());
    println!("labels.list ok: {}, labels.get ok: {}/{}", lab.is_ok(), cnt.iter().filter(|r| r.is_ok()).count(), cnt.len());
    println!("process start -> inbox rows ready (incl. token): {first20_ms:.0} ms; folder pane + rows all ready: {:.0} ms", token_ms + all_ms);
    println!("units: {}, calls {}, KB {:.0}, 429s {}, errors {}", st.units, st.calls, st.bytes as f64 / 1024.0, st.s429, st.errs);
}
