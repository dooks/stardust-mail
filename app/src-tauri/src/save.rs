//! Saving an attachment: the save dialog (opened from Rust), the file-name sanitizer and the `FileSaver` the
//! service calls. JS never sees bytes or paths.
//!
//! Privacy: only sizes are logged, never names or paths.

use std::path::PathBuf;

use mail_model::{CmdError, SaveResult};
use mail_service::FileSaver;
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use crate::settings::Settings;

/// Longest file name offered to the dialog (UTF-8 bytes of the name, extension included).
const MAX_NAME_BYTES: usize = 120;
/// Used when nothing usable is left of the name.
const FALLBACK_NAME: &str = "attachment";

/// Device names Windows refuses as file names, with or without an extension.
fn is_reserved_stem(stem: &str) -> bool {
    let s = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((s.starts_with("COM") || s.starts_with("LPT")) && s.len() == 4 && matches!(s.as_bytes()[3], b'1'..=b'9'))
}

/// Make an attachment name safe to offer as a Windows file name: keep only the last path component, replace
/// `\ / : * ? " < > |` and control characters with `_`, drop trailing dots and spaces, prefix reserved device
/// names with `_`, cap the length (keeping the extension), and fall back to `attachment` when nothing is left.
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).find(|s| !s.trim().is_empty()).unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    out = out.trim_matches(' ').trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() || out.chars().all(|c| c == '.' || c == '_') {
        return FALLBACK_NAME.to_string();
    }
    if out.len() > MAX_NAME_BYTES {
        let (stem, ext) = match out.rfind('.') {
            Some(i) if out.len() - i <= 16 && i > 0 => (out[..i].to_string(), out[i..].to_string()),
            _ => (out.clone(), String::new()),
        };
        let mut keep = MAX_NAME_BYTES.saturating_sub(ext.len());
        while !stem.is_char_boundary(keep) {
            keep -= 1;
        }
        out = format!("{}{}", stem[..keep].trim_end_matches(['.', ' ']), ext);
    }
    let stem = out.split('.').next().unwrap_or("");
    if is_reserved_stem(stem) {
        out.insert(0, '_');
    }
    out
}

/// The dialog-backed [`FileSaver`]. `save` blocks until the user answers, so call it off the async workers.
pub struct DialogSaver {
    pub app: AppHandle,
    pub window: Option<WebviewWindow>,
    /// App data dir that holds `settings.json`.
    pub data_dir: PathBuf,
}

impl DialogSaver {
    /// Where the dialog opens: the last saved folder if it still exists, else Downloads.
    fn start_dir(&self, settings: &Settings) -> Option<PathBuf> {
        settings.last_save_dir.clone().filter(|p| p.is_dir()).or_else(|| self.app.path().download_dir().ok())
    }
}

impl FileSaver for DialogSaver {
    fn save(&self, suggested_name: &str, bytes: &[u8]) -> Result<SaveResult, CmdError> {
        let name = sanitize_file_name(suggested_name);
        let mut settings = Settings::load(&self.data_dir);

        let mut dialog = self.app.dialog().file().set_file_name(&name);
        if let Some(dir) = self.start_dir(&settings) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(w) = &self.window {
            dialog = dialog.set_parent(w);
        }
        let Some(chosen) = dialog.blocking_save_file() else {
            return Ok(SaveResult { saved: false, file_name: None });
        };
        let path = chosen.into_path().map_err(|_| CmdError::internal("the chosen location is not a file path"))?;
        std::fs::write(&path, bytes).map_err(|e| CmdError::internal(format!("could not write the file: {}", e.kind())))?;

        if let Some(parent) = path.parent() {
            settings.last_save_dir = Some(parent.to_path_buf());
            if let Err(e) = settings.store(&self.data_dir) {
                tracing::warn!(kind = ?e.kind(), "could not persist the last save folder");
            }
        }
        tracing::info!(bytes = bytes.len(), "attachment saved");
        Ok(SaveResult { saved: true, file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(sanitize_file_name("Quarterly report.pdf"), "Quarterly report.pdf");
        assert_eq!(sanitize_file_name("héllo wörld.txt"), "héllo wörld.txt");
        assert_eq!(sanitize_file_name(".gitignore"), ".gitignore");
    }

    #[test]
    fn forbidden_characters_are_replaced() {
        assert_eq!(sanitize_file_name(r#"a\b/c:d*e?f"g<h>i|j.txt"#), "c_d_e_f_g_h_i_j.txt", "only the last component is kept");
        assert_eq!(sanitize_file_name(r#"inv:oice*2026?.txt"#), "inv_oice_2026_.txt");
        assert_eq!(sanitize_file_name("a\"b<c>d|e.txt"), "a_b_c_d_e.txt");
        assert_eq!(sanitize_file_name("tab\there\u{0}.txt"), "tab_here_.txt");
        assert_eq!(sanitize_file_name("file.txt:stream"), "file.txt_stream");
    }

    #[test]
    fn path_traversal_is_flattened() {
        assert_eq!(sanitize_file_name(r"..\..\Windows\evil.dll"), "evil.dll");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("C:\\dir\\"), "dir");
        assert_eq!(sanitize_file_name(".."), FALLBACK_NAME);
        assert_eq!(sanitize_file_name("/"), FALLBACK_NAME);
    }

    #[test]
    fn reserved_device_names_are_prefixed() {
        for n in ["CON", "con.txt", "Nul.tar.gz", "AUX.", "com1", "LPT9.log", "prn .txt", "CONIN$"] {
            assert!(sanitize_file_name(n).starts_with('_'), "{n}");
        }
        for n in ["COM0.txt", "COM10", "console.txt", "lpt.txt", "CONX"] {
            assert!(!sanitize_file_name(n).starts_with('_'), "{n}");
        }
    }

    #[test]
    fn trailing_dots_and_spaces_go() {
        assert_eq!(sanitize_file_name("name. . "), "name");
        assert_eq!(sanitize_file_name("  spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_file_name(""), FALLBACK_NAME);
        assert_eq!(sanitize_file_name("   "), FALLBACK_NAME);
        assert_eq!(sanitize_file_name("???"), FALLBACK_NAME);
    }

    #[test]
    fn long_names_keep_the_extension_and_stay_on_char_boundaries() {
        let long = format!("{}.pdf", "x".repeat(400));
        let s = sanitize_file_name(&long);
        assert!(s.len() <= MAX_NAME_BYTES && s.ends_with(".pdf"));
        let wide = format!("{}.txt", "é".repeat(300));
        let s = sanitize_file_name(&wide);
        assert!(s.len() <= MAX_NAME_BYTES && s.ends_with(".txt"));
    }
}
