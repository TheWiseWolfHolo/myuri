//! "Start with Windows", as a value under the current user's Run key.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};

use crate::win::wide;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("Myuri");

/// The command line Windows runs at sign-in: this executable, quoted.
fn command() -> Option<Vec<u16>> {
    let exe = std::env::current_exe().ok()?;
    Some(wide(&format!("\"{}\"", exe.display())))
}

/// Whether sign-in starts this very executable. A value left behind by a copy
/// in another folder does not count.
pub fn is_enabled() -> bool {
    let Some(expected) = command() else { return false };
    let mut buffer = vec![0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    status == ERROR_SUCCESS && buffer[..(bytes as usize / 2).min(buffer.len())] == expected[..]
}

pub fn set(enabled: bool) -> Result<(), String> {
    let status = unsafe {
        if enabled {
            let command = command().ok_or("cannot find this executable's path")?;
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE)
        }
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(format!("registry error {}", status.0)) }
}
