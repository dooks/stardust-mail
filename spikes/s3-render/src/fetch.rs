//! Network side of the spike: pick a diverse sample and save it under `out\`.
//!
//! Allowed endpoints only: `messages.list`, `messages.get` (`format=raw` and `format=full`), and
//! `messages.attachments.get`. All calls are GET. Nothing here prints message content.

use std::collections::HashSet;
use std::fs;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::sample::{Meta, out_dir};

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// One sampling query: a tag (printed), a `q` string, optional label filter, and a share of the sample.
struct Pick {
    tag: &'static str,
    q: &'static str,
    label: Option<&'static str>,
    share: usize,
}

const PICKS: &[Pick] = &[
    Pick { tag: "inbox", q: "", label: Some("INBOX"), share: 6 },
    Pick { tag: "attachment", q: "has:attachment", label: None, share: 6 },
    Pick { tag: "promotions", q: "category:promotions", label: None, share: 5 },
    Pick { tag: "updates", q: "category:updates", label: None, share: 5 },
    Pick {
        tag: "code",
        q: "subject:(code OR verification OR verify OR passcode OR otp)",
        label: None,
        share: 6,
    },
    Pick { tag: "large", q: "larger:2M", label: None, share: 2 },
    Pick { tag: "sent", q: "", label: Some("SENT"), share: 2 },
    Pick { tag: "intl", q: "from:(.jp OR .ru OR .cn OR .kr OR .pl OR .cz OR .gr OR .il)", label: None, share: 3 },
    Pick { tag: "social", q: "category:social OR category:forums", label: None, share: 3 },
];

/// Only the status code goes into the error text, never URLs with query strings or bodies.
async fn get_json(http: &reqwest::Client, token: &str, url: &str, query: &[(&str, &str)]) -> Result<Value, String> {
    let resp = http.get(url).query(query).bearer_auth(token).send().await.map_err(|_| "transport error")?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    resp.json().await.map_err(|_| "json error".to_string())
}

/// Alias for an account: `a1`, `a2` ... by order of first use; stored in `out\accounts.json`.
fn alias_for(email: &str) -> Result<String, String> {
    let path = out_dir().join("accounts.json");
    let mut map: serde_json::Map<String, Value> =
        fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for (alias, v) in &map {
        if v.as_str() == Some(email) {
            return Ok(alias.clone());
        }
    }
    let alias = format!("a{}", map.len() + 1);
    map.insert(alias.clone(), Value::String(email.to_string()));
    fs::create_dir_all(out_dir()).map_err(|e| e.to_string())?;
    fs::write(&path, serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(alias)
}

pub async fn fetch(email: &str, n: usize) -> Result<(), String> {
    let allowed = ["account-a@example.com", "account-b@example.com"];
    if !allowed.contains(&email) {
        return Err("this spike only uses its two assigned accounts".into());
    }
    let alias = alias_for(email)?;
    let http = gtoken::http();
    let token = gtoken::access_token(email).await.map_err(|e| format!("token: {e}"))?;
    let total_share: usize = PICKS.iter().map(|p| p.share).sum();
    let raw_dir = out_dir().join("raw").join(&alias);
    let meta_dir = out_dir().join("meta").join(&alias);
    let full_dir = out_dir().join("full").join(&alias);
    for d in [&raw_dir, &meta_dir, &full_dir] {
        fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut chosen: Vec<(String, &'static str)> = Vec::new();
    for p in PICKS {
        let want = (n * p.share).div_ceil(total_share).max(1);
        let max = (want * 3).clamp(10, 100).to_string();
        let mut query: Vec<(&str, &str)> = vec![("maxResults", &max)];
        if !p.q.is_empty() {
            query.push(("q", p.q));
        }
        if let Some(l) = p.label {
            query.push(("labelIds", l));
        }
        let v = get_json(&http, &token, &format!("{API}/messages"), &query).await?;
        let ids: Vec<String> = v["messages"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut taken = 0;
        // Spread the picks over the result page instead of taking only the newest few.
        let step = (ids.len() / want).max(1);
        for id in ids.iter().step_by(step) {
            if taken >= want {
                break;
            }
            if seen.insert(id.clone()) {
                chosen.push((id.clone(), p.tag));
                taken += 1;
            }
        }
        println!("query {:<11} candidates {:>3}  taken {:>2}", p.tag, ids.len(), taken);
    }
    println!("{alias}: fetching {} messages (raw + full)", chosen.len());

    for (id, tag) in &chosen {
        if raw_dir.join(format!("{id}.eml")).exists() && full_dir.join(format!("{id}.json")).exists() {
            continue;
        }
        let raw = get_json(&http, &token, &format!("{API}/messages/{id}"), &[("format", "raw")]).await?;
        let bytes = URL_SAFE_NO_PAD
            .decode(raw["raw"].as_str().unwrap_or("").trim_end_matches('='))
            .map_err(|_| "raw base64".to_string())?;
        fs::write(raw_dir.join(format!("{id}.eml")), &bytes).map_err(|e| e.to_string())?;
        let meta = Meta {
            id: id.clone(),
            thread_id: raw["threadId"].as_str().unwrap_or_default().to_string(),
            label_ids: raw["labelIds"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            size_estimate: raw["sizeEstimate"].as_u64().unwrap_or(0),
            internal_date_ms: raw["internalDate"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
            tag: (*tag).to_string(),
        };
        fs::write(meta_dir.join(format!("{id}.json")), serde_json::to_vec(&meta).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let full = get_json(&http, &token, &format!("{API}/messages/{id}"), &[("format", "full")]).await?;
        fs::write(full_dir.join(format!("{id}.json")), serde_json::to_vec(&full).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    }
    println!("{alias}: done");
    Ok(())
}

/// `messages.attachments.get` for one attachment; returns the decoded bytes.
pub async fn attachment_bytes(email: &str, msg_id: &str, attachment_id: &str) -> Result<Vec<u8>, String> {
    let http = gtoken::http();
    let token = gtoken::access_token(email).await.map_err(|e| format!("token: {e}"))?;
    let v = get_json(&http, &token, &format!("{API}/messages/{msg_id}/attachments/{attachment_id}"), &[]).await?;
    URL_SAFE_NO_PAD
        .decode(v["data"].as_str().unwrap_or("").trim_end_matches('='))
        .map_err(|_| "attachment base64".to_string())
}


/// Wall time (ms) and response size (bytes of JSON) of one `messages.get` in the given format (`raw` or `full`).
pub async fn time_get(email: &str, msg_id: &str, format: &str) -> Result<(u128, usize), String> {
    let http = gtoken::http();
    let token = gtoken::access_token(email).await.map_err(|e| format!("token: {e}"))?;
    let t = std::time::Instant::now();
    let resp = http
        .get(format!("{API}/messages/{msg_id}"))
        .query(&[("format", format)])
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|_| "transport error")?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let n = resp.bytes().await.map_err(|_| "body error")?.len();
    Ok((t.elapsed().as_millis(), n))
}
