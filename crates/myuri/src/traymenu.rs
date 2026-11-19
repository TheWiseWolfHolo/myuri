//! The menu of the tray icon, drawn like the rest of Myuri instead of the
//! gray system menu. A small window that opens at the pointer and closes on
//! a choice, on Escape, or when anything else is clicked.

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, AppContext, Bounds, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, point, px, size, svg,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_TOPMOST, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
};

use myuri_core::gesture::Pos;

use crate::theme::{self, Palette};
use crate::tray::{self, TrayHandle};
use crate::{autostart, panel, win};

const WIDTH: f32 = 216.0;
const ROW: f32 = 34.0;
const RULE: f32 = 9.0;
const PADDING: f32 = 6.0;

/// A row of the menu; `None` is a dividing line.
type Entry = Option<(usize, &'static str, &'static str)>;

const ENTRIES: [Entry; 10] = [
    Some((tray::MENU_SETTINGS, "icons/settings.svg", "设置")),
    Some((tray::MENU_HISTORY, "icons/history.svg", "对话历史")),
    None,
    Some((tray::MENU_PAUSE, "icons/mouse-pointer-click.svg", "暂停右键手势")),
    Some((tray::MENU_AUTOSTART, "icons/power.svg", "开机自启")),
    None,
    Some((tray::MENU_CONFIG, "icons/folder-open.svg", "打开配置文件夹")),
    Some((tray::MENU_LOG, "icons/scroll-text.svg", "打开日志")),
    None,
    Some((tray::MENU_QUIT, "icons/log-out.svg", "退出")),
];

struct MenuView {
    palette: Palette,
    tray: TrayHandle,
    paused: bool,
    autostart: bool,
    /// Whether the window has had the keyboard yet.
    activated: bool,
    _subscriptions: Vec<Subscription>,
}

pub fn open(at: Pos, dark: bool, tray: TrayHandle, cx: &mut App) {
    let rows = ENTRIES.iter().flatten().count() as f32;
    let rules = ENTRIES.len() as f32 - rows;
    let height = PADDING * 2.0 + rows * ROW + rules * RULE;
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(WIDTH), px(height)),
        ))),
        titlebar: None,
        show: false,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    let _ = cx.open_window(options, |window, cx| {
        cx.new(|cx| {
            let subscriptions = vec![
                cx.observe_window_activation(window, |this: &mut MenuView, window, _| {
                    if window.is_window_active() {
                        this.activated = true;
                    } else if this.activated {
                        window.remove_window();
                    }
                }),
                cx.observe_keystrokes(|_, event, window, _| {
                    if event.keystroke.key == "escape" {
                        window.remove_window();
                    }
                }),
            ];
            place(window, at, height);
            MenuView {
                palette: theme::palette(dark),
                paused: tray.is_paused(),
                autostart: autostart::is_enabled(),
                tray,
                activated: false,
                _subscriptions: subscriptions,
            }
        })
    });
}

/// Put the menu next to the pointer, inside the work area, and show it.
fn place(window: &Window, at: Pos, height: f32) {
    let Ok(RawWindowHandle::Win32(handle)) =
        HasWindowHandle::window_handle(window).map(|handle| handle.as_raw())
    else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as _);
    let scale = win::scale_at(at);
    let extent = ((WIDTH * scale).round() as i32, (height * scale).round() as i32);
    let (x, y) = match win::work_area_at(at) {
        Some(area) => panel::beside(at, extent, area, (4.0 * scale).round() as i32),
        None => (at.x - extent.0, at.y - extent.1),
    };
    unsafe {
        let corners = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corners).cast(),
            size_of_val(&corners) as u32,
        );
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, extent.0, extent.1, SWP_SHOWWINDOW);
        // Without the keyboard the menu would not notice a click elsewhere.
        let _ = SetForegroundWindow(hwnd);
    }
}

impl Render for MenuView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let rows = ENTRIES.iter().enumerate().map(|(index, entry)| {
            let Some((choice, icon, label)) = *entry else {
                return div()
                    .h(px(RULE))
                    .flex()
                    .items_center()
                    .child(div().h(px(1.0)).w_full().bg(palette.hairline))
                    .into_any_element();
            };
            let checked = (choice == tray::MENU_PAUSE && self.paused)
                || (choice == tray::MENU_AUTOSTART && self.autostart);
            div()
                .id(("entry", index))
                .h(px(ROW))
                .px(px(10.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(move |style| style.bg(palette.selected))
                .child(svg().path(icon).size(px(15.0)).flex_none().text_color(palette.muted))
                .child(div().flex_1().child(label))
                .when(checked, |row| {
                    row.child(svg().path("icons/check.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                })
                .on_click(cx.listener(move |this, _, window, _| {
                    this.tray.choose(choice);
                    window.remove_window();
                }))
                .into_any_element()
        });
        div()
            .size_full()
            .p(px(PADDING))
            .flex()
            .flex_col()
            .bg(palette.surface)
            .text_color(palette.ink)
            .font(theme::sans())
            .text_size(px(13.0))
            .children(rows)
    }
}
