//! Provider API keys, kept in the Windows credential store and nowhere else.
//! Chat, image, and speech providers each have their own credential namespace
//! (`provider`, `image`, `speech`) so deleting or editing one never affects another.

use windows::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win::wide;

pub use myuri_core::config::{NS_CHAT, NS_IMAGE, NS_SPEECH};

fn target(namespace: &str, provider: &str) -> Vec<u16> {
    wide(&format!("Myuri/{namespace}/{provider}"))
}

/// The key for a chat provider, or `None` when none is stored.
pub fn read(provider: &str) -> Option<String> {
    read_in(NS_CHAT, provider)
}

pub fn write(provider: &str, key: &str) -> Result<(), String> {
    write_in(NS_CHAT, provider, key)
}

/// Read a secret from the given namespace (`provider`, `image`, or `speech`).
pub fn read_in(namespace: &str, provider: &str) -> Option<String> {
    let target = target(namespace, provider);
    unsafe {
        let mut credential = std::ptr::null_mut();
        CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut credential).ok()?;
        let size = (*credential).CredentialBlobSize as usize;
        let ptr = (*credential).CredentialBlob;
        let key = if size == 0 || ptr.is_null() {
            None
        } else {
            let blob = std::slice::from_raw_parts(ptr, size);
            String::from_utf8(blob.to_vec()).ok()
        };
        CredFree(credential.cast());
        key.filter(|key| !key.is_empty())
    }
}

pub fn write_in(namespace: &str, provider: &str, key: &str) -> Result<(), String> {
    let mut target = target(namespace, provider);
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

pub fn delete_in(namespace: &str, provider: &str) -> Result<bool, String> {
    let target = target(namespace, provider);
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(error.message()),
    }
}
