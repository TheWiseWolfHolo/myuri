//! The panel: a small borderless window by the cursor, listing the assistants
//! that fit what was captured.
//!
//! The window is created once at startup and kept hidden, so asking for the
//! panel only moves and shows it.

use std::time::Duration;

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Subscription, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point, px,
    size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use regex::Regex;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::UI::Input::Ime::{HIMC, ImmAssociateContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetForegroundWindow, HWND_TOPMOST, IsWindow, SW_HIDE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_CLIPCHILDREN, WS_POPUP,
};

use myuri_core::config::{Assistant, ContentKind, ThemeChoice};
use myuri_core::gesture::Pos;
use myuri_core::matching::{self, Keys};

use crate::applog::{debug, info};
use crate::capture::{Captured, Content, Image, ImageFormat, Origin};
use crate::events::{UiMsg, UiSender};
use crate::icon::Glyph;
use crate::theme::{self, Palette};
use crate::win;

// Lengths at 100% scaling.
const WIDTH: f32 = 296.0;
const HEADER: f32 = 38.0;
const ROW: f32 = 32.0;
const PADDING: f32 = 6.0;
const MAX_ROWS: usize = 14;
/// Space between the cursor and the panel's nearest corner. The cursor starts
/// outside the list, so resting on a row always means the user moved there.
const GAP: f32 = 10.0;

pub struct Panel {
    window: WindowHandle<PanelView>,
}

impl Panel {
    pub fn open(
        cx: &mut App,
        theme: ThemeChoice,
        assistants: &[Assistant],
        ui: UiSender,
    ) -> Option<Self> {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(WIDTH), px(HEADER + ROW)),
            ))),
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        };
        let assistants = assistants.to_vec();
        cx.open_window(options, |window, cx| {
            cx.new(|cx| PanelView::new(theme, assistants, ui, window, cx))
        })
        .map(|window| Self { window })
        .inspect_err(|error| info!("panel: cannot create the window: {error}"))
        .ok()
    }

    pub fn update(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut PanelView, &mut Window, &mut Context<PanelView>),
    ) {
        let _ = self.window.update(cx, f);
    }
}

struct Entry {
    assistant: Assistant,
    keys: Keys,
    glyph: Glyph,
    pattern: Option<Regex>,
    preferred: bool,
}

impl Entry {
    fn new(assistant: Assistant) -> Self {
        let pattern = assistant.pattern.as_deref().map(|pattern| {
            Regex::new(pattern).expect("assistant match pattern was validated when reading configuration")
        });
        Self {
            keys: Keys::new(&assistant.name),
            glyph: Glyph::of(&assistant),
            assistant,
            pattern,
            preferred: false,
        }
    }
}

fn prioritize(entries: &mut [Entry], capture: &Captured) {
    let selected = match capture.content.as_ref() {
        Some((Content::Text(text), Origin::Selection)) => Some(text.as_str()),
        _ => None,
    };
    for entry in entries {
        entry.preferred = selected.zip(entry.pattern.as_ref()).is_some_and(|(text, re)| re.is_match(text));
    }
}

fn ranked_rows(entries: &[Entry], kind: Option<ContentKind>, query: &str) -> Vec<usize> {
    let fitting: Vec<_> = entries.iter().enumerate()
        .filter_map(|(index, entry)| entry.assistant.accepts(kind).then_some(index))
        .collect();
    let ranked = matching::rank(fitting.iter().map(|&index| &entries[index].keys), query);
    let mut listed: Vec<_> = ranked.into_iter().map(|position| fitting[position]).collect();
    listed.sort_by_key(|&index| !entries[index].preferred);
    listed
}

pub struct PanelView {
    /// The native window, as an integer so the view stays `Send`-agnostic.
    hwnd: isize,
    ui: UiSender,
    focus: FocusHandle,
    theme: ThemeChoice,
    palette: Palette,
    entries: Vec<Entry>,

    /// The request being answered or shown.
    press: Option<u64>,
    anchor: Pos,
    requested_at: u64,
    capture: Captured,
    /// Indices into `entries` that fit the content and the typed filter.
    listed: Vec<usize>,
    query: String,
    scroll: ScrollHandle,
    /// Row highlighted by the keyboard or the pointer.
    selected: usize,
    /// Row the pointer is resting on.
    hovered: Option<usize>,
    /// Row just chosen, shown for a moment before the panel closes.
    chosen: Option<usize>,
    visible: bool,
    /// Whether the panel has had the keyboard since it was last shown. Losing
    /// it closes the panel; a stale notice from an earlier showing must not.
    activated: bool,
    /// The window that had the focus before the panel took it.
    previous: isize,
    _subscriptions: Vec<Subscription>,
}

