//! The Win32 Credential Manager calls of [`crate::credentials`] (T7.4g): a generic credential
//! per target, local-machine persistence (this Windows account on this computer, never
//! roaming). Every `unsafe` call has its SAFETY comment; buffers that held the secret are
//! wiped before they are freed.

use std::sync::atomic::{Ordering, compiler_fence};

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Overwrites `bytes` with zeros in a way the compiler keeps.
fn zero(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to one byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Zeroes and drops a buffer that held the secret.
pub fn wipe(mut bytes: Vec<u8>) {
    zero(&mut bytes);
}

fn not_found(e: &windows::core::Error) -> bool {
    e.code() == ERROR_NOT_FOUND.to_hresult()
}

/// Stores `secret` as the generic credential `target` with user name `user`, replacing an
/// earlier one.
///
/// # Errors
/// Credential Manager refuses it.
pub fn write(target: &str, user: &str, secret: &[u8]) -> windows::core::Result<()> {
    let mut target_w = wide(target);
    let mut user_w = wide(user);
    // CREDENTIALW takes a `*mut u8`; give it a copy this frame owns and wipes.
    let mut blob = secret.to_vec();
    let size = u32::try_from(blob.len()).map_err(|_| windows::core::Error::empty())?;
    let credential = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        CredentialBlobSize: size,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user_w.as_mut_ptr()),
        ..CREDENTIALW::default()
    };
    // SAFETY: `credential` and every buffer it points to (`target_w`, `user_w`, `blob`, each
    // NUL-terminated where Win32 expects a string, `blob` exactly `size` bytes) are owned by
    // this frame and live for the whole call; the other pointers are null (zeroed default)
    // with `AttributeCount` 0. CredWriteW only reads them.
    let result = unsafe { CredWriteW(&raw const credential, 0) };
    zero(&mut blob);
    result
}

/// Frees a credential returned by `CredReadW` (wiping its blob first).
struct Returned(*mut CREDENTIALW);

impl Drop for Returned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful CredReadW and is freed exactly once,
        // here; nothing borrows it any more when the guard drops.
        unsafe { CredFree(self.0.cast_const().cast()) };
    }
}

/// The blob of the generic credential `target`, `None` when there is none. The returned
/// buffer holds the secret: pass it to [`wipe`] when done.
///
/// # Errors
/// Credential Manager fails (anything but "not found").
pub fn read(target: &str) -> windows::core::Result<Option<Vec<u8>>> {
    let target_w = wide(target);
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `target_w` is a NUL-terminated UTF-16 string alive for the call; `ptr` is a
    // valid out-pointer that CredReadW sets to a buffer it allocates on success.
    match unsafe {
        CredReadW(
            PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &raw mut ptr,
        )
    } {
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
        Ok(()) if ptr.is_null() => return Ok(None),
        Ok(()) => {}
    }
    let guard = Returned(ptr);
    // SAFETY: on success `ptr` points to a CREDENTIALW that stays valid until CredFree
    // (the guard's drop, after the last use below).
    let credential = unsafe { &*guard.0 };
    let size = usize::try_from(credential.CredentialBlobSize).unwrap_or(0);
    if credential.CredentialBlob.is_null() || size == 0 {
        return Ok(Some(Vec::new()));
    }
    // SAFETY: Credential Manager returns `CredentialBlobSize` bytes at `CredentialBlob`,
    // owned by the returned buffer (alive until the guard drops); no other reference to
    // them exists, so a mutable slice is sound for wiping them after the copy.
    let blob = unsafe { std::slice::from_raw_parts_mut(credential.CredentialBlob, size) };
    let copy = blob.to_vec();
    zero(blob);
    drop(guard);
    Ok(Some(copy))
}

/// Deletes the generic credential `target`; `false` when there was none.
///
/// # Errors
/// Credential Manager fails (anything but "not found").
pub fn delete(target: &str) -> windows::core::Result<bool> {
    let target_w = wide(target);
    // SAFETY: `target_w` is a NUL-terminated UTF-16 string alive for the call.
    match unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(true),
        Err(e) if not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}
