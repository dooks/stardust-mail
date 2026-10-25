//! `stats`: content-free counts over the saved sample. Prints ids, counts, sizes, MIME types, header NAMES and
//! pass/fail tallies only. Never subjects, addresses, snippets, file names or bodies.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::compare;
use crate::parse::{self, BodyKind};
use crate::sample::{self, SampleRef};
use crate::sanitize::{self, ImagePolicy};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

fn dist(name: &str, vals: &[f64], unit: &str) {
    let mut v = vals.to_vec();
    println!(
        "  {name}: n={} min={:.0}{unit} p50={:.0}{unit} p90={:.0}{unit} p99={:.0}{unit} max={:.0}{unit}",
        v.len(),
        pct(&mut v.clone(), 0.0),
        pct(&mut v.clone(), 0.5),
        pct(&mut v.clone(), 0.9),
        pct(&mut v.clone(), 0.99),
        pct(&mut v, 1.0)
    );
}

fn bump(m: &mut BTreeMap<String, usize>, k: impl Into<String>) {
    *m.entry(k.into()).or_default() += 1;
}

fn print_map(title: &str, m: &BTreeMap<String, usize>, top: usize) {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    println!("  {title}:");
    for (k, n) in v.into_iter().take(top) {
        println!("    {n:>4}  {k}");
    }
}