impl PanelView {
    fn new(
        theme: ThemeChoice,
        assistants: Vec<Assistant>,
        ui: UiSender,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hwnd = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };
        unsafe {
            let hwnd = HWND(hwnd as _);
            // No caption and no frame: the client area is the whole window.
            SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_CLIPCHILDREN).0 as isize);
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            // Windows 11 rounds the corners and draws the border and shadow.
            let corners = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&raw const corners).cast(),
                size_of_val(&corners) as u32,
            );
            // Typing filters by pinyin initials, so keys must arrive as plain
            // letters rather than go to an input method.
            ImmAssociateContext(hwnd, HIMC::default());
        }
        let subscriptions = vec![
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.activated = true;
                } else if this.visible && this.activated {
                    // The user clicked somewhere else.
                    this.hide(false);
                    cx.notify();
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_theme(this.theme, window);
                cx.notify();
            }),
        ];
        let mut view = Self {
            hwnd,
            ui,
            focus: cx.focus_handle(),
            theme,
            palette: theme::palette(false),
            entries: Vec::new(),
            press: None,
            anchor: Pos::default(),
            requested_at: 0,
            capture: Captured::default(),
            listed: Vec::new(),
            query: String::new(),
            scroll: ScrollHandle::new(),
            selected: 0,
            hovered: None,
            chosen: None,
            visible: false,
            activated: false,
            previous: 0,
            _subscriptions: subscriptions,
        };
        view.apply_theme(theme, window);
        view.set_assistants(assistants);
        view
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as _)
    }

    pub fn set_assistants(&mut self, assistants: Vec<Assistant>) {
        self.entries = assistants
            .into_iter()
            .filter(|assistant| assistant.enabled)
            .map(Entry::new)
            .collect();
        prioritize(&mut self.entries, &self.capture);
        self.refilter();
    }

    pub fn apply_theme(&mut self, theme: ThemeChoice, window: &Window) {
        self.theme = theme;
        self.palette = theme::palette(theme::is_dark(theme, window.appearance()));
        let line = self.palette.hairline;
        let channel = |value: f32| (value * 255.0).round() as u32;
        let color = COLORREF(channel(line.r) | channel(line.g) << 8 | channel(line.b) << 16);
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.hwnd(),
                DWMWA_BORDER_COLOR,
                (&raw const color).cast(),
                size_of::<COLORREF>() as u32,
            );
        }
    }

    /// The panel was asked for at `pos`; the content arrives in `captured`.
    pub fn trigger(&mut self, press: u64, pos: Pos) {
        if self.visible {
            self.hide(false);
        }
        self.press = Some(press);
        self.anchor = pos;
        self.requested_at = win::now_ms();
    }

    pub fn captured(
        &mut self,
        press: u64,
        capture: Captured,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.press == Some(press) {
            self.present(capture, window, cx);
        }
    }

    /// Show the panel with stand-in content.
    pub fn preview(&mut self, kind: Option<ContentKind>, window: &mut Window, cx: &mut Context<Self>) {
        let content = kind.map(|kind| match kind {
            ContentKind::Text => Content::Text(
                "The quick brown fox jumps over the lazy dog，然后被狼追了三条街。\n第二行不会显示。".into(),
            ),
            ContentKind::Image => Content::Image(Image { format: ImageFormat::Png, bytes: Vec::new() }),
            ContentKind::Files => {
                Content::Files(vec!["C:\\示例\\季度报告.pdf".into(), "C:\\示例\\附录.xlsx".into()])
            }
        });
        self.press = None;
        self.anchor = win::cursor_pos();
        self.requested_at = win::now_ms();
        self.present(Captured { content: content.map(|c| (c, Origin::Selection)) }, window, cx);
    }

    /// The right button came up. If the pointer is resting on a row, that row
    /// is the choice: press, slide, release.
    pub fn released(&mut self, press: u64, cx: &mut Context<Self>) {
        if self.visible && self.press == Some(press) {
            if let Some(row) = self.hovered {
                self.choose(row, cx);
            }
        }
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(true);
            cx.notify();
        }
    }

    fn present(&mut self, capture: Captured, window: &mut Window, cx: &mut Context<Self>) {
        self.capture = capture;
        prioritize(&mut self.entries, &self.capture);
        self.query.clear();
        self.hovered = None;
        self.chosen = None;
        self.refilter();
        self.activated = window.is_window_active();
        self.place(true);
        self.visible = true;
        window.focus(&self.focus, cx);
        cx.notify();
        debug!(
            "panel: shown {} ms after the request, {} rows",
            win::now_ms().saturating_sub(self.requested_at),
            self.listed.len()
        );
    }

    fn refilter(&mut self) {
        let kind = self.capture.content.as_ref().map(|(content, _)| content.kind());
        self.listed = ranked_rows(&self.entries, kind, &self.query);
        self.selected = 0;
        self.hovered = None;
        self.scroll.set_offset(point(px(0.0), px(0.0)));
    }

    fn has_header(&self) -> bool {
        self.capture.content.is_some() || !self.query.is_empty()
    }

    /// Size the window to its content. With `at_anchor` it is also moved next
    /// to the cursor, shown and given the keyboard.
    fn place(&mut self, at_anchor: bool) {
        let scale = win::scale_at(self.anchor);
        let rows = self.listed.len().clamp(1, MAX_ROWS) as f32;
        let header = if self.has_header() { HEADER } else { 0.0 };
        let width = (WIDTH * scale).round() as i32;
        let height = ((header + rows * ROW + PADDING * 2.0) * scale).round() as i32;
        unsafe {
            if !at_anchor {
                let _ = SetWindowPos(
                    self.hwnd(),
                    HWND::default(),
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                return;
            }
            let gap = (GAP * scale).round() as i32;
            let (x, y) = match win::work_area_at(self.anchor) {
                Some(area) => beside(self.anchor, (width, height), area, gap),
                None => (self.anchor.x + gap, self.anchor.y + gap),
            };
            let foreground = GetForegroundWindow();
            if foreground != self.hwnd() {
                self.previous = foreground.0 as isize;
            }
            let _ = SetWindowPos(
                self.hwnd(),
                HWND_TOPMOST,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            // Windows only lets a program take the foreground right after it
            // produced input, so produce some: a mouse move of zero pixels.
            let nudge = INPUT { r#type: INPUT_MOUSE, ..Default::default() };
            SendInput(&[nudge], size_of::<INPUT>() as i32);
            if !SetForegroundWindow(self.hwnd()).as_bool() {
                info!("panel: Windows would not give the panel the keyboard");
            }
        }
    }

    /// Hide the panel. With `give_focus_back`, the window that was in front
    /// before gets the keyboard again.
    fn hide(&mut self, give_focus_back: bool) {
        unsafe {
            let _ = ShowWindow(self.hwnd(), SW_HIDE);
            let previous = HWND(self.previous as _);
            if give_focus_back && IsWindow(previous).as_bool() {
                let _ = SetForegroundWindow(previous);
            }
        }
        self.visible = false;
        self.press = None;
        self.hovered = None;
        self.chosen = None;
        // Lets go of the captured text or image.
        self.capture = Captured::default();
    }

    fn choose(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(&entry) = self.listed.get(row) else { return };
        if self.chosen.is_some() {
            return;
        }
        self.selected = row;
        self.chosen = Some(row);
        info!(
            "panel: chose \"{}\" for {}",
            self.entries[entry].assistant.name,
            self.capture.describe()
        );
        cx.notify();
        // Leave the chosen row lit for a moment so the choice registers.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(110)).await;
            let _ = this.update(cx, |this, cx| {
                let Some(row) = this.chosen else { return };
                if let Some(&entry) = this.listed.get(row) {
                    let assistant = this.entries[entry].assistant.name.clone();
                    let content = this.capture.content.take().map(|(content, _)| content);
                    this.ui.send(UiMsg::Chosen { assistant, content, anchor: this.anchor });
                }
                // The assistant window takes the keyboard next.
                this.hide(false);
                cx.notify();
            });
        })
        .detach();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let rows = self.listed.len();
        match keystroke.key.as_str() {
            "escape" if event.is_held => return,
            "escape" if self.query.is_empty() => return self.dismiss(cx),
            "escape" => {
                self.query.clear();
                self.refilter();
                self.place(false);
            }
            "enter" => return self.choose(self.selected, cx),
            "down" | "tab" if rows > 0 && !(keystroke.key == "tab" && modifiers.shift) => {
                self.selected = (self.selected + 1) % rows;
            }
            "up" | "tab" if rows > 0 => self.selected = (self.selected + rows - 1) % rows,
            "home" | "pageup" => self.selected = 0,
            "end" | "pagedown" => self.selected = rows.saturating_sub(1),
            "backspace" => {
                if self.query.pop().is_some() {
                    self.refilter();
                    self.place(false);
                }
            }
            key => {
                let mut chars = key.chars();
                let (Some(typed), None) = (chars.next(), chars.next()) else { return };
                if !typed.is_alphanumeric() {
                    return;
                }
                // With nothing typed yet, a digit picks that row.
                if self.query.is_empty() {
                    if let Some(digit @ 1..=9) = typed.to_digit(10) {
                        if digit as usize <= rows {
                            return self.choose(digit as usize - 1, cx);
                        }
                    }
                }
                self.query.push(typed);
                self.refilter();
                self.place(false);
            }
        }
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    fn header(&self) -> Option<impl IntoElement + use<>> {
        let palette = self.palette;
        let bar = div()
            .h(px(HEADER))
            .flex_none()
            .px(px(14.0))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(palette.hairline);
        if !self.query.is_empty() {
            return Some(
                bar.gap(px(1.0))
                    .child(div().font(theme::mono()).child(self.query.clone()))
                    .child(div().w(px(1.5)).h(px(15.0)).bg(palette.accent)),
            );
        }
        let (content, origin) = self.capture.content.as_ref()?;
        let (lead, detail) = summarize(content, *origin);
        Some(
            bar.gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .font(theme::serif())
                        .text_size(px(13.5))
                        .child(lead),
                )
                .child(div().flex_none().text_size(px(11.0)).text_color(palette.muted).child(detail)),
        )
    }

    fn row(&self, row: usize, entry: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let assistant = &self.entries[entry].assistant;
        let selected = row == self.selected;
        let icon =
            self.entries[entry].glyph.render(20.0, if selected { palette.accent } else { palette.muted });
        div()
            .id(("row", row))
            .h(px(ROW))
            .flex_none()
            .mx(px(PADDING))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .when(selected, |row| row.bg(palette.selected))
            .when(self.chosen == Some(row), |row| row.bg(palette.accent_wash))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered = Some(row);
                    this.selected = row;
                } else if this.hovered == Some(row) {
                    this.hovered = None;
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.choose(row, cx)))
            // The hand is already on the right button.
            .on_mouse_up(MouseButton::Right, cx.listener(move |this, _, _, cx| this.choose(row, cx)))
            .child(icon)
            .child(div().flex_1().min_w(px(0.0)).truncate().child(assistant.name.clone()))
            .when(row < 9 && self.query.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .font(theme::mono())
                        .text_size(px(11.0))
                        .text_color(palette.faint)
                        .child((row + 1).to_string()),
                )
            })
    }
}

