//! `settings.json` in the app data dir: the only app setting is the last folder an attachment was saved to.
//!
//! Nothing here is secret, but the path is user data: callers must never log it.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the app data directory.
pub const FILE_NAME: &str = "settings.json";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Folder of the last saved attachment; `None` until the first save (the dialog then opens in Downloads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_save_dir: Option<PathBuf>,
}

impl Settings {
    /// Read `<dir>/settings.json`. A missing or unreadable/corrupt file yields the defaults.
    pub fn load(dir: &Path) -> Settings {
        std::fs::read(dir.join(FILE_NAME))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write `<dir>/settings.json` via a temporary file and a rename, creating `dir` if needed.
    pub fn store(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, dir.join(FILE_NAME))
    }
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh empty directory under the system temp dir (removed on drop).
    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new(tag: &str) -> TestDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "gmail-ro-test-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("create test dir");
            TestDir(p)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TestDir;
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let d = TestDir::new("settings-missing");
        assert_eq!(Settings::load(&d.0), Settings::default());
        assert_eq!(Settings::load(&d.0.join("nope")), Settings::default());
    }

    #[test]
    fn roundtrip_and_overwrite() {
        let d = TestDir::new("settings-rt");
        let s = Settings { last_save_dir: Some(PathBuf::from(r"C:\Users\x\Documents")) };
        s.store(&d.0).unwrap();
        assert_eq!(Settings::load(&d.0), s);
        let s2 = Settings { last_save_dir: Some(PathBuf::from(r"D:\other")) };
        s2.store(&d.0).unwrap();
        assert_eq!(Settings::load(&d.0), s2);
        assert!(!d.0.join("settings.json.tmp").exists(), "no tmp file left behind");
    }

    #[test]
    fn store_creates_the_directory() {
        let d = TestDir::new("settings-mkdir");
        let nested = d.0.join("a").join("b");
        Settings::default().store(&nested).unwrap();
        assert!(nested.join(FILE_NAME).is_file());
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let d = TestDir::new("settings-corrupt");
        std::fs::write(d.0.join(FILE_NAME), b"{ not json").unwrap();
        assert_eq!(Settings::load(&d.0), Settings::default());
    }

    #[test]
    fn json_is_camel_case() {
        let s = Settings { last_save_dir: Some(PathBuf::from("x")) };
        assert!(serde_json::to_string(&s).unwrap().contains("lastSaveDir"));
    }
}
