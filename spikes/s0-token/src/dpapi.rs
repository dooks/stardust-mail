//! DPAPI wrapper (copied from icloud-notes' `icloud-session`, different entropy).

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

// Extra entropy so another app running as the same user can't just call CryptUnprotectData blindly.
const ENTROPY: &[u8] = b"starmail-spikes-token";

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

/// `CryptProtectData`, CurrentUser scope, `CRYPTPROTECT_UI_FORBIDDEN`, with the app's entropy.
pub fn protect(data: &[u8]) -> windows::core::Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(ENTROPY));
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, w!("starmail spikes token"), Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        Ok(take(out))
    }
}

/// Inverse of [`protect`].
pub fn unprotect(data: &[u8]) -> windows::core::Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(ENTROPY));
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
        Ok(take(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let plain = b"synthetic-not-a-real-token";
        let enc = protect(plain).unwrap();
        assert_ne!(enc.as_slice(), plain.as_slice());
        assert_eq!(unprotect(&enc).unwrap(), plain);
    }

    #[test]
    fn tamper_fails() {
        let mut enc = protect(b"x").unwrap();
        let last = enc.len() - 1;
        enc[last] ^= 0xff;
        assert!(unprotect(&enc).is_err());
    }
}
