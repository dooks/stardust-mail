//! Pure helpers for spike S2 (no network): label tree, RFC 2047, addresses, batch framing, stats.

use base64::Engine;
use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// Label tree
// ---------------------------------------------------------------------------------------------

/// One row of the flattened folder tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Full "/"-joined path.
    pub path: String,
    /// Last path segment.
    pub name: String,
    /// 0 for top level.
    pub depth: usize,
    /// Gmail label id, `None` when the parent exists only because a child name implies it.
    pub label_id: Option<String>,
}

impl TreeNode {
    /// True when no label of this name exists (a pure grouping node).
    pub fn is_synthetic(&self) -> bool {
        self.label_id.is_none()
    }
}

/// Build a depth-first, case-insensitively sorted tree from `(id, full name)` user labels.
pub fn build_tree(labels: &[(String, String)]) -> Vec<TreeNode> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<(Vec<String>, String), TreeNode> = BTreeMap::new();
    let key = |segs: &[&str]| -> (Vec<String>, String) { (segs.iter().map(|s| s.to_lowercase()).collect(), segs.join("/")) };
    for (id, name) in labels {
        let segs: Vec<&str> = name.split('/').collect();
        for i in 1..=segs.len() {
            let part = &segs[..i];
            let node = map.entry(key(part)).or_insert_with(|| TreeNode {
                path: part.join("/"),
                name: part[i - 1].to_string(),
                depth: i - 1,
                label_id: None,
            });
            if i == segs.len() {
                node.label_id = Some(id.clone());
            }
        }
    }
    map.into_values().collect()
}

// ---------------------------------------------------------------------------------------------
// RFC 2047
// ---------------------------------------------------------------------------------------------

fn q_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = h {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'=');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn charset_to_string(cs: &str, bytes: &[u8]) -> String {
    match cs.to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "windows-1252" | "cp1252" => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn parse_encoded_word(after: &str) -> Option<(String, &str)> {
    let c_end = after.find('?')?;
    let charset = &after[..c_end];
    let r = &after[c_end + 1..];
    let e_end = r.find('?')?;
    let enc = &r[..e_end];
    let r2 = &r[e_end + 1..];
    let t_end = r2.find("?=")?;
    let text = &r2[..t_end];
    if enc.len() != 1 || charset.is_empty() || charset.contains(char::is_whitespace) || text.contains(char::is_whitespace) {
        return None;
    }
    let bytes = match enc {
        "B" | "b" => base64::engine::general_purpose::STANDARD.decode(text.as_bytes()).ok()?,
        "Q" | "q" => q_decode(text),
        _ => return None,
    };
    let charset = charset.split('*').next().unwrap_or(charset);
    Some((charset_to_string(charset, &bytes), &r2[t_end + 2..]))
}

/// Decode RFC 2047 encoded words (B and Q). Whitespace between adjacent encoded words is dropped.
/// Charsets other than UTF-8/ASCII/Latin-1 fall back to lossy UTF-8.
pub fn decode_rfc2047(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    let mut prev_encoded = false;
    while let Some(start) = rest.find("=?") {
        let before = &rest[..start];
        let after = &rest[start + 2..];
        match parse_encoded_word(after) {
            Some((decoded, tail)) => {
                if !(prev_encoded && before.chars().all(char::is_whitespace)) {
                    out.push_str(before);
                }
                out.push_str(&decoded);
                prev_encoded = true;
                rest = tail;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                prev_encoded = false;
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------------------------

/// A parsed single mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    /// Display name, decoded and unquoted, when present.
    pub name: Option<String>,
    /// The addr-spec.
    pub addr: String,
}

/// Parse the first mailbox of a From/To style header value.
pub fn parse_first_address(raw: &str) -> Option<Addr> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut in_q = false;
    let mut esc = false;
    let mut lt = None;
    let mut gt = None;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_q => esc = true,
            '"' => in_q = !in_q,
            '<' if !in_q && lt.is_none() => lt = Some(i),
            '>' if !in_q && lt.is_some() && gt.is_none() => gt = Some(i),
            ',' if !in_q && lt.is_none() => break,
            _ => {}
        }
    }
    if let (Some(l), Some(g)) = (lt, gt) {
        let addr = s[l + 1..g].trim().to_string();
        let name = decode_rfc2047(&unquote(s[..l].trim())).trim().to_string();
        return Some(Addr { name: if name.is_empty() { None } else { Some(name) }, addr });
    }
    let first = s.split(',').next().unwrap_or(s).trim();
    Some(Addr { name: None, addr: first.to_string() })
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let mut out = String::new();
        let mut esc = false;
        for c in t[1..t.len() - 1].chars() {
            if esc {
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// Gmail JSON helpers
// ---------------------------------------------------------------------------------------------

/// First header value by case-insensitive name from `payload.headers`.
pub fn header<'a>(msg: &'a Value, name: &str) -> Option<&'a str> {
    msg["payload"]["headers"].as_array()?.iter().find_map(|h| {
        if h["name"].as_str()?.eq_ignore_ascii_case(name) { h["value"].as_str() } else { None }
    })
}

/// How many headers with this name are present.
pub fn header_count(msg: &Value, name: &str) -> usize {
    msg["payload"]["headers"]
        .as_array()
        .map_or(0, |a| a.iter().filter(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))).count())
}

