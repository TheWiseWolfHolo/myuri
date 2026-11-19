//! Capture worker: works out what the user has selected when the panel is
//! asked for.
//!
//! The order is chosen to disturb the system as little as possible:
//!
//! 1. UI Automation reads the selection outright, where the program supports
//!    it and asking is free. Nothing is typed and the clipboard is untouched.
//! 2. File Explorer and the desktop are asked for their selected files.
//! 3. Otherwise a copy shortcut is sent and the clipboard read, then put back.
//!    Ctrl+C is never sent to a terminal, where it would interrupt a running
//!    command.
//! 4. With nothing selected, something copied in the last few seconds stands in.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, CountClipboardFormats};
use windows::Win32::System::Threading::{GetCurrentThreadId, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, GA_PARENT, GUITHREADINFO, GetAncestor, GetGUIThreadInfo,
    GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT, WM_APP, WM_CLIPBOARDUPDATE,
};
use windows::core::w;

use myuri_core::config::{self, ContentKind};

use crate::applog::{debug, info};
use crate::clipboard;
use crate::events::{UiMsg, UiSender};
use crate::keys::{self, Chord};
use crate::shell;
use crate::uia::{Selection, Uia};
use crate::win::{self, WindowInfo};

const WM_WAKE: u32 = WM_APP + 1;
/// A press must last this long before its selection is read ahead of the
/// panel. Ordinary clicks are shorter, so they cost nothing.
const READ_AHEAD_MS: u64 = 120;

pub enum Content {
    Text(String),
    Image(Image),
    Files(Vec<PathBuf>),
}

impl Content {
    pub fn kind(&self) -> ContentKind {
        match self {
            Content::Text(_) => ContentKind::Text,
            Content::Image(_) => ContentKind::Image,
            Content::Files(_) => ContentKind::Files,
        }
    }
}

pub struct Image {
    pub format: ImageFormat,
    pub bytes: Vec<u8>,
}

impl Image {
    /// Models take PNG, not the bare bitmap the clipboard holds. A bitmap that
    /// cannot be converted is kept as it is.
    pub fn into_png(self) -> Self {
        match self.format {
            ImageFormat::Png => self,
            ImageFormat::Dib => match dib_to_png(&self.bytes) {
                Some(bytes) => Self { format: ImageFormat::Png, bytes },
                None => self,
            },
        }
    }

    /// Width and height in pixels, read from the header.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let bytes = &self.bytes;
        let field = |at: usize| -> Option<[u8; 4]> { bytes.get(at..at + 4)?.try_into().ok() };
        match self.format {
            // Signature, chunk length, "IHDR", then big-endian width and height.
            ImageFormat::Png if bytes.get(12..16) == Some(b"IHDR") => {
                Some((u32::from_be_bytes(field(16)?), u32::from_be_bytes(field(20)?)))
            }
            ImageFormat::Png => None,
            // Header size, then width and height; a negative height means the
            // rows run top to bottom.
            ImageFormat::Dib => Some((
                i32::from_le_bytes(field(4)?).unsigned_abs(),
                i32::from_le_bytes(field(8)?).unsigned_abs(),
            )),
        }
    }
}

fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::bmp::BmpDecoder;
    use image::{DynamicImage, imageops::FilterType};

    /// Longest side sent to a model; larger pictures are scaled down.
    const LONGEST: u32 = 2400;
    let decoder = BmpDecoder::new_without_file_header(std::io::Cursor::new(dib)).ok()?;
    let mut pixels = DynamicImage::from_decoder(decoder).ok()?.into_rgba8();
    // Screenshots often carry an alpha channel that is zero everywhere.
    if pixels.pixels().all(|pixel| pixel[3] == 0) {
        pixels.pixels_mut().for_each(|pixel| pixel[3] = 255);
    }
    let mut picture = DynamicImage::ImageRgba8(pixels);
    if picture.width().max(picture.height()) > LONGEST {
        picture = picture.resize(LONGEST, LONGEST, FilterType::Triangle);
    }
    let mut png = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    /// A device-independent bitmap as stored on the clipboard: header and
    /// pixels, without the file header a `.bmp` starts with.
    Dib,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Selection,
    /// Nothing was selected; this was copied a moment ago.
    Clipboard,
}

