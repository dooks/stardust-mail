//! The DPAPI token store: one `<sub>.dpapi` file per account.

use std::fmt;
use std::path::{Path, PathBuf};

use mail_model::{AccountId, AuthError};
use serde::{Deserialize, Serialize};

use crate::TOKEN_ENTROPY;

/// What is persisted per account (DPAPI-encrypted JSON, entropy [`crate::TOKEN_ENTROPY`]).
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
    /// Google's stable `sub` claim; also the file name stem and the [`AccountId`].
    pub sub: String,
    pub refresh_token: String,
    /// Scopes Google reported as granted.
    pub scopes: Vec<String>,
    /// Seconds since the epoch.
    pub created_at: u64,
    /// Set when the refresh token died (`invalid_grant`) or Gmail refused the scope; cleared by a new sign-in.
    #[serde(default)]
    pub needs_reauth: bool,
}

impl StoredAccount {
    /// The account id (the `sub`).
    pub fn id(&self) -> AccountId {
        AccountId(self.sub.clone())
    }
}

impl fmt::Debug for StoredAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAccount")
            .field("sub", &self.sub)
            .field("refresh_token", &"<redacted>")
            .field("scopes", &self.scopes)
            .field("needs_reauth", &self.needs_reauth)
            .finish()
    }
}