pub fn run() {
    let all: Vec<SampleRef> = sample::list();
    println!("== sample ==");
    println!("messages: {}", all.len());
    let mut per_tag = BTreeMap::new();
    let mut per_acct = BTreeMap::new();
    for s in &all {
        bump(&mut per_acct, s.alias.clone());
        bump(&mut per_tag, s.meta().tag);
    }
    print_map("per account", &per_acct, 10);
    print_map("per sampling query", &per_tag, 20);

    let mut raw_sizes = Vec::new();
    let mut full_json_sizes = Vec::new();
    let mut size_ratio = Vec::new();
    let (mut parse_fail, mut with_html, mut html_only, mut text_only, mut both, mut none) = (0, 0, 0, 0, 0, 0);
    let mut shapes = BTreeMap::new();
    let mut charsets = BTreeMap::new();
    let (mut non_utf8_msgs, mut charset_problem_msgs, mut flowed) = (0, 0, 0);
    let (mut msgs_with_att, mut att_total, mut cid_total, mut cid_referenced) = (0, 0, 0, 0);
    let mut att_mimes = BTreeMap::new();
    let mut att_sizes = Vec::new();
    let (mut html_msgs, mut remote_msgs, mut remote_total, mut pixel_msgs, mut pixel_total) = (0, 0, 0, 0, 0);
    let (mut css_remote_msgs, mut style_msgs, mut dark_media_msgs, mut cid_missing_msgs, mut data_img_msgs) = (0, 0, 0, 0, 0);
    let mut san_ms = Vec::new();
    let mut san_ratio = Vec::new();
    let (mut auth_any, mut auth_trusted, mut auth_multi, mut auth_untrusted_only) = (0, 0, 0, 0);
    let (mut spf, mut dkim, mut dmarc) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let (mut spf_missing, mut dkim_missing, mut dmarc_missing) = (0, 0, 0);
    let (mut mailed_by, mut signed_by, mut levels) = (0, 0, BTreeMap::new());
    let (mut sent_no_auth, mut sent_total, mut non_sent_no_auth) = (0, 0, 0);
    let (mut date_missing, mut date_far) = (0, 0);
    let mut date_diffs = Vec::new();
    // [mail-parser bodies, absent in full, equal using declared charset, equal assuming UTF-8, equal under neither]
    let mut cmp_html = [0usize; 5];
    let mut cmp_text = [0usize; 5];
    let (mut cmp_att_eq, mut cmp_att_diff) = (0, 0);
    let (mut body_by_aid, mut full_missing, mut full_unknown_cs) = (0, 0, 0);
    let (mut enc_header_msgs, mut enc_leftover, mut full_encoded_words) = (0, 0, 0);
    let mut subject_cmp = [0usize; 2];
    let mut from_cmp = [0usize; 4];
    let mut parts_total = Vec::new();

    for s in &all {
        let Some(raw) = s.raw() else { continue };
        let meta = s.meta();
        raw_sizes.push(raw.len() as f64);
        if meta.size_estimate > 0 {
            size_ratio.push(raw.len() as f64 / meta.size_estimate as f64);
        }
        if let Ok(md) = std::fs::metadata(s.full_path()) {
            full_json_sizes.push(md.len() as f64);
        }
        let Some(p) = parse::parse(&raw) else {
            parse_fail += 1;
            continue;
        };
        parts_total.push(p.part_count as f64);
        bump(&mut shapes, p.structure.clone());
        for c in &p.charsets {
            bump(&mut charsets, c.clone());
        }
        if p.charsets.iter().any(|c| !matches!(c.as_str(), "utf-8" | "us-ascii" | "ascii")) {
            non_utf8_msgs += 1;
        }
        if p.charset_problems > 0 {
            charset_problem_msgs += 1;
        }
        {
            let head = &raw[..raw.len().min(400_000)];
            if String::from_utf8_lossy(head).to_ascii_lowercase().contains("format=flowed") {
                flowed += 1;
            }
        }
        match (p.has_html_part, p.has_text_part) {
            (true, true) => {
                both += 1;
                with_html += 1;
            }
            (true, false) => {
                html_only += 1;
                with_html += 1;
            }
            (false, true) => text_only += 1,
            (false, false) => none += 1,
        }
        // attachments
        if !p.attachments.is_empty() {
            msgs_with_att += 1;
        }
        for a in &p.attachments {
            att_total += 1;
            bump(&mut att_mimes, a.mime.clone());
            att_sizes.push(a.size as f64);
            if a.content_id.is_some() {
                cid_total += 1;
            }
            if a.inline_referenced {
                cid_referenced += 1;
            }
        }
        // sanitize
        if p.body_kind() == BodyKind::Html {
            html_msgs += 1;
            let html = p.html.as_deref().unwrap_or("");
            let cids = p.cids.clone();
            let t = Instant::now();
            let (frag, rep) = sanitize::sanitize(
                html,
                ImagePolicy::Allow,
                std::sync::Arc::new(move |id| cids.get(id).map(|n| format!("http://gmail-asset.localhost/x/y/{n}"))),
            );
            san_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            san_ratio.push(frag.len() as f64 / html.len().max(1) as f64);
            if rep.remote_images > 0 {
                remote_msgs += 1;
                remote_total += rep.remote_images;
            }
            if rep.tracking_pixels > 0 {
                pixel_msgs += 1;
                pixel_total += rep.tracking_pixels;
            }
            if rep.remote_css_urls > 0 {
                css_remote_msgs += 1;
            }
            style_msgs += usize::from(rep.has_style_block);
            dark_media_msgs += usize::from(rep.has_dark_media);
            cid_missing_msgs += usize::from(rep.cid_missing > 0);
            data_img_msgs += usize::from(rep.data_images > 0);
        }
        // auth
        let is_sent = meta.label_ids.iter().any(|l| l == "SENT");
        sent_total += usize::from(is_sent);
        if p.auth_header_count > 0 {
            auth_any += 1;
        }
        if p.auth_header_count > 1 {
            auth_multi += 1;
        }
        match &p.auth {
            Some(a) => {
                auth_trusted += 1;
                match &a.spf {
                    Some(v) => bump(&mut spf, v.label()),
                    None => spf_missing += 1,
                }
                match a.dkim_overall() {
                    Some(v) => bump(&mut dkim, v.label()),
                    None => dkim_missing += 1,
                }
                match &a.dmarc {
                    Some(v) => bump(&mut dmarc, v.label()),
                    None => dmarc_missing += 1,
                }
                mailed_by += usize::from(a.mailed_by.is_some());
                signed_by += usize::from(a.signed_by.is_some());
                bump(&mut levels, a.level());
            }
            None => {
                if p.auth_header_count > 0 {
                    auth_untrusted_only += 1;
                }
                if is_sent {
                    sent_no_auth += 1;
                } else {
                    non_sent_no_auth += 1;
                }
            }
        }
        // dates
        match p.date_ts {
            None => date_missing += 1,
            Some(t) => {
                if meta.internal_date_ms > 0 {
                    let d = ((meta.internal_date_ms / 1000) as i64 - t).abs();
                    date_diffs.push(d as f64);
                    if d > 86_400 {
                        date_far += 1;
                    }
                }
            }
        }

        // raw vs full
        let Some(full) = std::fs::read(s.full_path()).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
            full_missing += 1;
            continue;
        };
        let fv = compare::view(&full);
        full_unknown_cs += usize::from(fv.unknown_charsets > 0);
        body_by_aid += fv.body_parts_by_attachment_id;
        let cmp = |mine: &Option<String>, declared: &[String], utf8: &[String], sep: &str, c: &mut [usize; 5]| {
            let Some(m) = mine else { return };
            c[0] += 1;
            if declared.is_empty() {
                c[1] += 1;
                return;
            }
            let want = compare::norm_body(m);
            if compare::norm_body(&declared.join(sep)) == want {
                c[2] += 1;
            }
            if compare::norm_body(&utf8.join(sep)) == want {
                c[3] += 1;
            }
            if compare::norm_body(&declared.join(sep)) != want && compare::norm_body(&utf8.join(sep)) != want {
                c[4] += 1;
            }
        };
        cmp(&p.html, &fv.html, &fv.html_u8, "\n", &mut cmp_html);
        cmp(&p.text, &fv.text, &fv.text_u8, "\n\n", &mut cmp_text);
        // Attachment lists: same count and same multiset of (mime, size)?
        let mut a: Vec<(String, u64)> = fv.attachments.iter().map(|x| (x.mime.clone(), x.size)).collect();
        let mut b: Vec<(String, u64)> = p.attachments.iter().map(|x| (x.mime.clone(), x.size as u64)).collect();
        a.sort();
        b.sort();
        if a == b {
            cmp_att_eq += 1;
        } else {
            cmp_att_diff += 1;
        }
        // Encoded words: raw header values in `full` that need RFC 2047 decoding vs what mail-parser left behind.
        if p.had_encoded_words {
            enc_header_msgs += 1;
            let leftovers = p.subject.as_deref().is_some_and(|x| x.contains("=?"))
                || p.from.iter().chain(&p.to).chain(&p.cc).any(|a| a.name.as_deref().is_some_and(|n| n.contains("=?")));
            enc_leftover += usize::from(leftovers);
            // Does format=full already carry the decoded text?
            let full_subject = fv.headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| v.as_str()).unwrap_or("");
            full_encoded_words += usize::from(full_subject.contains("=?"));
        }
        let ws = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let full_subject = fv.headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| ws(v)).unwrap_or_default();
        subject_cmp[0] += 1;
        subject_cmp[1] += usize::from(full_subject == ws(p.subject.as_deref().unwrap_or("")));
        let full_from = fv.headers.iter().find(|(n, _)| n == "from").map(|(_, v)| v.to_ascii_lowercase()).unwrap_or_default();
        from_cmp[0] += 1;
        from_cmp[1] += usize::from(p.from.first().is_some_and(|a| full_from.contains(&a.address.to_ascii_lowercase())));
        from_cmp[2] += usize::from(p.from.first().and_then(|a| a.name.as_ref()).is_some_and(|n| full_from.contains(&n.to_ascii_lowercase())));
        from_cmp[3] += usize::from(p.from.first().and_then(|a| a.name.as_ref()).is_some());
    }

    println!("\n== Q1 raw size (bytes of the decoded .eml) ==");
    dist("raw size", &raw_sizes.iter().map(|x| *x / 1024.0).collect::<Vec<_>>(), " KB");
    let buckets = [(10_240.0, "<10 KB"), (102_400.0, "<100 KB"), (1_048_576.0, "<1 MB"), (5_242_880.0, "<5 MB"), (f64::MAX, ">=5 MB")];
    let mut prev = 0.0;
    for (lim, name) in buckets {
        let n = raw_sizes.iter().filter(|&&x| x >= prev && x < lim).count();
        println!("    {n:>4}  {name}");
        prev = lim;
    }
    let raw_total: f64 = raw_sizes.iter().sum();
    let full_total: f64 = full_json_sizes.iter().sum();
    println!(
        "  total raw bytes {:.1} MB (JSON transfer ~x1.37 for base64url) vs total format=full JSON {:.1} MB",
        raw_total / 1_048_576.0,
        full_total / 1_048_576.0
    );
    let mut r = size_ratio.clone();
    println!("  raw_len / sizeEstimate: p50 {:.3} min {:.3} max {:.3}", pct(&mut r.clone(), 0.5), pct(&mut r.clone(), 0.0), pct(&mut r, 1.0));
    println!("  parse failures: {parse_fail}; missing format=full copies: {full_missing}");
    dist("parts per message", &parts_total, "");
    print_map("MIME tree shapes (top 14)", &shapes, 14);
    print_map("text charsets", &charsets, 12);
    println!("  messages with a non-UTF-8/ASCII text charset: {non_utf8_msgs}");
    println!("  messages with a charset problem reported by mail-parser (unknown charset / malformed bytes): {charset_problem_msgs}");
    println!("  messages containing format=flowed: {flowed}");
    println!("  -- raw+mail-parser vs format=full payload tree (naive decode with encoding_rs) --");
    for (name, c) in [("HTML", cmp_html), ("text", cmp_text)] {
        println!("  {name} bodies (compared modulo whitespace): {} in mail-parser; absent in full {}; full decoded with its DECLARED charset equal {}; full decoded as UTF-8 equal {}; equal under neither {}", c[0], c[1], c[2], c[3], c[4]);
    }
    println!("  attachment lists ((mime,size) multisets): equal {cmp_att_eq}, different {cmp_att_diff}");
    println!("  body-type parts delivered only as attachmentId in format=full (text not inline): {body_by_aid}");
    println!("  messages where a text part declares a charset encoding_rs does not know: {full_unknown_cs}");
    println!("  messages with RFC 2047 encoded words in raw Subject/From/To/Cc: {enc_header_msgs}; still undecoded after mail-parser: {enc_leftover}; of those, Subject still encoded in format=full: {full_encoded_words}");
    println!("  Subject equal between mail-parser and format=full header: {}/{}; From address found in full header {}/{}; From display name (decoded) found in full header {}/{} with a name", subject_cmp[1], subject_cmp[0], from_cmp[1], from_cmp[0], from_cmp[2], from_cmp[3]);

    println!("\n== Q2 body choice ==");
    println!("  html+text: {both}  html only: {html_only}  text only: {text_only}  no body: {none}  (html total {with_html})");

    println!("\n== Q3 sanitizing (images allowed pass, {html_msgs} HTML messages) ==");
    dist("sanitize time", &san_ms, " ms");
    let mut sr = san_ratio.clone();
    println!("  output/input bytes: p50 {:.2} min {:.2} max {:.2}", pct(&mut sr.clone(), 0.5), pct(&mut sr.clone(), 0.0), pct(&mut sr, 1.0));
    println!("  messages with <style> blocks: {style_msgs}; with prefers-color-scheme CSS: {dark_media_msgs}; with data: images: {data_img_msgs}");

    println!("\n== Q4 remote images ==");
    println!("  HTML messages with >=1 remote <img>/background: {remote_msgs}/{html_msgs} ({remote_total} references)");
    println!("  messages with a tracking-pixel-like image (<=2px): {pixel_msgs}/{html_msgs} ({pixel_total} pixels)");
    println!("  messages with remote CSS url(): {css_remote_msgs}/{html_msgs}");

    println!("\n== Q5 attachments / cid ==");
    println!("  messages with attachments() entries: {msgs_with_att}; entries: {att_total}; with Content-ID: {cid_total}; referenced inline by cid: {cid_referenced}");
    println!("  HTML messages with a cid: reference that has no matching part: {cid_missing_msgs}");
    print_map("attachment MIME types", &att_mimes, 12);
    dist("attachment size", &att_sizes.iter().map(|x| *x / 1024.0).collect::<Vec<_>>(), " KB");

    println!("\n== Q6 Authentication-Results ==");
    println!("  messages with any Authentication-Results header: {auth_any}/{}", all.len());
    println!("  with more than one: {auth_multi}; with Google's (trusted) one: {auth_trusted}; only non-Google ones: {auth_untrusted_only}");
    println!("  SENT messages: {sent_total}, of which without trusted result: {sent_no_auth}; non-SENT without trusted result: {non_sent_no_auth}");
    print_map("SPF", &spf, 10);
    print_map("DKIM (overall)", &dkim, 10);
    print_map("DMARC", &dmarc, 10);
    println!("  trusted but missing: SPF {spf_missing}, DKIM {dkim_missing}, DMARC {dmarc_missing}; mailed-by derivable {mailed_by}, signed-by derivable {signed_by}");
    print_map("summary level", &levels, 6);

    println!("\n== dates ==");
    println!("  Date header missing/invalid: {date_missing}; |Date - internalDate| > 1 day: {date_far}");
    let mut dd = date_diffs.clone();
    println!("  |Date - internalDate| seconds: p50 {:.0} p90 {:.0} max {:.0}", pct(&mut dd.clone(), 0.5), pct(&mut dd.clone(), 0.9), pct(&mut dd, 1.0));
}