#[derive(Default)]
pub struct Captured {
    pub content: Option<(Content, Origin)>,
}

impl Captured {
    /// For the log: kind and size, never the content itself.
    pub fn describe(&self) -> String {
        let Some((content, origin)) = &self.content else { return "nothing".into() };
        let what = match content {
            Content::Text(text) => format!("text, {} chars", text.chars().count()),
            Content::Image(image) => format!("image, {} KB", image.bytes.len() / 1024),
            Content::Files(files) => format!("{} files", files.len()),
        };
        match origin {
            Origin::Selection => what,
            Origin::Clipboard => format!("{what}, from the clipboard"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaptureSettings {
    pub wait: Duration,
    /// Zero turns the stand-in off.
    pub recent: Duration,
    pub ctrl_c_fallback: bool,
    pub never_ctrl_c: Vec<String>,
}

impl From<&config::CaptureSettings> for CaptureSettings {
    fn from(settings: &config::CaptureSettings) -> Self {
        Self {
            wait: Duration::from_millis(settings.clipboard_wait_ms),
            recent: Duration::from_secs(settings.clipboard_recent_secs),
            ctrl_c_fallback: settings.ctrl_c_fallback,
            never_ctrl_c: settings.never_ctrl_c.clone(),
        }
    }
}

enum Msg {
    Press { press: u64, target: WindowInfo },
    Cancel { press: u64 },
    Resolve { press: u64, target: Option<WindowInfo> },
    Settings(CaptureSettings),
    Quit,
}

#[derive(Clone)]
pub struct CaptureHandle {
    tx: Sender<Msg>,
    thread_id: u32,
}

impl CaptureHandle {
    /// The right button went down over `target`. If the press lasts, its
    /// selection is read before the panel is asked for.
    pub fn press(&self, press: u64, target: WindowInfo) {
        self.send(Msg::Press { press, target });
    }

    /// The press turned out to be a click or a drag.
    pub fn cancel(&self, press: u64) {
        self.send(Msg::Cancel { press });
    }

    /// The panel was asked for. Answers with `UiMsg::Captured`.
    pub fn resolve(&self, press: u64, target: Option<WindowInfo>) {
        self.send(Msg::Resolve { press, target });
    }

    pub fn apply(&self, settings: CaptureSettings) {
        self.send(Msg::Settings(settings));
    }

    pub fn quit(&self) {
        self.send(Msg::Quit);
    }

    fn send(&self, message: Msg) {
        if self.tx.send(message).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

pub fn spawn(settings: CaptureSettings, ui: UiSender) -> CaptureHandle {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("myuri-capture".into())
        .spawn(move || unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE);
            let _ = ready_tx.send(GetCurrentThreadId());
            // UI Automation and the shell objects want a single-threaded
            // apartment, which this thread serves by pumping messages.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let window = win::message_window(w!("MyuriCapture"), window_proc);
            OWN_SEQUENCE.store(clipboard::sequence(), Ordering::Relaxed);
            if AddClipboardFormatListener(window).is_err() {
                info!("capture: cannot watch the clipboard; recent copies will not be offered");
            }
            Worker { settings, ui, window, uia: None, waiting: None, ahead: None }.run(rx);
        })
        .expect("spawn capture thread");
    CaptureHandle { tx, thread_id: ready_rx.recv().expect("capture thread start") }
}

/// When another program last changed the clipboard, on the `now_ms` clock.
static CHANGED_AT: AtomicU64 = AtomicU64::new(u64::MAX);
/// The clipboard sequence number right after this program last wrote to it.
static OWN_SEQUENCE: AtomicU32 = AtomicU32::new(0);
/// Set while this program is copying and restoring.
static BUSY: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        if !BUSY.load(Ordering::Relaxed) && clipboard::sequence() != OWN_SEQUENCE.load(Ordering::Relaxed) {
            CHANGED_AT.store(win::now_ms(), Ordering::Relaxed);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            if msg.message != WM_WAKE {
                DispatchMessageW(&msg);
            }
        }
    }
}

fn wait(timeout_ms: u32) {
    unsafe {
        MsgWaitForMultipleObjectsEx(None, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
    }
    pump();
}

struct Worker {
    settings: CaptureSettings,
    ui: UiSender,
    /// Owns the clipboard while restoring and hears about its changes.
    window: HWND,
    uia: Option<Uia>,
    /// A press under way: its id, where the focus is, and when to read ahead.
    waiting: Option<(u64, WindowInfo, u64)>,
    /// The selection read ahead for a press.
    ahead: Option<(u64, Selection)>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let timeout = match &self.waiting {
                Some((_, _, due)) => due.saturating_sub(win::now_ms()) as u32,
                None => INFINITE,
            };
            wait(timeout);
            while let Ok(message) = rx.try_recv() {
                match message {
                    Msg::Press { press, target } => {
                        self.ahead = None;
                        self.waiting = Some((press, target, win::now_ms() + READ_AHEAD_MS));
                    }
                    Msg::Cancel { press } => {
                        self.waiting.take_if(|(waiting, ..)| *waiting == press);
                        self.ahead.take_if(|(ahead, _)| *ahead == press);
                    }
                    Msg::Resolve { press, target } => self.resolve(press, target),
                    Msg::Settings(settings) => self.settings = settings,
                    Msg::Quit => return,
                }
            }
            if let Some((press, target, _)) =
                self.waiting.take_if(|(_, _, due)| win::now_ms() >= *due)
            {
                if !is_web(&focus_chain(&target)) {
                    self.ahead = Some((press, self.selection()));
                }
            }
        }
    }

    fn resolve(&mut self, press: u64, target: Option<WindowInfo>) {
        let started = Instant::now();
        self.waiting = None;
        let ahead = self.ahead.take().filter(|(read, _)| *read == press).map(|(_, s)| s);
        let (selected, how) = match &target {
            Some(target) => self.read_selection(target, ahead),
            None => (None, "no window"),
        };
        let content = match selected {
            Some(content) => Some((content, Origin::Selection)),
            None => self.recent_copy().map(|content| (content, Origin::Clipboard)),
        };
        let capture = Captured { content };
        debug!(
            "capture: press {press} via {how}: {} in {} ms",
            capture.describe(),
            started.elapsed().as_millis()
        );
        self.ui.send(UiMsg::Captured { press, capture });
    }

    fn read_selection(
        &mut self,
        target: &WindowInfo,
        ahead: Option<Selection>,
    ) -> (Option<Content>, &'static str) {
        let chain = focus_chain(target);
        let web = is_web(&chain);
        let selection = match ahead {
            Some(selection) => selection,
            None if web => Selection::Unsupported,
            None => self.selection(),
        };
        match selection {
            Selection::Text(text) => return (Some(Content::Text(text)), "ui automation"),
            // The program says nothing is selected. Believe it: sending a copy
            // shortcut now could only do harm.
            Selection::Empty => return (None, "ui automation"),
            Selection::Unsupported => {}
        }
        if shell::is_file_view(target) {
            let files = shell::selected_files(target);
            return ((!files.is_empty()).then_some(Content::Files(files)), "shell");
        }
        match chord_for(&target.exe, &chain, &self.settings) {
            Some(chord) => (self.copy(chord), "copy shortcut"),
            None => (None, "nothing safe to send"),
        }
    }

    fn selection(&mut self) -> Selection {
        if self.uia.is_none() {
            self.uia = Uia::new();
        }
        self.uia.as_ref().map_or(Selection::Unsupported, Uia::selection)
    }

    /// Send a copy shortcut, read what it produced and restore the clipboard.
    fn copy(&mut self, chord: Chord) -> Option<Content> {
        if !keys::modifiers_released(Duration::from_millis(300)) {
            debug!("capture: a modifier key is still held; not sending a copy shortcut");
            return None;
        }
        let Some(snapshot) = clipboard::snapshot(self.window) else {
            info!("capture: the clipboard is too large to set aside; leaving it alone");
            return None;
        };
        BUSY.store(true, Ordering::Relaxed);
        let before = clipboard::sequence();
        keys::send(chord);

        let deadline = Instant::now() + self.settings.wait;
        while clipboard::sequence() == before {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            wait(left.as_millis().max(1) as u32);
        }
        let mut content = None;
        if clipboard::sequence() != before {
            // A program may empty the clipboard first and fill it a moment
            // later.
            let settle = Instant::now() + Duration::from_millis(60);
            while unsafe { CountClipboardFormats() } == 0 && Instant::now() < settle {
                wait(5);
            }
            content = clipboard::read(self.window, true);
            if !clipboard::restore(self.window, &snapshot) {
                info!("capture: could not put the clipboard back");
            }
        }
        OWN_SEQUENCE.store(clipboard::sequence(), Ordering::Relaxed);
        BUSY.store(false, Ordering::Relaxed);
        content
    }

    /// Something another program put on the clipboard a moment ago.
    fn recent_copy(&self) -> Option<Content> {
        let changed = CHANGED_AT.load(Ordering::Relaxed);
        let age = Duration::from_millis(win::now_ms().checked_sub(changed)?);
        if age > self.settings.recent || self.settings.recent.is_zero() {
            return None;
        }
        clipboard::read(self.window, false)
    }
}

/// Window classes from the focused control up to its top-level window.
fn focus_chain(target: &WindowInfo) -> Vec<String> {
    unsafe {
        let thread = GetWindowThreadProcessId(target.handle(), None);
        let mut gui =
            GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let focus = match GetGUIThreadInfo(thread, &mut gui) {
            Ok(()) if !gui.hwndFocus.0.is_null() => gui.hwndFocus,
            _ => target.handle(),
        };
        let mut chain = Vec::new();
        let mut hwnd = focus;
        while !hwnd.0.is_null() && chain.len() < 16 {
            chain.push(win::class_name(hwnd));
            if hwnd == target.handle() {
                return chain;
            }
            hwnd = GetAncestor(hwnd, GA_PARENT);
        }
        chain.push(target.class.clone());
        chain
    }
}

/// Chromium (Chrome, Edge, Electron, WebView2, CEF) and Firefox. They copy
/// with Ctrl+Insert, and they turn on their accessibility engine for good
/// once a UI Automation client asks them anything.
fn is_web(chain: &[String]) -> bool {
    chain.iter().any(|class| class.starts_with("Chrome_") || class.starts_with("Mozilla"))
}

const TERMINAL_CLASSES: &[&str] = &[
    "ConsoleWindowClass",
    "CASCADIA_HOSTING_WINDOW_CLASS",
    "mintty",
    "PuTTY",
    "KiTTY",
    "VirtualConsoleClass",
    // Java windows: JetBrains IDEs and their built-in terminals.
    "SunAwtFrame",
];

const TERMINAL_EXES: &[&str] = &[
    "windowsterminal.exe",
    "openconsole.exe",
    "conhost.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "mintty.exe",
    "conemu.exe",
    "conemu64.exe",
    "xshell.exe",
    "securecrt.exe",
    "mobaxterm.exe",
    "windterm.exe",
    "finalshell.exe",
    "devenv.exe",
    "zed.exe",
];

/// Terminals that copy with Ctrl+Shift+C and pass Ctrl+Insert to the shell.
const CTRL_SHIFT_C_EXES: &[&str] = &[
    "alacritty.exe",
    "wezterm-gui.exe",
    "rio.exe",
    "ghostty.exe",
    "warp.exe",
    "tabby.exe",
    "hyper.exe",
    "waveterm.exe",
];

/// Terminals that copy as soon as text is selected. Any keystroke would go to
/// the remote shell, and the copy is already on the clipboard.
const NO_KEY_EXES: &[&str] = &["putty.exe", "kitty.exe"];

/// Which copy shortcut is safe to send to this program, if any.
fn chord_for(exe: &str, chain: &[String], settings: &CaptureSettings) -> Option<Chord> {
    if NO_KEY_EXES.contains(&exe) {
        return None;
    }
    if CTRL_SHIFT_C_EXES.contains(&exe) {
        return Some(Chord::CtrlShiftC);
    }
    let ctrl_c_is_safe = settings.ctrl_c_fallback
        && !is_web(chain)
        && !TERMINAL_EXES.contains(&exe)
        && !chain.iter().any(|class| TERMINAL_CLASSES.contains(&class.as_str()))
        && !settings.never_ctrl_c.iter().any(|name| name == exe);
    Some(if ctrl_c_is_safe { Chord::CtrlInsertThenC } else { Chord::CtrlInsert })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CaptureSettings {
        CaptureSettings::from(&config::CaptureSettings::default())
    }

    fn chord(exe: &str, classes: &[&str]) -> Option<Chord> {
        let chain: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
        chord_for(exe, &chain, &settings())
    }

    #[test]
    fn ctrl_c_never_reaches_a_terminal() {
        assert_eq!(chord("windowsterminal.exe", &["CASCADIA_HOSTING_WINDOW_CLASS"]), Some(Chord::CtrlInsert));
        assert_eq!(chord("unknown.exe", &["ConsoleWindowClass"]), Some(Chord::CtrlInsert));
        assert_eq!(chord("mintty.exe", &["mintty"]), Some(Chord::CtrlInsert));
        assert_eq!(chord("idea64.exe", &["SunAwtFrame"]), Some(Chord::CtrlInsert));
        assert_eq!(chord("alacritty.exe", &["Window Class"]), Some(Chord::CtrlShiftC));
        assert_eq!(chord("putty.exe", &["PuTTY"]), None);
    }

    #[test]
    fn web_engines_get_ctrl_insert_only() {
        // Editors built on Electron have a terminal pane.
        assert_eq!(chord("code.exe", &["Chrome_WidgetWin_1"]), Some(Chord::CtrlInsert));
        assert_eq!(chord("firefox.exe", &["MozillaWindowClass"]), Some(Chord::CtrlInsert));
        // A web view embedded in a native window.
        assert_eq!(
            chord("app.exe", &["Chrome_RenderWidgetHostHWND", "Chrome_WidgetWin_0", "AppFrame"]),
            Some(Chord::CtrlInsert)
        );
    }

    #[test]
    fn ordinary_programs_get_both_shortcuts() {
        assert_eq!(chord("wechat.exe", &["WeChatMainWndForPC"]), Some(Chord::CtrlInsertThenC));

        let mut cautious = settings();
        cautious.never_ctrl_c = vec!["wechat.exe".into()];
        let chain = vec!["WeChatMainWndForPC".to_string()];
        assert_eq!(chord_for("wechat.exe", &chain, &cautious), Some(Chord::CtrlInsert));

        let mut off = settings();
        off.ctrl_c_fallback = false;
        assert_eq!(chord_for("wechat.exe", &chain, &off), Some(Chord::CtrlInsert));
    }

    #[test]
    fn clipboard_bitmaps_become_png() {
        // 2 x 1 pixels, 32 bits, alpha left at zero as screenshots do.
        let mut dib = 40u32.to_le_bytes().to_vec();
        dib.extend(2i32.to_le_bytes());
        dib.extend(1i32.to_le_bytes());
        dib.extend(1u16.to_le_bytes());
        dib.extend(32u16.to_le_bytes());
        dib.extend([0u8; 24]);
        dib.extend([255, 0, 0, 0, 0, 255, 0, 0]);
        let png = Image { format: ImageFormat::Dib, bytes: dib }.into_png();
        assert_eq!(png.format, ImageFormat::Png);
        assert_eq!(png.dimensions(), Some((2, 1)));
        let decoded = image::load_from_memory(&png.bytes).unwrap().into_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 255, 255]);

