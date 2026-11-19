//! Low-level mouse hook and global hotkey on a dedicated thread.
//!
//! Windows removes a low-level hook whose callback is slow, so the callback
//! only runs the gesture state machine, injects input and posts messages. The
//! hold timer is the timeout of the thread's own wait, which needs no timer
//! objects and cannot leak.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_MOUSE, MOD_NOREPEAT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, RegisterHotKey, SendInput, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetSystemMetrics, HHOOK,
    LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG, MSLLHOOKSTRUCT, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    QS_ALLINPUT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL, WM_APP, WM_HOTKEY,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use myuri_core::gesture::{Effect, Gesture, Outcome, Pos};
use myuri_core::hotkey::Hotkey;

use crate::applog::{debug, info};
use crate::capture::CaptureHandle;
use crate::events::{UiMsg, UiSender};
use crate::win::{self, WindowInfo};

/// Tag on the input this thread injects, for anyone inspecting the stream.
const INJECT_TAG: usize = 0x4D59_5552;
const HOTKEY_ID: i32 = 1;
const WM_WAKE: u32 = WM_APP + 1;
/// How often an idle thread checks that the hook is still installed.
const WATCHDOG_MS: u32 = 5000;

#[derive(Clone, Debug, PartialEq)]
pub struct HookSettings {
    pub enabled: bool,
    pub hold_ms: u64,
    /// At 100% scaling.
    pub tolerance: i32,
    pub skip_fullscreen: bool,
    pub exclude: Vec<String>,
    /// The binding as written, for messages.
    pub hotkey_text: String,
    pub hotkey: Option<Hotkey>,
}

enum Command {
    Settings(HookSettings),
    Quit,
}

pub struct HookHandle {
    thread_id: u32,
    tx: Sender<Command>,
}

impl HookHandle {
    pub fn apply(&self, settings: HookSettings) {
        self.send(Command::Settings(settings));
    }

    pub fn quit(&self) {
        self.send(Command::Quit);
    }

    fn send(&self, command: Command) {
        if self.tx.send(command).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

struct State {
    gesture: Gesture,
    settings: HookSettings,
    paused: Arc<AtomicBool>,
    /// Identifies the current press across the hook, capture and UI threads.
    press: u64,
    /// The focused window when the current press began; capture reads from it.
    target: Option<WindowInfo>,
    processes: Vec<(u32, win::Process)>,
    events: u64,
    ui: UiSender,
    capture: CaptureHandle,
}

/// What the watchdog saw at its last check.
struct Watch {
    at: u64,
    cursor: Pos,
    events: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Run `f` against this thread's state. Returns `None` when the state is busy,
/// which callers treat as "leave the event alone".
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok().and_then(|mut state| state.as_mut().map(f)))
}

pub fn spawn(
    settings: HookSettings,
    paused: Arc<AtomicBool>,
    ui: UiSender,
    capture: CaptureHandle,
) -> HookHandle {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("myuri-hook".into())
        .spawn(move || {
            // Create the message queue before announcing the thread id.
            let mut msg = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE);
                let _ = ready_tx.send(GetCurrentThreadId());
            }
            run(settings, paused, ui, capture, rx);
        })
        .expect("spawn hook thread");
    let thread_id = ready_rx.recv().expect("hook thread start");
    HookHandle { thread_id, tx }
}

fn install() -> Option<HHOOK> {
    unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), HINSTANCE::default(), 0).ok() }
}

fn run(
    settings: HookSettings,
    paused: Arc<AtomicBool>,
    ui: UiSender,
    capture: CaptureHandle,
    rx: Receiver<Command>,
) {
    let Some(mut hook) = install() else {
        ui.send(UiMsg::Fatal("装不上鼠标钩子".into()));
        return;
    };
    register_hotkey(&settings, &ui);
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            gesture: Gesture::new(),
            settings,
            paused,
            press: 0,
            target: None,
            processes: Vec::new(),
            events: 0,
            ui,
            capture,
        });
    });

    let mut watch = Watch { at: win::now_ms(), cursor: win::cursor_pos(), events: 0 };
    'thread: loop {
        let deadline = with_state(|st| st.gesture.deadline()).flatten();
        let timeout = match deadline {
            Some(deadline) => deadline.saturating_sub(win::now_ms()) as u32,
            None => WATCHDOG_MS,
        };
        unsafe {
            MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            // The hook callback runs inside PeekMessageW, so no state borrow
            // may be held across it.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                match msg.message {
                    WM_QUIT => break 'thread,
                    WM_HOTKEY if msg.wParam.0 == HOTKEY_ID as usize => {
                        with_state(State::on_hotkey);
                    }
                    WM_WAKE => {}
                    _ => {
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Quit => break 'thread,
                Command::Settings(new) => {
                    with_state(|st| {
                        if (st.settings.hotkey, &st.settings.hotkey_text)
                            != (new.hotkey, &new.hotkey_text)
                        {
                            unsafe {
                                let _ = UnregisterHotKey(HWND::default(), HOTKEY_ID);
                            }
                            register_hotkey(&new, &st.ui);
                        }
                        st.settings = new;
                    });
                }
            }
        }
        with_state(|st| {
            let now = win::now_ms();
            if st.gesture.deadline().is_some_and(|deadline| now >= deadline) {
                let outcome = st.gesture.tick(win::cursor_pos(), now);
                st.apply(outcome, None);
            }
        });

        // Windows drops a low-level hook without telling anyone when a
        // callback overruns its time limit. If the cursor moved over a whole
        // interval and the callback saw nothing, install a fresh hook.
        let now = win::now_ms();
        if now.saturating_sub(watch.at) >= u64::from(WATCHDOG_MS) {
            let cursor = win::cursor_pos();
            let events = with_state(|st| st.events).unwrap_or(watch.events);
            if cursor != watch.cursor && events == watch.events {
                if let Some(fresh) = install() {
                    unsafe {
                        let _ = UnhookWindowsHookEx(hook);
                    }
                    hook = fresh;
                    debug!("hook: reinstalled after a silent interval");
                }
            }
            watch = Watch { at: now, cursor, events };
        }
    }

    unsafe {
        let _ = UnhookWindowsHookEx(hook);
        let _ = UnregisterHotKey(HWND::default(), HOTKEY_ID);
    }
    STATE.with(|cell| cell.borrow_mut().take());
}

