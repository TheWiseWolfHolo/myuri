//! Provider API keys, kept in the Windows credential store and nowhere else.

use windows::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win::wide;

fn target(provider: &str) -> Vec<u16> {
    wide(&format!("Myuri/provider/{provider}"))
}

/// The key for a provider, or `None` when none is stored.
pub fn read(provider: &str) -> Option<String> {
    let target = target(provider);
    unsafe {
        let mut credential = std::ptr::null_mut();
        CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut credential).ok()?;
        let blob = std::slice::from_raw_parts(
            (*credential).CredentialBlob,
            (*credential).CredentialBlobSize as usize,
        );
        let key = String::from_utf8(blob.to_vec()).ok();
        CredFree(credential.cast());
        key.filter(|key| !key.is_empty())
    }
}

pub fn write(provider: &str, key: &str) -> Result<(), String> {
    let mut target = target(provider);
    let mut user = wide("myuri");
    let credential = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        Comment: PWSTR::null(),
        LastWritten: FILETIME::default(),
        CredentialBlobSize: key.len() as u32,
        CredentialBlob: key.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: PWSTR::null(),
        UserName: PWSTR(user.as_mut_ptr()),
    };
    unsafe { CredWriteW(&credential, 0) }.map_err(|error| error.message())
}

/// Returns whether a key was stored.
pub fn delete(provider: &str) -> Result<bool, String> {
    let target = target(provider);
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(error.message()),
    }
}