/// `internalDate` (ms since epoch) of a message object.
pub fn internal_date(msg: &Value) -> Option<i64> {
    msg["internalDate"].as_str()?.parse().ok()
}

/// True when the text contains an HTML character reference like `&#39;` or `&amp;`.
pub fn has_html_entity(s: &str) -> bool {
    for (i, _) in s.match_indices('&') {
        if let Some(end) = s[i..].find(';') {
            let body = &s[i + 1..i + end];
            if (1..=8).contains(&body.len()) && (body.starts_with('#') || body.chars().all(|c| c.is_ascii_alphabetic())) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Quota
// ---------------------------------------------------------------------------------------------

/// Quota units for a GET path relative to `users/me/` (per Google's quota table).
pub fn unit_cost(path: &str) -> u64 {
    let mut it = path.split('/');
    let res = it.next().unwrap_or("");
    let has_id = it.next().is_some();
    match (res, has_id) {
        ("labels", _) | ("profile", _) => 1,
        ("messages", false) => 5,
        ("messages", true) => 20,
        ("threads", false) => 10,
        ("threads", true) => 40,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Batch framing
// ---------------------------------------------------------------------------------------------

/// Build a multipart/mixed batch body containing only GET sub-requests.
/// `gets` are paths like `/gmail/v1/users/me/messages/ID?format=metadata`.
pub fn build_batch_body(boundary: &str, gets: &[String]) -> String {
    let mut s = String::new();
    for (i, g) in gets.iter().enumerate() {
        s.push_str(&format!("--{boundary}\r\nContent-Type: application/http\r\nContent-ID: <item{i}>\r\n\r\nGET {g}\r\n\r\n"));
    }
    s.push_str(&format!("--{boundary}--\r\n"));
    s
}

/// Extract `boundary=` from a Content-Type value.
pub fn boundary_of(content_type: &str) -> Option<String> {
    let i = content_type.find("boundary=")?;
    let v = content_type[i + 9..].split(';').next()?.trim().trim_matches('"');
    Some(v.to_string())
}

/// Parse a batch response into `(status, json body)` per part, in order.
pub fn parse_batch_response(body: &str, boundary: &str) -> Vec<(u16, Option<Value>)> {
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    for part in body.split(&delim) {
        let Some(h) = part.find("HTTP/1.") else { continue };
        let inner = &part[h..];
        let status = inner.get(9..12).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
        let json = inner.find("\r\n\r\n").map(|p| inner[p + 4..].trim()).filter(|t| t.starts_with('{'));
        out.push((status, json.and_then(|t| serde_json::from_str(t).ok())));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------------------------

/// Percentile by nearest rank (`p` in 0..=100). Returns 0 for empty input.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(id: &str, n: &str) -> (String, String) {
        (id.into(), n.into())
    }

    #[test]
    fn tree_synthetic_parent() {
        let t = build_tree(&[l("L1", "Work/Projects/Alpha"), l("L3", "Home/Bills"), l("L4", "Zed")]);
        let got: Vec<_> = t.iter().map(|n| (n.path.as_str(), n.depth, n.is_synthetic())).collect();
        assert_eq!(
            got,
            vec![
                ("Home", 0, true),
                ("Home/Bills", 1, false),
                ("Work", 0, true),
                ("Work/Projects", 1, true),
                ("Work/Projects/Alpha", 2, false),
                ("Zed", 0, false)
            ]
        );
    }

    #[test]
    fn rfc2047_basic() {
        assert_eq!(decode_rfc2047("=?UTF-8?B?w6RiYw==?="), "äbc");
        assert_eq!(decode_rfc2047("=?utf-8?Q?caf=C3=A9_au_lait?="), "café au lait");
        assert_eq!(decode_rfc2047("=?iso-8859-1?Q?caf=E9?="), "café");
        assert_eq!(decode_rfc2047("plain text"), "plain text");
        assert_eq!(decode_rfc2047("a =?UTF-8?Q?b?= =?UTF-8?Q?c?= d"), "a bc d");
        assert_eq!(decode_rfc2047("broken =?UTF-8?X?zz?= end"), "broken =?UTF-8?X?zz?= end");
    }

    #[test]
    fn address_forms() {
        let a = parse_first_address("\"Doe, Jane\" <jane@example.com>, other@example.com").unwrap();
        assert_eq!(a, Addr { name: Some("Doe, Jane".into()), addr: "jane@example.com".into() });
        let b = parse_first_address("=?UTF-8?B?w6RiYw==?= <abc@example.com>").unwrap();
        assert_eq!(b.name.as_deref(), Some("äbc"));
        let c = parse_first_address("bare@example.com").unwrap();
        assert_eq!(c, Addr { name: None, addr: "bare@example.com".into() });
        let d = parse_first_address("<only@example.com>").unwrap();
        assert_eq!(d.name, None);
        let e = parse_first_address("\"=?UTF-8?Q?caf=C3=A9?=\" <x@example.com>").unwrap();
        assert_eq!(e.name.as_deref(), Some("café"));
        assert!(parse_first_address("  ").is_none());
    }

    #[test]
    fn entities() {
        assert!(has_html_entity("it&#39;s"));
        assert!(has_html_entity("a &amp; b"));
        assert!(!has_html_entity("a & b; c"));
        assert!(!has_html_entity("plain"));
    }

    #[test]
    fn costs() {
        assert_eq!(unit_cost("labels"), 1);
        assert_eq!(unit_cost("labels/INBOX"), 1);
        assert_eq!(unit_cost("messages"), 5);
        assert_eq!(unit_cost("messages/abc"), 20);
        assert_eq!(unit_cost("threads"), 10);
        assert_eq!(unit_cost("threads/abc"), 40);
        assert_eq!(unit_cost("profile"), 1);
    }

    #[test]
    fn batch_roundtrip() {
        let body = build_batch_body("bb", &["/gmail/v1/users/me/messages/a?format=metadata".into(), "/gmail/v1/users/me/messages/b".into()]);
        assert!(body.contains("GET /gmail/v1/users/me/messages/a?format=metadata\r\n"));
        assert!(!body.contains("POST") && !body.contains("DELETE"));
        assert!(body.ends_with("--bb--\r\n"));
        assert_eq!(boundary_of("multipart/mixed; boundary=batch_xyz"), Some("batch_xyz".into()));
        let resp = "--rb\r\nContent-Type: application/http\r\nContent-ID: <response-item0>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":\"a\"}\r\n--rb\r\nContent-Type: application/http\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"error\":{}}\r\n--rb--";
        let parts = parse_batch_response(resp, "rb");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, 200);
        assert_eq!(parts[0].1.as_ref().unwrap()["id"], "a");
        assert_eq!(parts[1].0, 404);
    }

    #[test]
    fn pct() {
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 95.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn headers() {
        let m: Value = serde_json::json!({"payload":{"headers":[{"name":"From","value":"x"},{"name":"subject","value":"s"}]}});
        assert_eq!(header(&m, "FROM"), Some("x"));
        assert_eq!(header(&m, "Subject"), Some("s"));
        assert_eq!(header_count(&m, "to"), 0);
    }
}