        let broken = Image { format: ImageFormat::Dib, bytes: vec![1, 2, 3] }.into_png();
        assert_eq!(broken.format, ImageFormat::Dib);
    }

    #[test]
    fn image_sizes_are_read_from_the_header() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        png.extend(b"IHDR");
        png.extend(1920u32.to_be_bytes());
        png.extend(1080u32.to_be_bytes());
        assert_eq!(Image { format: ImageFormat::Png, bytes: png }.dimensions(), Some((1920, 1080)));

        let mut dib = 40u32.to_le_bytes().to_vec();
        dib.extend(640i32.to_le_bytes());
        dib.extend((-480i32).to_le_bytes());
        assert_eq!(Image { format: ImageFormat::Dib, bytes: dib }.dimensions(), Some((640, 480)));

        assert_eq!(Image { format: ImageFormat::Png, bytes: vec![0; 8] }.dimensions(), None);
        assert_eq!(Image { format: ImageFormat::Dib, bytes: vec![] }.dimensions(), None);
    }

    #[test]
    fn the_log_line_has_sizes_but_no_content() {
        let capture = Captured {
            content: Some((Content::Text("secret words".into()), Origin::Clipboard)),
        };
        assert_eq!(capture.describe(), "text, 12 chars, from the clipboard");
        assert_eq!(Captured::default().describe(), "nothing");
    }
}
