//! PLAN §3.4: the crate's source must never contain a write endpoint or a non-GET method (except the one batch
//! POST). The forbidden strings are built with `concat!` so this file doesn't contain them literally.

use std::fs;
use std::path::Path;

fn src_files() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            out.push((p.file_name().unwrap().to_string_lossy().into_owned(), fs::read_to_string(&p).unwrap()));
        }
    }
    assert!(out.len() >= 4, "expected the crate sources");
    out
}

#[test]
fn no_write_endpoints_in_source() {
    let forbidden: Vec<String> = [
        ["/mod", "ify"],
        ["/tra", "sh"],
        ["/untra", "sh"],
        ["/se", "nd"],
        ["/ins", "ert"],
        ["/imp", "ort"],
        ["/batchMod", "ify"],
        ["/batchDel", "ete"],
        ["/wat", "ch"],
        ["/st", "op"],
        ["/dra", "fts"],
        ["/sett", "ings"],
        ["\"DEL", "ETE\""],
        ["\"PU", "T\""],
        ["\"PAT", "CH\""],
    ]
    .iter()
    .map(|p| p.concat())
    .collect();
    for (name, text) in src_files() {
        for f in &forbidden {
            assert!(!text.contains(f.as_str()), "{name} contains forbidden `{f}`");
        }
    }
}

#[test]
fn only_one_non_get_call_the_batch_post() {
    let verbs: Vec<String> =
        [[".pu", "t("], [".pat", "ch("], [".dele", "te("], [".hea", "d("]].iter().map(|p| p.concat()).collect();
    let post = [".po", "st("].concat();
    let mut posts = 0;
    for (name, text) in src_files() {
        for v in &verbs {
            assert!(!text.contains(v.as_str()), "{name} uses `{v}`");
        }
        assert!(!text.contains("Method::"), "{name} builds a raw method");
        assert!(!text.contains(".request("), "{name} builds a raw request");
        posts += text.matches(post.as_str()).count();
    }
    assert_eq!(posts, 1, "exactly one POST (the batch endpoint) is allowed");
}
