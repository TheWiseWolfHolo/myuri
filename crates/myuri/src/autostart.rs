//! "Start with Windows", as a value under the current user's Run key.
//! Also checks and clears `StartupApproved\Run`, where Windows records when
//! the user disabled an entry in Task Manager or Settings.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_BINARY, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW,
    RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};

use crate::win::wide;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const APPROVED_KEY: PCWSTR =
    w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
const VALUE: PCWSTR = w!("Myuri");

/// Where "Start with Windows" stands for this executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Sign-in runs this executable and Windows has not disabled it.
    Enabled,
    /// Registered for this executable, but disabled in Windows Startup settings.
    BlockedByWindows,
    /// Registered for a copy in another folder.
    OtherPath(String),
    /// Not registered.
    Disabled,
}

/// The command line Windows runs at sign-in: this executable, quoted.
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

/// Read the `Myuri` string from `Run`, if present.
fn read_run_value() -> Option<String> {
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
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (bytes as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..units]);
    Some(text.trim_end_matches('\0').to_string())
}

/// Read the `StartupApproved\Run` binary flag for `Myuri`, if present.
fn is_blocked_by_windows() -> bool {
    let mut buffer = [0u8; 32];
    let mut bytes = buffer.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            APPROVED_KEY,
            VALUE,
            RRF_RT_REG_BINARY,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    status == ERROR_SUCCESS && approved_disabled(&buffer[..(bytes as usize).min(buffer.len())])
}

/// In `StartupApproved\Run`, a leading byte of `0x02` or `0x06` means enabled;
/// odd values such as `0x03` or `0x07` mean disabled by the user or policy.
fn approved_disabled(bytes: &[u8]) -> bool {
    bytes.first().is_some_and(|&flag| flag != 0 && !matches!(flag, 0x02 | 0x06))
}

/// Compare two Windows startup command paths ignoring quotes, `\\?\` prefix and case.
fn same_command(stored: &str, expected: &str) -> bool {
    let normalize = |raw: &str| {
        let trimmed = raw.trim().trim_matches('"');
        trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed).to_lowercase()
    };
    let (a, b) = (normalize(stored), normalize(expected));
    !a.is_empty() && a == b
}

pub fn status() -> Status {
    let Some(stored) = read_run_value() else {
        return Status::Disabled;
    };
    let Some(expected) = command() else {
        return Status::Disabled;
    };
    if !same_command(&stored, &expected) {
        return Status::OtherPath(stored.trim().trim_matches('"').to_string());
    }
    if is_blocked_by_windows() {
        return Status::BlockedByWindows;
    }
    Status::Enabled
}

/// Whether sign-in starts this very executable. A value left behind by a copy
/// in another folder, or one disabled in Task Manager, does not count.
pub fn is_enabled() -> bool {
    status() == Status::Enabled
}

pub fn set(enabled: bool) -> Result<(), String> {
    if enabled {
        let cmd = wide(&command().ok_or("找不到当前程序路径")?);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr().cast()),
                (cmd.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("写注册表失败（错误码 {}）", status.0));
        }
        // Clear any Task Manager disable flag; if deleting fails, overwrite with enabled.
        let cleared = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE) };
        if cleared != ERROR_SUCCESS
            && cleared != ERROR_FILE_NOT_FOUND
            && cleared != ERROR_PATH_NOT_FOUND
        {
            let approved: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let _ = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    APPROVED_KEY,
                    VALUE,
                    REG_BINARY.0,
                    Some(approved.as_ptr().cast()),
                    approved.len() as u32,
                )
            };
        }
        Ok(())
    } else {
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
        if status != ERROR_SUCCESS
            && status != ERROR_FILE_NOT_FOUND
            && status != ERROR_PATH_NOT_FOUND
        {
            return Err(format!("删注册表失败（错误码 {}）", status.0));
        }
        let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_comparison_ignores_case_and_quotes() {
        assert!(same_command(r#""Q:\Apps\Sample Folder\myuri.exe""#, r#""q:\apps\sample folder\MYURI.exe""#));
        assert!(same_command(r"Q:\Apps\Sample Folder\myuri.exe", r#""\\?\Q:\Apps\Sample Folder\myuri.exe""#));
        assert!(!same_command(r#""Q:\Apps\Old\myuri.exe""#, r#""Q:\Apps\Sample Folder\myuri.exe""#));
        assert!(!same_command("", r#""Q:\Apps\Sample Folder\myuri.exe""#));
    }

    #[test]
    fn startup_approved_flags_are_read() {
        assert!(!approved_disabled(&[]));
        assert!(!approved_disabled(&[0x02, 0, 0, 0]));
        assert!(!approved_disabled(&[0x06, 0, 0, 0]));
        assert!(approved_disabled(&[0x03, 0, 0, 0]));
        assert!(approved_disabled(&[0x07, 0, 0, 0]));
    }
}
