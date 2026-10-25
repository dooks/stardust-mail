//! The saved sample under `out\`: `raw\<alias>\<id>.eml`, `meta\<alias>\<id>.json`, `full\<alias>\<id>.json`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Per-message sidecar written next to the `.eml` (labels are the only thing Gmail gives besides the MIME).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Meta {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub size_estimate: u64,
    pub internal_date_ms: u64,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct SampleRef {
    pub alias: String,
    pub id: String,
}

static OUT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Override where `out` lives (the Tauri harness runs from another directory). Call once, before any other use.
pub fn set_out_dir(p: PathBuf) {
    let _ = OUT.set(p);
}

pub fn out_dir() -> PathBuf {
    OUT.get().cloned().unwrap_or_else(|| PathBuf::from("out"))
}

impl SampleRef {
    pub fn raw_path(&self) -> PathBuf {
        out_dir().join("raw").join(&self.alias).join(format!("{}.eml", self.id))
    }
    pub fn meta_path(&self) -> PathBuf {
        out_dir().join("meta").join(&self.alias).join(format!("{}.json", self.id))
    }
    pub fn full_path(&self) -> PathBuf {
        out_dir().join("full").join(&self.alias).join(format!("{}.json", self.id))
    }
    pub fn raw(&self) -> Option<Vec<u8>> {
        fs::read(self.raw_path()).ok()
    }
    pub fn meta(&self) -> Meta {
        fs::read(self.meta_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Meta { id: self.id.clone(), ..Meta::default() })
    }
}

/// All saved messages, sorted by alias then id.
pub fn list() -> Vec<SampleRef> {
    let mut out = Vec::new();
    let Ok(accts) = fs::read_dir(out_dir().join("raw")) else { return out };
    for a in accts.flatten() {
        let alias = a.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(a.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".eml") {
                out.push(SampleRef { alias: alias.clone(), id: id.to_string() });
            }
        }
    }
    out.sort_by(|x, y| (&x.alias, &x.id).cmp(&(&y.alias, &y.id)));
    out
}

/// Find a sample by id (any account).
pub fn find(id: &str) -> Option<SampleRef> {
    list().into_iter().find(|s| s.id == id)
}

/// alias -> email, from `out\accounts.json`. Emails never leave `out\`.
pub fn account_map() -> Vec<(String, String)> {
    let map: serde_json::Map<String, serde_json::Value> = fs::read(out_dir().join("accounts.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    map.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect()
}
