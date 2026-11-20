//! Tray icon with its menu and notifications, on its own thread. The same
//! hidden window receives commands from `myuri preview` and `myuri quit`.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowThreadProcessId, HICON, HMENU, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
    SW_SHOWNORMAL, SetForegroundWindow, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCSTR, PCWSTR, w};

use myuri_core::config::ContentKind;
use myuri_core::gesture::Pos;

use crate::applog::{self, info};
use crate::events::{UiMsg, UiSender};
use crate::win::wide;
use crate::{autostart, config_dir};

const WINDOW_CLASS: PCWSTR = w!("MyuriControl");
const ICON_ID: u32 = 1;
/// The icon was activated from the keyboard.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// From the tray icon.
const WM_TRAY: u32 = WM_APP + 1;
/// A queued `Command` is waiting.
const WM_COMMAND_QUEUED: u32 = WM_APP + 2;
/// From another `myuri` process: `wparam` is the request, `lparam` its argument.
const WM_CONTROL: u32 = WM_APP + 10;
const CONTROL_PREVIEW: usize = 1;
const CONTROL_QUIT: usize = 2;
const CONTROL_SETTINGS: usize = 3;
const CONTROL_MENU: usize = 4;

pub const MENU_PAUSE: usize = 1;
pub const MENU_AUTOSTART: usize = 2;
pub const MENU_CONFIG: usize = 3;
pub const MENU_LOG: usize = 4;
pub const MENU_QUIT: usize = 5;
pub const MENU_SETTINGS: usize = 6;
pub const MENU_HISTORY: usize = 7;

enum Command {
    Notify(String),
    /// An entry of the menu was chosen.
    Menu(usize),
}

#[derive(Clone)]
pub struct TrayHandle {
    window: isize,
    tx: Sender<Command>,
    paused: Arc<AtomicBool>,
}

impl TrayHandle {
    /// Show a notification from the tray icon.
    pub fn notify(&self, text: impl Into<String>) {
        if self.tx.send(Command::Notify(text.into())).is_ok() {
            self.post(WM_COMMAND_QUEUED);
        }
    }

    pub fn quit(&self) {
        self.post(WM_CLOSE);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Carry out an entry of the menu.
    pub fn choose(&self, entry: usize) {
        if self.tx.send(Command::Menu(entry)).is_ok() {
            self.post(WM_COMMAND_QUEUED);
        }
    }

    fn post(&self, message: u32) {
        unsafe {
            let _ = PostMessageW(HWND(self.window as _), message, WPARAM(0), LPARAM(0));
        }
    }
}

struct Tray {
    window: HWND,
    paused: Arc<AtomicBool>,
    ui: UiSender,
    rx: Receiver<Command>,
    icon: HICON,
    icon_paused: HICON,
    taskbar_created: u32,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

pub fn spawn(paused: Arc<AtomicBool>, ui: UiSender) -> TrayHandle {
    let handle_paused = paused.clone();
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("myuri-tray".into())
        .spawn(move || unsafe {
            follow_system_theme_in_menus();
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                lpszClassName: WINDOW_CLASS,
                ..Default::default()
            };
            RegisterClassW(&class);
            // A hidden top-level window rather than a message-only one: only
            // top-level windows hear that the taskbar was recreated.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                WINDOW_CLASS,
                w!("Myuri"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                HWND::default(),
                HMENU::default(),
                HINSTANCE::default(),
                None,
            )
            .unwrap_or_default();
            let size = GetSystemMetrics(SM_CXSMICON).max(16);
            let tray = Tray {
                window,
                paused,
                ui,
                rx,
                icon: load_icon(1, size),
                icon_paused: load_icon(2, size),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            };
            tray.add_icon();
            TRAY.with(|cell| *cell.borrow_mut() = Some(tray));
            let _ = ready_tx.send(window.0 as isize);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
            if let Some(tray) = TRAY.with(|cell| cell.borrow_mut().take()) {
                let _ = DestroyIcon(tray.icon);
                let _ = DestroyIcon(tray.icon_paused);
            }
        })
        .expect("spawn tray thread");
    TrayHandle { window: ready_rx.recv().expect("tray thread start"), tx, paused: handle_paused }
}

/// Ask a running instance to do something. Returns false when none is running.
pub fn send_to_running_instance(request: Request) -> bool {
    let (wparam, lparam) = match request {
        Request::Quit => (CONTROL_QUIT, 0),
        Request::Settings => (CONTROL_SETTINGS, 0),
        Request::Menu => (CONTROL_MENU, 0),
        Request::Preview(kind) => (
            CONTROL_PREVIEW,
            match kind {
                None => 0,
                Some(ContentKind::Text) => 1,
                Some(ContentKind::Image) => 2,
                Some(ContentKind::Files) => 3,
            },
        ),
    };
    unsafe {
        match FindWindowW(WINDOW_CLASS, PCWSTR::null()) {
            Ok(window) if !window.0.is_null() => {
                if wparam == CONTROL_SETTINGS {
                    let mut process = 0;
                    GetWindowThreadProcessId(window, Some(&mut process));
                    if process != 0 {
                        let _ = AllowSetForegroundWindow(process);
                    }
                }
                PostMessageW(window, WM_CONTROL, WPARAM(wparam), LPARAM(lparam)).is_ok()
            }
            _ => false,
        }
    }
}

pub enum Request {
    Preview(Option<ContentKind>),
    Settings,
    /// Show the tray menu at the pointer (`myuri preview menu`).
    Menu,
    Quit,
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The menu runs its own message loop inside `show_menu`, so the state
    // must not stay borrowed while it is open.
    let handled = TRAY.with(|cell| {
        let Ok(guard) = cell.try_borrow() else { return false };
        let Some(tray) = guard.as_ref() else { return false };
        match message {
            WM_TRAY => {
                let event = (lparam.0 & 0xFFFF) as u32;
                // A click opens the settings; the right button, the menu.
                if matches!(event, NIN_SELECT | NIN_KEYSELECT) {
                    unsafe {
                        let _ = SetForegroundWindow(hwnd);
                    }
                    tray.ui.send(UiMsg::Settings);
                } else if event == WM_CONTEXTMENU {
                    let mut cursor = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut cursor);
                        // Lets the menu window take the keyboard.
                        let _ = SetForegroundWindow(hwnd);
                    }
                    tray.ui.send(UiMsg::TrayMenu(Pos { x: cursor.x, y: cursor.y }));
                }
                true
            }
            WM_COMMAND_QUEUED => {
                while let Ok(command) = tray.rx.try_recv() {
                    match command {
                        Command::Notify(text) => tray.notify(&text),
                        Command::Menu(entry) => {
                            if let Some(notice) = on_menu(entry, &tray.paused, &tray.ui) {
                                tray.notify(&notice);
                            }
                            tray.refresh_icon();
                        }
                    }
                }
                true
            }
            WM_CONTROL => {
                match wparam.0 {
                    CONTROL_QUIT => tray.ui.send(UiMsg::Quit),
                    CONTROL_SETTINGS => tray.ui.send(UiMsg::Settings),
                    CONTROL_MENU => {
                        let mut cursor = POINT::default();
                        unsafe {
                            let _ = GetCursorPos(&mut cursor);
                        }
                        tray.ui.send(UiMsg::TrayMenu(Pos { x: cursor.x, y: cursor.y }));
                    }
                    CONTROL_PREVIEW => tray.ui.send(UiMsg::Preview(match lparam.0 {
                        1 => Some(ContentKind::Text),
                        2 => Some(ContentKind::Image),
                        3 => Some(ContentKind::Files),
                        _ => None,
                    })),
                    _ => {}
                }
                true
            }
            WM_CLOSE => {
                tray.remove_icon();
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                true
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                true
            }
            // Explorer restarted and forgot every icon.
            _ if message == tray.taskbar_created => {
                tray.add_icon();
                true
            }
            _ => false,
        }
    });
    if handled { LRESULT(0) } else { unsafe { DefWindowProcW(hwnd, message, wparam, lparam) } }
}

impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        let paused = self.paused.load(Ordering::Relaxed);
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: if paused { self.icon_paused } else { self.icon },
            ..Default::default()
        };
        copy_text(&mut data.szTip, if paused { "Myuri（右键手势已暂停）" } else { "Myuri" });
        data
    }

    fn add_icon(&self) {
        let mut data = self.data();
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                info!("tray: could not add the icon");
            }
            // Version 4 reports clicks with screen coordinates and keyboard use.
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    fn refresh_icon(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data());
        }
    }

    fn remove_icon(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
        }
    }

    fn notify(&self, text: &str) {
        let mut data = self.data();
        data.uFlags |= NIF_INFO;
        data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        data.hBalloonIcon = self.icon;
        copy_text(&mut data.szInfoTitle, "Myuri");
        copy_text(&mut data.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
}

/// Copy into a fixed UTF-16 buffer, always leaving a terminator.
fn copy_text<const N: usize>(buffer: &mut [u16; N], text: &str) {
    for (slot, unit) in buffer.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *slot = unit;
    }
}

fn on_menu(choice: usize, paused: &AtomicBool, ui: &UiSender) -> Option<String> {
    match choice {
        MENU_PAUSE => {
            let now = !paused.fetch_xor(true, Ordering::Relaxed);
            info!("gesture: {}", if now { "paused" } else { "resumed" });
        }
        MENU_AUTOSTART => {
            let enable = !autostart::is_enabled();
            if let Err(error) = autostart::set(enable) {
                info!("autostart: {error}");
                return Some(format!("开机自启设置失败：{error}"));
            }
        }
        MENU_CONFIG => open(&config_dir().to_string_lossy()),
        MENU_LOG => open(&applog::path().to_string_lossy()),
        MENU_SETTINGS => ui.send(UiMsg::Settings),
        MENU_HISTORY => ui.send(UiMsg::Page("history")),
        MENU_QUIT => ui.send(UiMsg::Quit),
        _ => {}
    }
    None
}

fn open(path: &str) {
    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(wide(path).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Popup menus stay light in dark mode unless the process opts in through two
/// unnamed uxtheme exports, as Explorer and most tray programs do.
unsafe fn follow_system_theme_in_menus() {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(set_preferred_app_mode) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(set_preferred_app_mode);
            set(1);
        }
        if let Some(flush_menu_themes) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush_menu_themes);
            flush();
        }
    }
}

/// An icon embedded in the executable (`assets/myuri.rc`) at the tray's
/// size: 1 is the mark, 2 its gray version shown while paused.
fn load_icon(resource: usize, size: i32) -> HICON {
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(HINSTANCE(module.0), PCWSTR(resource as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
            .map(|handle| HICON(handle.0))
            .unwrap_or_default()
    }
}
