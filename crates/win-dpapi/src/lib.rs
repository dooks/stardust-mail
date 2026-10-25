//! DPAPI wrapper (CurrentUser scope, `CRYPTPROTECT_UI_FORBIDDEN`). Copied from spike S0 with the entropy made a
//! parameter: callers pass their own (`gmail-ro-token` for tokens, `gmail-ro-cache-key` for cache keys), so another
//! process running as the same user cannot call `CryptUnprotectData` blindly.
//!
//! The crate is Windows-only by nature; on other platforms the functions do not exist.

#![cfg(windows)]

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

fn blob(d: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: d.len() as u32, pbData: d.as_ptr() as *mut u8 }
}

/// Copy out and free a DPAPI-allocated blob.
///
/// # Safety
/// `out` must have been filled by a successful DPAPI call.
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
    v
}

/// `CryptProtectData`, CurrentUser scope, UI forbidden, with `entropy` as the optional secondary secret.
pub fn protect(data: &[u8], entropy: &[u8]) -> windows::core::Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(entropy));
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, w!("gmail-ro"), Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        Ok(take(out))
    }
}

/// Inverse of [`protect`]; fails if the data was tampered with, the entropy differs, or it was protected by
/// another Windows user.
pub fn unprotect(data: &[u8], entropy: &[u8]) -> windows::core::Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(entropy));
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        Ok(take(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: &[u8] = b"gmail-ro-test-entropy";

    #[test]
    fn roundtrip() {
        let plain = b"synthetic-not-a-real-token";
        let enc = protect(plain, E).unwrap();
        assert_ne!(enc.as_slice(), plain.as_slice());
        assert_eq!(unprotect(&enc, E).unwrap(), plain);
    }

    #[test]
    fn roundtrip_empty() {
        let enc = protect(b"", E).unwrap();
        assert_eq!(unprotect(&enc, E).unwrap(), b"");
    }

    #[test]
    fn tamper_fails() {
        let mut enc = protect(b"x", E).unwrap();
        let last = enc.len() - 1;
        enc[last] ^= 0xff;
        assert!(unprotect(&enc, E).is_err());
    }

    #[test]
    fn wrong_entropy_fails() {
        let enc = protect(b"x", E).unwrap();
        assert!(unprotect(&enc, b"another-entropy").is_err());
    }
}