/// Directory of `<sub>.dpapi` files (`%LOCALAPPDATA%\com.ziggy.stardust-mail\accounts`). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// A `sub` is used as a file name, so it must be a plain token.
pub(crate) fn valid_sub(sub: &str) -> bool {
    !sub.is_empty()
        && sub.len() <= 128
        && sub
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn io_err(what: &str, e: std::io::Error) -> AuthError {
    AuthError::Store(format!("{what}: {:?}", e.kind()))
}

impl Store {
    /// Use `dir` (created if missing).
    pub fn open(dir: &Path) -> Result<Store, AuthError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| io_err("cannot create the account directory", e))?;
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, sub: &str) -> Result<PathBuf, AuthError> {
        if !valid_sub(sub) {
            return Err(AuthError::Store(
                "account id has unexpected characters".into(),
            ));
        }
        Ok(self.dir.join(format!("{sub}.dpapi")))
    }

    fn read_file(path: &Path) -> Option<StoredAccount> {
        let bytes = std::fs::read(path).ok()?;
        let plain = win_dpapi::unprotect(&bytes, TOKEN_ENTROPY).ok()?;
        serde_json::from_slice::<StoredAccount>(&plain).ok()
    }

    /// All readable accounts, and how many files were skipped (undecryptable or malformed: skipped, never fatal).
    pub fn list(&self) -> (Vec<StoredAccount>, usize) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return (Vec::new(), 0);
        };
        let (mut out, mut skipped) = (Vec::new(), 0usize);
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("dpapi") {
                continue;
            }
            match Self::read_file(&p) {
                // The file name must agree with the content, or `delete`/`set_needs_reauth` could not find it.
                Some(a) if p.file_stem().and_then(|s| s.to_str()) == Some(a.sub.as_str()) => {
                    out.push(a)
                }
                _ => skipped += 1,
            }
        }
        out.sort_by(|a, b| a.email.cmp(&b.email).then_with(|| a.sub.cmp(&b.sub)));
        if skipped > 0 {
            tracing::warn!(skipped, "token files skipped (undecryptable or malformed)");
        }
        (out, skipped)
    }

    /// Write via tmp file + rename (atomic). Replaces an existing account with the same `sub`.
    pub fn save(&self, account: &StoredAccount) -> Result<(), AuthError> {
        let path = self.path(&account.sub)?;
        let json = serde_json::to_vec(account)
            .map_err(|_| AuthError::Store("cannot serialize the account".into()))?;
        let enc = win_dpapi::protect(&json, TOKEN_ENTROPY)
            .map_err(|_| AuthError::Store("DPAPI encryption failed".into()))?;
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| io_err("cannot create the account directory", e))?;
        let tmp = path.with_extension("dpapi.tmp");
        std::fs::write(&tmp, enc).map_err(|e| io_err("cannot write the token file", e))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io_err("cannot replace the token file", e)
        })
    }

    /// Persist the `needs_reauth` flag of one account.
    pub fn set_needs_reauth(&self, id: &AccountId, needs_reauth: bool) -> Result<(), AuthError> {
        let path = self.path(&id.0)?;
        let Some(mut account) = Self::read_file(&path) else {
            return Err(AuthError::NotSignedIn);
        };
        if account.needs_reauth == needs_reauth {
            return Ok(());
        }
        account.needs_reauth = needs_reauth;
        self.save(&account)
    }

    /// Delete the token file. `Ok(true)` if a file was removed.
    pub fn delete(&self, id: &AccountId) -> Result<bool, AuthError> {
        let path = self.path(&id.0)?;
        let _ = std::fs::remove_file(path.with_extension("dpapi.tmp"));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err("cannot delete the token file", e)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gauth-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn sample(sub: &str, email: &str) -> StoredAccount {
        StoredAccount {
            email: email.into(),
            sub: sub.into(),
            refresh_token: "synthetic-refresh".into(),
            scopes: vec!["openid".into(), crate::SCOPE_GMAIL_READONLY.into()],
            created_at: 1_700_000_000,
            needs_reauth: false,
        }
    }

    #[test]
    fn replace_and_delete_leave_no_strays() {
        let d = tmp("store");
        let s = Store::open(&d.join("accounts")).unwrap();
        s.save(&sample("111", "a@example.com")).unwrap();
        s.save(&sample("222", "b@example.com")).unwrap();
        s.save(&sample("111", "a-renamed@example.com")).unwrap();
        let (all, bad) = s.list();
        assert_eq!((all.len(), bad), (2, 0));
        assert_eq!(all[0].email, "a-renamed@example.com");
        let raw = std::fs::read(d.join("accounts").join("111.dpapi")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("synthetic-refresh"));
        assert!(s.delete(&AccountId::new("111")).unwrap());
        assert!(!s.delete(&AccountId::new("111")).unwrap());
        let names: Vec<_> = std::fs::read_dir(d.join("accounts"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("222.dpapi")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_and_foreign_files_are_skipped_and_counted() {
        let d = tmp("corrupt");
        let s = Store::open(&d).unwrap();
        s.save(&sample("111", "a@example.com")).unwrap();
        std::fs::write(d.join("999.dpapi"), b"garbage").unwrap();
        // Encrypted with other entropy: undecryptable for us.
        std::fs::write(
            d.join("888.dpapi"),
            win_dpapi::protect(b"{}", b"other-entropy").unwrap(),
        )
        .unwrap();
        std::fs::write(d.join("notes.txt"), b"ignored").unwrap();
        let (all, bad) = s.list();
        assert_eq!((all.len(), bad), (1, 2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_sub_rejected() {
        let d = tmp("badsub");
        let s = Store::open(&d).unwrap();
        assert!(matches!(
            s.save(&sample("../evil", "x@example.com")),
            Err(AuthError::Store(_))
        ));
        assert!(s.delete(&AccountId::new("a/b")).is_err());
        assert!(s.save(&sample("", "x@example.com")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn needs_reauth_is_persisted_and_cleared_by_resave() {
        let d = tmp("reauth");
        let s = Store::open(&d).unwrap();
        s.save(&sample("111", "a@example.com")).unwrap();
        s.set_needs_reauth(&AccountId::new("111"), true).unwrap();
        assert!(s.list().0[0].needs_reauth);
        // A fresh sign-in saves a new account value, which clears the flag.
        s.save(&sample("111", "a@example.com")).unwrap();
        assert!(!s.list().0[0].needs_reauth);
        assert_eq!(
            s.set_needs_reauth(&AccountId::new("404"), true),
            Err(AuthError::NotSignedIn)
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn debug_hides_refresh_token() {
        assert!(!format!("{:?}", sample("1", "a@example.com")).contains("synthetic-refresh"));
    }
}