fn register_hotkey(settings: &HookSettings, ui: &UiSender) {
    let Some(hotkey) = settings.hotkey else { return };
    let modifiers = HOT_KEY_MODIFIERS(hotkey.modifiers) | MOD_NOREPEAT;
    if unsafe { RegisterHotKey(HWND::default(), HOTKEY_ID, modifiers, hotkey.vk) }.is_err() {
        info!("hotkey: {} is held by another program", settings.hotkey_text);
        ui.send(UiMsg::HotkeyUnavailable(settings.hotkey_text.clone()));
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if with_state(|st| st.on_mouse(wparam.0 as u32, info)) == Some(true) {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

impl State {
    /// Returns whether to swallow the event.
    fn on_mouse(&mut self, message: u32, info: &MSLLHOOKSTRUCT) -> bool {
        self.events = self.events.wrapping_add(1);
        // Input injected by this or any other program is never part of the
        // gesture.
        if info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0 {
            return false;
        }
        let pos = Pos::new(info.pt.x, info.pt.y);
        let outcome = match message {
            WM_MOUSEMOVE => {
                if self.gesture.is_idle() {
                    return false;
                }
                self.gesture.motion(pos)
            }
            WM_RBUTTONDOWN => {
                let mut tolerance = 0;
                if self.gesture.is_idle() {
                    let Some(target) = self.admit(pos) else { return false };
                    self.press += 1;
                    self.capture.press(self.press, target.clone());
                    self.target = Some(target);
                    tolerance = (self.settings.tolerance as f32 * win::scale_at(pos)).round() as i32;
                }
                self.gesture.down(pos, win::now_ms(), self.settings.hold_ms, tolerance)
            }
            WM_RBUTTONUP => self.gesture.up(pos, win::now_ms()),
            _ => {
                if self.gesture.is_idle() {
                    return false;
                }
                self.gesture.other(pos)
            }
        };
        self.apply(outcome, Some((message, info)));
        outcome.swallow
    }

    /// Decide whether a right-button press may be withheld, and if so which
    /// window the selection will be read from.
    fn admit(&mut self, pos: Pos) -> Option<WindowInfo> {
        if !self.settings.enabled || self.paused.load(Ordering::Relaxed) || win::cursor_hidden() {
            return None;
        }
        let under = win::root_window_at(pos);
        // The panel and the tray menu take right clicks as they are.
        if under.0.is_null() || win::window_pid(under) == unsafe { GetCurrentProcessId() } {
            return None;
        }
        let clicked = self.describe(under)?;
        if self.settings.skip_fullscreen
            && clicked.exe != "explorer.exe"
            && win::is_fullscreen(under)
        {
            return None;
        }
        let foreground = unsafe { GetForegroundWindow() };
        if foreground == under || foreground.0.is_null() {
            return Some(clicked);
        }
        // While the panel is open, a right click elsewhere is an ordinary
        // click: it reaches the application and the panel closes, like a menu.
        if win::window_pid(foreground) == unsafe { GetCurrentProcessId() } {
            return None;
        }
        self.describe(foreground)
    }

    /// Window details, or `None` when the gesture must stay away from it:
    /// excluded by the user, or elevated so a replayed click could not reach it.
    fn describe(&mut self, hwnd: HWND) -> Option<WindowInfo> {
        let pid = win::window_pid(hwnd);
        let index = match self.processes.iter().position(|(known, _)| *known == pid) {
            Some(index) => index,
            None => {
                // Process ids get reused, so the cache stays small and recent.
                if self.processes.len() >= 32 {
                    self.processes.remove(0);
                }
                self.processes.push((pid, win::inspect_process(pid)));
                self.processes.len() - 1
            }
        };
        let process = &self.processes[index].1;
        if process.protected || self.settings.exclude.contains(&process.exe) {
            return None;
        }
        Some(WindowInfo {
            hwnd: hwnd.0 as isize,
            exe: process.exe.clone(),
            class: win::class_name(hwnd),
        })
    }

    fn on_hotkey(&mut self) {
        let foreground = unsafe { GetForegroundWindow() };
        if win::window_pid(foreground) == unsafe { GetCurrentProcessId() } {
            self.ui.send(UiMsg::Dismiss);
            return;
        }
        self.press += 1;
        let pos = win::cursor_pos();
        self.ui.send(UiMsg::Trigger { press: self.press, pos });
        self.capture.resolve(self.press, win::describe_window(foreground));
    }

    fn apply(&mut self, outcome: Outcome, current: Option<(u32, &MSLLHOOKSTRUCT)>) {
        match outcome.effect {
            Effect::None => {}
            Effect::Click { down, up } => {
                self.capture.cancel(self.press);
                inject(&[button(down, true), button(up, false)]);
            }
            Effect::Flush { at, now } => {
                self.capture.cancel(self.press);
                let mut inputs = vec![button(at, true), mouse(now, MOUSE_EVENT_FLAGS(0), 0)];
                // A swallowed wheel or button event goes back in after the
                // press, so the application sees them in the order they
                // happened.
                if outcome.swallow {
                    inputs.extend(current.and_then(|(message, info)| repeat(message, info)));
                }
                inject(&inputs);
            }
            Effect::Trigger { pos, .. } => {
                self.ui.send(UiMsg::Trigger { press: self.press, pos });
                self.capture.resolve(self.press, self.target.take());
            }
            Effect::Release { .. } => self.ui.send(UiMsg::Released { press: self.press }),
        }
    }
}

/// `SendInput` takes physical buttons, which Windows swaps afterwards for
/// left-handed setups, while the hook reports buttons after the swap.
fn swapped() -> bool {
    unsafe { GetSystemMetrics(SM_SWAPBUTTON) != 0 }
}

/// The right button going down or up at `pos`.
fn button(pos: Pos, down: bool) -> INPUT {
    let flags = match (down, swapped()) {
        (true, false) => MOUSEEVENTF_RIGHTDOWN,
        (false, false) => MOUSEEVENTF_RIGHTUP,
        (true, true) => MOUSEEVENTF_LEFTDOWN,
        (false, true) => MOUSEEVENTF_LEFTUP,
    };
    mouse(pos, flags, 0)
}

/// The event the hook just swallowed, as input to send again.
fn repeat(message: u32, info: &MSLLHOOKSTRUCT) -> Option<INPUT> {
    let high = (info.mouseData >> 16) as u16;
    let swap = swapped();
    let (flags, data) = match message {
        WM_LBUTTONDOWN if swap => (MOUSEEVENTF_RIGHTDOWN, 0),
        WM_LBUTTONDOWN => (MOUSEEVENTF_LEFTDOWN, 0),
        WM_LBUTTONUP if swap => (MOUSEEVENTF_RIGHTUP, 0),
        WM_LBUTTONUP => (MOUSEEVENTF_LEFTUP, 0),
        WM_MBUTTONDOWN => (MOUSEEVENTF_MIDDLEDOWN, 0),
        WM_MBUTTONUP => (MOUSEEVENTF_MIDDLEUP, 0),
        WM_XBUTTONDOWN => (MOUSEEVENTF_XDOWN, u32::from(high)),
        WM_XBUTTONUP => (MOUSEEVENTF_XUP, u32::from(high)),
        // The wheel delta is signed; keep the sign when widening.
        WM_MOUSEWHEEL => (MOUSEEVENTF_WHEEL, high as i16 as i32 as u32),
        WM_MOUSEHWHEEL => (MOUSEEVENTF_HWHEEL, high as i16 as i32 as u32),
        _ => return None,
    };
    Some(mouse(Pos::new(info.pt.x, info.pt.y), flags, data))
}

/// Mouse input at an absolute position on the virtual desktop.
fn mouse(pos: Pos, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN) as i64,
            GetSystemMetrics(SM_YVIRTUALSCREEN) as i64,
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as i64,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as i64,
        )
    };
    // Windows maps the 0..65535 range back with a truncating multiply, so
    // round up to land on the exact pixel.
    let normalize = |value: i64, origin: i64, extent: i64| {
        (((value - origin) * 65536 + extent - 1) / extent).clamp(0, 65535) as i32
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: normalize(pos.x as i64, left, width),
                dy: normalize(pos.y as i64, top, height),
                mouseData: data,
                dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn inject(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, size_of::<INPUT>() as i32);
    }
}