impl Render for PanelView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let list = div()
            .id("list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .py(px(PADDING))
            .flex()
            .flex_col();
        let list = if self.listed.is_empty() {
            let message = if self.query.is_empty() { "没有适用的助手" } else { "没有匹配的助手" };
            list.child(
                div()
                    .h(px(ROW))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .text_color(palette.muted)
                    .child(message),
            )
        } else {
            let rows: Vec<_> = self
                .listed
                .iter()
                .enumerate()
                .map(|(row, &entry)| self.row(row, entry, cx))
                .collect();
            list.children(rows)
        };
        div()
            .id("panel")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.surface)
            .text_color(palette.ink)
            .font(theme::sans())
            .text_size(px(13.0))
            .children(self.header())
            .child(list)
    }
}

/// Top-left corner for a panel of `size` next to `anchor`: below and to the
/// right when there is room, otherwise flipped to the other side, and always
/// inside `area`.
pub fn beside(anchor: Pos, size: (i32, i32), area: RECT, gap: i32) -> (i32, i32) {
    let along = |at: i32, extent: i32, low: i32, high: i32| {
        let (after, before) = (at + gap, at - gap - extent);
        let chosen = if after + extent <= high {
            after
        } else if before >= low {
            before
        } else if high - after >= at - gap - low {
            // Fits on neither side: take the roomier one.
            after
        } else {
            before
        };
        chosen.clamp(low, (high - extent).max(low))
    };
    (along(anchor.x, size.0, area.left, area.right), along(anchor.y, size.1, area.top, area.bottom))
}

