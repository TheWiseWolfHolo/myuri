//! Small Win32 helpers shared by the hook, capture and UI threads.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetCursorInfo, GetCursorPos, GetWindowRect,
    GetWindowThreadProcessId, IsZoomed, WindowFromPoint, CURSORINFO, GA_ROOT,
};
use windows::core::{PCWSTR, PWSTR};

use myuri_core::gesture::Pos;

/// Open a file with its program, like double clicking it.
pub fn shell_open(path: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::w;
    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Show a file selected in Explorer.
pub fn shell_select(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

/// Put the file's picture on the clipboard.
pub fn shell_copy_image(hwnd: isize, path: &std::path::Path) {
    if let Ok(bytes) = std::fs::read(path) {
        crate::clipboard::write_image(HWND(hwnd as *mut std::ffi::c_void), &bytes);
    }
}

/// Milliseconds on a monotonic clock that starts with the process.
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// NUL-terminated UTF-16.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn cursor_pos() -> Pos {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    Pos::new(point.x, point.y)
}

/// A top-level window and the process behind it. `hwnd` is kept as an integer
/// so the value can cross threads.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    /// Lowercase file name such as `notepad.exe`; empty when unknown.
    pub exe: String,
    pub class: String,
}

impl WindowInfo {
    pub fn handle(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }
}

pub struct Process {
    pub exe: String,
    /// Runs at a higher integrity level than this process (elevated), or could
    /// not be inspected at all. Input injected by this process never reaches
    /// such a window.
    pub protected: bool,
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

pub fn root_window_at(pos: Pos) -> HWND {
    unsafe {
        let hwnd = WindowFromPoint(POINT { x: pos.x, y: pos.y });
        if hwnd.0.is_null() { hwnd } else { GetAncestor(hwnd, GA_ROOT) }
    }
}

pub fn inspect_process(pid: u32) -> Process {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return Process { exe: String::new(), protected: true };
        };
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let exe = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .ok()
        .map(|_| String::from_utf16_lossy(&buffer[..len as usize]))
        .and_then(|path| path.rsplit('\\').next().map(str::to_lowercase))
        .unwrap_or_default();
        let protected = integrity_level(handle).is_none_or(|level| level > own_integrity());
        let _ = CloseHandle(handle);
        Process { exe, protected }
    }
}

pub fn describe_window(hwnd: HWND) -> Option<WindowInfo> {
    if hwnd.0.is_null() {
        return None;
    }
    Some(WindowInfo {
        hwnd: hwnd.0 as isize,
        exe: inspect_process(window_pid(hwnd)).exe,
        class: class_name(hwnd),
    })
}

fn own_integrity() -> u32 {
    static LEVEL: OnceLock<u32> = OnceLock::new();
    // Medium integrity is the fallback: it errs towards leaving windows alone.
    *LEVEL.get_or_init(|| unsafe { integrity_level(GetCurrentProcess()).unwrap_or(0x2000) })
}

unsafe fn integrity_level(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        // u64 storage keeps the pointer inside TOKEN_MANDATORY_LABEL aligned.
        let mut buffer = [0u64; 16];
        let mut len = 0;
        let result = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        result.ok()?;
        let sid = (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        (count > 0).then(|| *GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

/// A window that spans its whole monitor without being maximized: a game, a
/// video or a presentation. Maximized windows also span the monitor when the
/// taskbar auto-hides, which is why `IsZoomed` matters.
pub fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe {
        if IsZoomed(hwnd).as_bool() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let monitor = monitor_rect(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST));
        monitor.is_some_and(|m| {
            rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
        })
    }
}

fn monitor_rect(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe { GetMonitorInfoW(monitor, &mut info).as_bool().then_some(info.rcMonitor) }
}

/// Work area (monitor minus taskbar) of the monitor nearest to `pos`.
pub fn work_area_at(pos: Pos) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: pos.x, y: pos.y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(monitor, &mut info).as_bool().then_some(info.rcWork)
    }
}

/// Games that take over the mouse hide the cursor; nothing can be pointed at.
pub fn cursor_hidden() -> bool {
    let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
    unsafe { GetCursorInfo(&mut info).is_ok() && info.flags.0 == 0 }
}

/// Scale factor of the monitor at `pos`: 1.0 at 100%, 1.25 at 125%.
pub fn scale_at(pos: Pos) -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: pos.x, y: pos.y }, MONITOR_DEFAULTTONEAREST);
        let (mut x, mut y) = (96, 96);
        match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) {
            Ok(()) => x as f32 / 96.0,
            Err(_) => 1.0,
        }
    }
}

/// A window that exists only to receive messages on the calling thread.
pub fn message_window(
    class: windows::core::PCWSTR,
    procedure: unsafe extern "system" fn(
        HWND,
        u32,
        windows::Win32::Foundation::WPARAM,
        windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT,
) -> HWND {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, HMENU, HWND_MESSAGE, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
        WNDCLASSW,
    };
    unsafe {
        let info =
            WNDCLASSW { lpfnWndProc: Some(procedure), lpszClassName: class, ..Default::default() };
        RegisterClassW(&info);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            class,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            HMENU::default(),
            HINSTANCE::default(),
            None,
        )
        .unwrap_or_default()
    }
}
