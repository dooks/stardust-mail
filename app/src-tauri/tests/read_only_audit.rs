//! Workspace-wide read-only audit (CLAUDE.md "Read-only, by construction").
//!
//! Scans every `.rs` file under `crates/` and `app/src-tauri/src/` for Gmail write endpoints and HTTP verbs that
//! could modify anything, and for POST calls outside the known allow-list.
//!
//! Scope notes:
//! - `tests/` directories of other crates are skipped: `gmail-api/tests/read_only.rs` legitimately spells the
//!   forbidden strings in its own assertions. This file lives in `app/src-tauri/tests/`, which is not scanned.
//! - Inline `#[cfg(test)]` modules ARE scanned, so the mock servers' paths (`/revoke`, `/tokeninfo`) are covered.
//! - The forbidden strings are assembled with `concat!` so this file never matches itself.
//! - `.delete(` is only a violation in a file that uses `reqwest`; `store.delete(..)` on the local token store is
//!   not an HTTP call.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_SEGMENTS: &[&str] = &[
    concat!("/", "modify"),
    concat!("/", "trash"),
    concat!("/", "untrash"),
    concat!("/", "send"),
    concat!("/", "insert"),
    concat!("/", "import"),
    concat!("/", "batch", "Modify"),
    concat!("/", "batch", "Delete"),
    concat!("/", "watch"),
    concat!("/", "stop"),
    concat!("/", "drafts"),
    concat!("/", "settings"),
];

const FORBIDDEN_VERBS: &[&str] = &[concat!(".", "put("), concat!(".", "patch(")];
const DELETE_VERB: &str = concat!(".", "delete(");
const POST_CALL: &str = concat!(".", "post(");

/// The only POSTs the app may make: (file suffix, text that must be on the same line).
/// Gmail batch (a POST that carries only GET sub-requests), and oauth2 token / revoke / tokeninfo.
const ALLOWED_POSTS: &[(&str, &str)] = &[
    ("gauth/src/login.rs", "endpoints.token"),
    ("gauth/src/tokens.rs", "endpoints.token"),
    ("gauth/src/revoke.rs", ".post(url)"), // revoke_at / tokeninfo_scopes_at; URLs asserted below
    ("gmail-api/src/client.rs", "c.post(url.clone())"), // batch_once; URL asserted below
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name == "tests" || name == "target" || name == "node_modules" {
                continue;
            }
            rs_files(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

fn scanned_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut v = Vec::new();
    rs_files(&root.join("crates"), &mut v);
    rs_files(&root.join("app").join("src-tauri").join("src"), &mut v);
    v
}

/// Is `needle` at `idx` a whole path segment (next char is not part of a name or file extension, so local files
/// like `settings.json` or `import.css` do not count)?
fn segment_at(text: &str, idx: usize, needle: &str) -> bool {
    text[idx + needle.len()..].chars().next().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
}

fn audit(files: &[(String, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (name, text) in files {
        let norm = name.replace('\\', "/");
        for seg in FORBIDDEN_SEGMENTS {
            for (idx, _) in text.match_indices(seg) {
                if segment_at(text, idx, seg) {
                    bad.push(format!("{norm}: forbidden endpoint segment {seg}"));
                }
            }
        }
        for verb in FORBIDDEN_VERBS {
            if text.contains(verb) {
                bad.push(format!("{norm}: forbidden HTTP verb {verb}"));
            }
        }
        if text.contains(DELETE_VERB) && text.contains("reqwest") {
            bad.push(format!("{norm}: forbidden HTTP verb {DELETE_VERB} in a file using reqwest"));
        }
        for line in text.lines().filter(|l| l.contains(POST_CALL)) {
            let ok = ALLOWED_POSTS.iter().any(|(suffix, must)| norm.ends_with(suffix) && line.contains(must));
            if !ok {
                bad.push(format!("{norm}: POST outside the allow-list: {}", line.trim()));
            }
        }
    }
    bad
}

fn load() -> Vec<(String, String)> {
    let root = workspace_root();
    scanned_files()
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().into_owned();
            (rel, fs::read_to_string(&p).unwrap_or_default())
        })
        .collect()
}

#[test]
fn workspace_contains_no_write_endpoints_or_verbs() {
    let files = load();
    assert!(files.len() > 30, "the scan found only {} files; paths are wrong", files.len());
    let bad = audit(&files);
    assert!(bad.is_empty(), "read-only violations:\n{}", bad.join("\n"));
}

#[test]
fn the_scan_covers_the_crates_that_talk_to_google() {
    let names: Vec<String> = load().into_iter().map(|(n, _)| n.replace('\\', "/")).collect();
    for must in ["crates/gmail-api/src/client.rs", "crates/gauth/src/revoke.rs", "crates/mail-service/src/inner.rs", "app/src-tauri/src/lib.rs"] {
        assert!(names.iter().any(|n| n == must), "{must} not scanned");
    }
}

#[test]
fn allowed_posts_target_the_known_endpoints() {
    let root = workspace_root();
    let read = |p: &str| fs::read_to_string(root.join(p)).unwrap();
    // The Gmail batch endpoint is a multipart POST of GET sub-requests.
    assert!(read("crates/gmail-api/src/lib.rs").contains("BATCH_PATH: &str = \"batch/gmail/v1\""));
    assert!(read("crates/gmail-api/src/client.rs").contains("extend(BATCH_PATH.split('/'))"));
    // The default OAuth endpoints are the three oauth2.googleapis.com ones.
    let c = read("crates/gauth/src/client.rs");
    for ep in ["oauth2.googleapis.com/token\"", "oauth2.googleapis.com/revoke\"", "oauth2.googleapis.com/tokeninfo\""] {
        assert!(c.contains(ep), "{ep}");
    }
}

#[test]
fn the_audit_itself_detects_violations() {
    let send_url = format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{}{}", "x", concat!("/", "send"));
    let files = vec![
        ("a.rs".to_string(), format!("let u = \"{send_url}\";")),
        ("b.rs".to_string(), format!("client{}url)", concat!(".", "put("))),
        ("c.rs".to_string(), format!("use reqwest; c{}u)", concat!(".", "delete("))),
        ("d.rs".to_string(), format!("c{}evil_url)", POST_CALL)),
        ("e.rs".to_string(), "let ok = \"/sender\"; store.delete(&id);".to_string()),
    ];
    let bad = audit(&files);
    assert_eq!(bad.len(), 4, "{bad:?}");
    assert!(bad.iter().all(|b| !b.starts_with("e.rs")));
}