/// The header's two parts: what was captured, and how much of it.
fn summarize(content: &Content, origin: Origin) -> (String, String) {
    let (lead, detail) = match content {
        Content::Text(text) => {
            let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
            let lead: String =
                line.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
            (lead, format!("{} 字", text.chars().filter(|c| !c.is_whitespace()).count()))
        }
        Content::Image(image) => (
            "图片".to_string(),
            image.dimensions().map(|(w, h)| format!("{w} × {h}")).unwrap_or_default(),
        ),
        Content::Files(files) => {
            let name = files
                .first()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, if files.len() > 1 { format!("{} 个文件", files.len()) } else { String::new() })
        }
    };
    let detail = match origin {
        Origin::Selection => detail,
        Origin::Clipboard if detail.is_empty() => "剪贴板".to_string(),
        Origin::Clipboard => format!("剪贴板 · {detail}"),
    };
    (lead, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_selected_text_takes_priority_without_bypassing_filters() {
        let assistants = myuri_core::config::parse_assistants(r#"
[[assistant]]
name = "通用"

[[assistant]]
name = "报错解释"
match = '^ERROR [0-9]+'

[[assistant]]
name = "报错核对"
match = '^ERROR'

[[assistant]]
name = "图片说明"
input = ["image"]
match = '.'
"#).unwrap();
        let mut entries: Vec<_> = assistants.into_iter().map(Entry::new).collect();
        let text = || Content::Text("ERROR 1234".into());
        let text_kind = Some(ContentKind::Text);
        prioritize(&mut entries, &Captured { content: Some((text(), Origin::Selection)) });
        assert_eq!(ranked_rows(&entries, text_kind, ""), [1, 2, 0]);
        assert_eq!(ranked_rows(&entries, text_kind, "bc"), [1, 2]);
        assert_eq!(ranked_rows(&entries, text_kind, "ty"), [0]);
        prioritize(&mut entries, &Captured { content: Some((text(), Origin::Clipboard)) });
        assert_eq!(ranked_rows(&entries, text_kind, ""), [0, 1, 2]);
    }

    const AREA: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };

    #[test]
    fn the_panel_sits_below_right_of_the_cursor() {
        assert_eq!(beside(Pos::new(500, 300), (300, 400), AREA, 10), (510, 310));
    }

    #[test]
    fn the_panel_flips_instead_of_covering_the_cursor() {
        // Near the right and bottom edges it goes to the left and above.
        assert_eq!(beside(Pos::new(1800, 900), (300, 400), AREA, 10), (1490, 490));
        // Only one axis flips when only one edge is near.
        assert_eq!(beside(Pos::new(1800, 100), (300, 400), AREA, 10), (1490, 110));
    }

    #[test]
    fn the_panel_never_leaves_the_work_area() {
        // No room on either side: stay inside.
        assert_eq!(beside(Pos::new(100, 100), (300, 1000), AREA, 10), (110, 40));
        let second = RECT { left: -1280, top: 0, right: 0, bottom: 720 };
        assert_eq!(beside(Pos::new(-20, 700), (300, 400), second, 10), (-330, 290));
    }

    #[test]
    fn text_is_summarized_by_its_first_line() {
        let content = Content::Text("\n  Hello   wide\tworld  \nsecond line".into());
        assert_eq!(summarize(&content, Origin::Selection), ("Hello wide world".into(), "24 字".into()));
        assert_eq!(
            summarize(&Content::Text("你好".into()), Origin::Clipboard),
            ("你好".into(), "剪贴板 · 2 字".into())
        );
    }

    #[test]
    fn files_and_images_are_summarized() {
        let one = Content::Files(vec!["C:\\docs\\报告.pdf".into()]);
        assert_eq!(summarize(&one, Origin::Selection), ("报告.pdf".into(), String::new()));
        assert_eq!(summarize(&one, Origin::Clipboard), ("报告.pdf".into(), "剪贴板".into()));
        let two = Content::Files(vec!["C:\\a.txt".into(), "C:\\b.txt".into()]);
        assert_eq!(summarize(&two, Origin::Selection), ("a.txt".into(), "2 个文件".into()));

        let image = Content::Image(Image { format: ImageFormat::Png, bytes: Vec::new() });
        assert_eq!(summarize(&image, Origin::Selection), ("图片".into(), String::new()));
    }
}
