//! `s3` CLI: fetch a sample, print content-free stats, render messages to HTML files under `out\`.
//!
//! ```text
//! s3 fetch <email> [--n 40]       save raw + full copies of a diverse sample (no content printed)
//! s3 stats                        content-free counts over the sample
//! s3 render <id>                  out\<id>.html (iframe page) + out\<id>.view.html (header + sandboxed iframe)
//! s3 render-all [--images]        the same for every message in the sample
//! s3 time <email> [--k 4]         raw vs full latency for a few messages (sizes and ms only)
//! s3 verify-att <email> [--k 6]   compare messages.attachments.get bytes with the attachment bytes inside raw
//! ```

use std::sync::Arc;

use s3render::render::{self, CidMode, DarkPolicy, RenderOpts};
use s3render::sample::{self, SampleRef};
use s3render::sanitize::ImagePolicy;
use s3render::{compare, fetch, parse, stats, text};

fn arg_val(a: &[String], flag: &str) -> Option<String> {
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned()
}

fn now_ts() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Write `out\<id>.html` and `out\<id>.view.html`; returns a content-free summary line.
fn render_one(s: &SampleRef, images: ImagePolicy) -> Result<String, String> {
    let raw = s.raw().ok_or("missing raw")?;
    let p = parse::parse(&raw).ok_or("unparseable")?;
    let meta = s.meta();
    let opts = RenderOpts { account: s.alias.clone(), msg_id: s.id.clone(), images, dark: DarkPolicy::HtmlWhite };
    let r = render::render(&p, &opts, &CidMode::Asset);
    std::fs::write(sample::out_dir().join(format!("{}.html", s.id)), &r.page).map_err(|e| e.to_string())?;

    // Standalone page for a plain browser: cid images inlined as data: URIs, header block above the iframe.
    let inline = render::render(&p, &opts, &CidMode::Inline(Arc::new(raw.clone())));
    let mv = render::meta_view(&p, &meta, &s.alias, now_ts());
    let view = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{id}</title><style>\
         body{{margin:0;font:14px/1.45 system-ui,'Segoe UI',sans-serif;background:#f1f3f4;color:#202124}}\
         .hdr{{background:#fff;padding:12px 16px;border-bottom:1px solid #dadce0}}.hdr h2{{margin:0 0 8px;font-size:18px}}\
         .hdr div{{margin:2px 0}}.hdr b{{display:inline-block;min-width:78px;color:#5f6368;font-weight:600}}\
         .auth.good{{color:#137333}}.auth.bad{{color:#b3261e}}.auth.mixed{{color:#b06000}}.auth.unknown{{color:#5f6368}}\
         iframe{{display:block;width:100%;height:calc(100vh - 230px);min-height:400px;border:0;background:#fff}}\
         </style><div class=\"hdr\">{hdr}</div><iframe sandbox=\"allow-popups allow-popups-to-escape-sandbox\" srcdoc=\"{doc}\"></iframe>",
        id = s.id,
        hdr = render::header_html(&mv),
        doc = text::escape(&inline.page)
    );
    std::fs::write(sample::out_dir().join(format!("{}.view.html", s.id)), view).map_err(|e| e.to_string())?;
    Ok(format!(
        "{} {} body={:?} page={}B remote_img={} cid_ok={} cid_missing={}",
        s.alias,
        s.id,
        r.body,
        r.page.len(),
        r.report.remote_images,
        r.report.cid_images,
        r.report.cid_missing
    ))
}

async fn time_cmd(email: &str, k: usize) -> Result<(), String> {
    let accts = sample::account_map();
    let alias = accts.iter().find(|(_, e)| e == email).map(|(a, _)| a.clone()).ok_or("account not in sample")?;
    let mut mine: Vec<SampleRef> = sample::list().into_iter().filter(|s| s.alias == alias).collect();
    mine.sort_by_key(|s| std::fs::metadata(s.raw_path()).map(|m| m.len()).unwrap_or(0));
    // smallest, median, largest few
    let n = mine.len();
    let mut picks: Vec<&SampleRef> = Vec::new();
    for i in 0..k {
        picks.push(&mine[(i * (n - 1)) / (k - 1).max(1)]);
    }
    println!("{:<10} {:>10} {:>10} {:>10} {:>10}", "id", "raw bytes", "raw ms", "full json", "full ms");
    for s in picks {
        let (r_ms, r_len) = fetch::time_get(email, &s.id, "raw").await?;
        let (f_ms, f_len) = fetch::time_get(email, &s.id, "full").await?;
        println!("{:<10} {:>10} {:>10} {:>10} {:>10}", &s.id[..s.id.len().min(10)], r_len, r_ms, f_len, f_ms);
    }
    Ok(())
}

async fn verify_att(email: &str, k: usize) -> Result<(), String> {
    let accts = sample::account_map();
    let alias = accts.iter().find(|(_, e)| e == email).map(|(a, _)| a.clone()).ok_or("account not in sample")?;
    let (mut checked, mut identical, mut size_only) = (0, 0, 0);
    let mut cands: Vec<(SampleRef, compare::FullAtt)> = Vec::new();
    for s in sample::list().into_iter().filter(|s| s.alias == alias) {
        let Some(full) = std::fs::read(s.full_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) else { continue };
        for a in compare::view(&full).attachments {
            if a.attachment_id.is_some() && a.size > 0 && a.size < 3_000_000 {
                cands.push((s.clone(), a));
            }
        }
    }
    cands.sort_by_key(|(_, a)| a.size);
    let step = (cands.len() / k.max(1)).max(1);
    for (s, a) in cands.iter().step_by(step).take(k) {
        let got = fetch::attachment_bytes(email, &s.id, a.attachment_id.as_deref().unwrap_or("")).await?;
        let raw = s.raw().ok_or("raw")?;
        let p = parse::parse(&raw).ok_or("parse")?;
        let same = p.attachments.iter().filter(|x| x.size as u64 == a.size && x.mime == a.mime).any(|x| {
            parse::part_bytes(&raw, x.part_id).is_some_and(|(_, b)| b == got)
        });
        checked += 1;
        if same {
            identical += 1;
        } else if got.len() as u64 == a.size {
            size_only += 1;
        }
    }
    println!("{alias}: attachments.get checked {checked}, byte-identical to the part inside raw {identical}, same size but different bytes {size_only}");
    Ok(())
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let cmd = a.get(1).map(String::as_str).unwrap_or("");
    let res: Result<(), String> = match cmd {
        "fetch" => {
            let n = arg_val(&a, "--n").and_then(|s| s.parse().ok()).unwrap_or(40);
            fetch::fetch(a.get(2).map_or("", String::as_str), n).await
        }
        "stats" => {
            stats::run();
            Ok(())
        }
        "render" => match a.get(2).and_then(|id| sample::find(id)) {
            Some(s) => render_one(&s, ImagePolicy::Block).map(|l| println!("{l}")),
            None => Err("unknown id".into()),
        },
        "render-all" => {
            let images = if a.iter().any(|x| x == "--images") { ImagePolicy::Allow } else { ImagePolicy::Block };
            let mut fails = 0;
            for s in sample::list() {
                match render_one(&s, images) {
                    Ok(l) => println!("{l}"),
                    Err(e) => {
                        fails += 1;
                        println!("{} {} FAILED {e}", s.alias, s.id);
                    }
                }
            }
            println!("render-all: {fails} failures");
            Ok(())
        }
        "time" => time_cmd(a.get(2).map_or("", String::as_str), arg_val(&a, "--k").and_then(|s| s.parse().ok()).unwrap_or(4)).await,
        "verify-att" => verify_att(a.get(2).map_or("", String::as_str), arg_val(&a, "--k").and_then(|s| s.parse().ok()).unwrap_or(6)).await,
        _ => {
            eprintln!("usage: s3 fetch|stats|render|render-all|time|verify-att  (see src/main.rs)");
            Ok(())
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
