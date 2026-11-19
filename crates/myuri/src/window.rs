//! The assistant window: one conversation with one assistant.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Animation, AnimationExt, AnyWindowHandle, App, AppContext, Bounds, ClipboardItem, Context, Div,
    ElementId, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, Rgba,
    ScrollEvent, ScrollHandle, Stateful, StatefulInteractiveElement, Styled, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowOptions, div, img, px,
    relative, rgb, size, svg,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
};

use myuri_core::catalog::{self, Catalog};
use myuri_core::config::{Assistant, ModelRef, Provider, Purpose, Reasoning, SpeechSettings};
use myuri_core::image::{self, ImageRequest};

use crate::images;
use crate::secrets;
use crate::speech;
use myuri_core::gesture::Pos;
use myuri_core::history::{self, Conversation, Entry, SavedCall};
use myuri_core::llm::{Event, Picture, Role, ToolCall, ToolResult, Turn};

use crate::mcp::Kit;

use crate::applog::info;
use crate::capture::{Content, ImageFormat};
use crate::chat::Route;
use crate::icon::Glyph;
use crate::net::Abort;
use crate::picker::{ModelPicker, PickerEvent};
use crate::theme::{self, Palette};
use crate::{panel, win};

/// What the window needs to know to start.
pub struct Opening {
    pub assistant: Assistant,
    pub system: String,
    pub route: Result<Route, String>,
    pub content: Option<Content>,
    pub dark: bool,
    /// Where the panel was asked for; the window opens next to it.
    pub anchor: Pos,
    /// Every provider, for switching models.
    pub providers: Vec<Provider>,
    pub favorites: Vec<ModelRef>,
    /// `[speech]` from config.toml: which provider/model listens and speaks.
    pub speech: SpeechSettings,
    pub catalog: Arc<Catalog>,
    /// A saved conversation to continue.
    pub resume: Option<Conversation>,
}

/// What is open above the input box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sheet {
    None,
    Models,
    Reasoning,
    /// The 比例 and 质量 menus in picture mode.
    Ratio,
    Quality,
}

/// A generated picture saved to disk, with its preview.
struct Shot {
    path: PathBuf,
    thumb: Option<Arc<gpui_kit::Image>>,
}

/// Where a tool call stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallStatus {
    /// Just asked for by the model.
    Pending,
    /// The user has to allow it first.
    Waiting,
    Running,
    Done,
    Failed,
    Denied,
}

struct CallState {
    call: ToolCall,
    status: CallStatus,
    /// What goes back to the model.
    result: String,
    /// The arguments and the result are shown.
    open: bool,
}

impl CallState {
    fn settled(&self) -> bool {
        matches!(self.status, CallStatus::Done | CallStatus::Failed | CallStatus::Denied)
    }
}

/// How many times in a row the model may come back for more tools.
const MAX_ROUNDS: usize = 16;
/// Longer tool results are cut before they go to the model.
const MAX_RESULT: usize = 60_000;

struct Message {
    role: Role,
    text: String,
    pictures: Vec<Picture>,
    /// The picture as shown in the conversation.
    thumb: Option<Arc<gpui_kit::Image>>,
    /// Pictures the assistant generated.
    shots: Vec<Shot>,
    /// Replies are rendered as Markdown; user messages as written.
    view: Option<Entity<TextViewState>>,
    /// A long user message shown in full.
    expanded: bool,
    /// What the model thought before answering, where the provider shares it.
    thoughts: String,
    /// When the thinking began, and how long it took once the answer started.
    thinking_since: Option<Instant>,
    thought_for: Option<Duration>,
    thoughts_open: bool,
    /// Tools the assistant asked for in this message.
    calls: Vec<CallState>,
}

impl Message {
    /// A reply that has not received anything yet.
    fn blank(&self) -> bool {
        self.role == Role::Assistant
            && self.text.is_empty()
            && self.shots.is_empty()
            && self.thoughts.is_empty()
            && self.calls.is_empty()
    }
}

pub struct ChatView {
    handle: AnyWindowHandle,
    hwnd: isize,
    assistant: Assistant,
    glyph: Glyph,
    system: String,
    route: Result<Route, String>,
    /// For this conversation; starts as the assistant's setting.
    reasoning: Option<Reasoning>,
    providers: Vec<Provider>,
    catalog: Arc<Catalog>,
    picker: Entity<ModelPicker>,
    sheet: Sheet,
    /// The sheet a mouse press just closed, so the same click on its button
    /// does not open it again.
    dismissed: Sheet,
    palette: Palette,
    messages: Vec<Message>,
    input: Entity<TextareaState>,
    scroll: ScrollHandle,
    /// Keep the newest text in view while it streams.
    follow: bool,
    /// The request in flight.
    abort: Option<Abort>,
    /// Counts requests, so events from a stopped one are recognized.
    generation: u64,
    /// An image request is in flight; it cannot be aborted, only ignored.
    generating: bool,
    /// Picture mode: aspect ratio and quality, `None` being 自动.
    ratio: Option<&'static str>,
    quality: Option<&'static str>,
    error: Option<String>,
    /// The user pinned the window: losing focus no longer closes it.
    keep_open: bool,
    /// Whether the window has had the keyboard yet.
    activated: bool,
    /// The reply whose copy button was just used.
    copied: Option<usize>,
    /// `[speech]` settings: which provider listens and speaks.
    speech: SpeechSettings,
    /// The message being read out, and a counter against late results.
    speaking: Option<usize>,
    speak_generation: u64,
    /// An open microphone recording, when the user is dictating.
    recorder: Option<speech::Recorder>,
    /// A recording in flight being transcribed.
    transcribing: bool,
    /// Transcribed text waiting for the next frame to type into the box.
    dictated: Option<String>,
    _subscriptions: Vec<Subscription>,
    /// Under which id and creation time the conversation is saved.
    conversation: String,
    created: u64,
    /// The assistant's tools and skills, gathered at the first request.
    kit: Option<Arc<Kit>>,
    kit_loading: bool,
    /// Servers whose tools need no further asking in this conversation.
    trusted: std::collections::HashSet<String>,
    /// Requests made in a row because the model asked for tools.
    rounds: usize,
}

pub fn open(opening: Opening, cx: &mut App) {
    let prefs = &opening.assistant.window;
    let name = opening.assistant.name.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(prefs.width as f32), px(prefs.height as f32)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(name.clone().into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_min_size: Some(size(px(340.0), px(300.0))),
        // Shown once it is in place, so it never jumps across the screen.
        show: false,
        ..Default::default()
    };
    if let Err(error) =
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| ChatView::new(opening, window, cx)))
    {
        info!("window: cannot open \"{name}\": {error}");
    }
}

impl ChatView {
    fn new(opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hwnd = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };
        let input =
            cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6).placeholder("继续问"));
        let catalog = opening.catalog;
        let palette = theme::palette(opening.dark);
        let picker = cx.new(|cx| {
            ModelPicker::new(
                opening.providers.clone(),
                opening.favorites.clone(),
                catalog.clone(),
                Purpose::Chat,
                palette,
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, window, cx| match event {
                PickerEvent::Chosen(reference) => {
                    this.route = Route::to(&this.providers, reference);
                    this.error = this.route.as_ref().err().cloned();
                    this.close_sheet(window, cx);
                }
                PickerEvent::Dismissed => this.close_sheet(window, cx),
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
                // Enter ends a dictation; the second Enter sends the message.
                InputEvent::PressEnter { shift: false, .. } if this.recorder.is_some() => {
                    this.finish_recording(window, cx)
                }
                InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
            cx.observe_window_activation(window, |this, window, _| {
                if window.is_window_active() {
                    this.activated = true;
                } else if this.activated && this.assistant.window.close_on_blur && !this.keep_open {
                    window.remove_window();
                }
            }),
            cx.observe_keystrokes(|this, event, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.modified() || Window::window_handle(window) != this.handle {
                    return;
                }
                match (this.sheet, keystroke.key.as_str()) {
                    // Escape stops a reply first and closes the window second.
                    (Sheet::None, "escape") if this.busy() => this.stop(cx),
                    (Sheet::None, "escape") => window.remove_window(),
                    (_, "escape") => this.close_sheet(window, cx),
                    _ => {}
                }
            }),
        ];
        let mut view = Self {
            handle: Window::window_handle(window),
            hwnd,
            glyph: Glyph::of(&opening.assistant),
            reasoning: opening.assistant.reasoning,
            assistant: opening.assistant,
            system: opening.system,
            route: opening.route,
            providers: opening.providers,
            catalog,
            picker,
            sheet: Sheet::None,
            dismissed: Sheet::None,
            palette,
            messages: Vec::new(),
            input,
            scroll: ScrollHandle::new(),
            follow: true,
            abort: None,
            generation: 0,
            generating: false,
            ratio: None,
            quality: None,
            error: None,
            keep_open: false,
            activated: false,
            copied: None,
            speech: opening.speech,
            speaking: None,
            speak_generation: 0,
            recorder: None,
            transcribing: false,
            dictated: None,
            conversation: history::new_id(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or_default(),
            ),
            created: crate::historyview::now(),
            kit: None,
            kit_loading: false,
            trusted: Default::default(),
            rounds: 0,
            _subscriptions: subscriptions,
        };
        if let Some(conversation) = opening.resume {
            view.restore(conversation, cx);
        }
        if let Some(content) = opening.content {
            let kind = content.kind();
            let (turn, thumb) = first_turn(content);
            view.push(turn, thumb, cx);
            if view.assistant.sends(Some(kind)) {
                view.request(cx);
            }
        }
        view.place(opening.anchor);
        view.input.update(cx, |input, cx| input.focus(window, cx));
        view
    }

    /// Size the window, move it next to `anchor`, apply the always-on-top
    /// setting, then show it and give it the keyboard.
    fn place(&self, anchor: Pos) {
        let hwnd = HWND(self.hwnd as _);
        let prefs = &self.assistant.window;
        unsafe {
            let quiet = SWP_NOZORDER | SWP_NOACTIVATE;
            // Onto the right monitor first, so that its scaling is the one in force.
            let _ = SetWindowPos(hwnd, HWND::default(), anchor.x, anchor.y, 0, 0, quiet | SWP_NOSIZE);
            let scale = win::scale_at(anchor);
            let want = (
                (prefs.width as f32 * scale).round() as i32,
                (prefs.height as f32 * scale).round() as i32,
            );
            let _ = SetWindowPos(hwnd, HWND::default(), 0, 0, want.0, want.1, quiet | SWP_NOMOVE);
            // The invisible resize border counts toward the window's own size.
            let (mut outer, mut inner) = (RECT::default(), RECT::default());
            let _ = GetWindowRect(hwnd, &mut outer);
            let _ = GetClientRect(hwnd, &mut inner);
            let extent = (
                want.0 + (outer.right - outer.left) - inner.right,
                want.1 + (outer.bottom - outer.top) - inner.bottom,
            );
            let gap = (12.0 * scale).round() as i32;
            let (x, y) = match win::work_area_at(anchor) {
                Some(area) => panel::beside(anchor, extent, area, gap),
                None => (anchor.x, anchor.y),
            };
            let above = if prefs.pinned { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(hwnd, above, x, y, extent.0, extent.1, SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        }
    }

    fn push(&mut self, turn: Turn, thumb: Option<Arc<gpui_kit::Image>>, cx: &mut Context<Self>) {
        let view = (turn.role == Role::Assistant)
            .then(|| cx.new(|cx| TextViewState::markdown(&turn.text, cx)));
        self.messages.push(Message {
            role: turn.role,
            text: turn.text,
            pictures: turn.pictures,
            thumb,
            shots: Vec::new(),
            view,
            expanded: false,
            thoughts: String::new(),
            thinking_since: None,
            thought_for: None,
            thoughts_open: false,
            calls: Vec::new(),
        });
    }

    /// Put a saved conversation back on screen, to be continued.
    fn restore(&mut self, conversation: Conversation, cx: &mut Context<Self>) {
        use base64::Engine;
        self.conversation = conversation.id;
        self.created = conversation.created;
        for entry in conversation.messages {
            let role = if entry.role == "user" { Role::User } else { Role::Assistant };
            let pictures: Vec<Picture> =
                entry.pictures.into_iter().map(|(mime, base64)| Picture { mime, base64 }).collect();
            let thumb = pictures.first().and_then(|picture| {
                let bytes = base64::engine::general_purpose::STANDARD.decode(&picture.base64).ok()?;
                gpui_image(&bytes)
            });
            let turn = match role {
                Role::User => Turn { pictures, ..Turn::user(entry.text) },
                Role::Assistant => Turn::assistant(entry.text),
            };
            self.push(turn, thumb, cx);
            if let Some(message) = self.messages.last_mut() {
                message.calls = entry
                    .calls
                    .into_iter()
                    .map(|saved| CallState {
                        status: if saved.failed { CallStatus::Failed } else { CallStatus::Done },
                        call: ToolCall { id: saved.id, name: saved.name, arguments: saved.arguments, echo: saved.echo },
                        result: saved.result,
                        open: false,
                    })
                    .collect();
                message.thoughts = entry.thoughts;
                message.shots = entry
                    .shots
                    .into_iter()
                    .map(PathBuf::from)
                    .map(|path| Shot { thumb: std::fs::read(&path).ok().and_then(|bytes| gpui_image(&bytes)), path })
                    .collect();
            }
        }
    }

    /// Save the conversation as it stands, off the UI thread.
    fn remember(&self) {
        let answered = self.messages.iter().any(|m| m.role == Role::Assistant);
        if !answered {
            return;
        }
        let conversation = Conversation {
            id: self.conversation.clone(),
            assistant: self.assistant.name.clone(),
            model: self
                .route
                .as_ref()
                .map(|route| format!("{}/{}", route.provider_id, route.model))
                .unwrap_or_default(),
            created: self.created,
            updated: crate::historyview::now(),
            messages: self
                .messages
                .iter()
                .map(|m| Entry {
                    role: if m.role == Role::User { "user" } else { "assistant" }.into(),
                    text: m.text.clone(),
                    thoughts: m.thoughts.clone(),
                    pictures: m.pictures.iter().map(|p| (p.mime.clone(), p.base64.clone())).collect(),
                    shots: m.shots.iter().map(|shot| shot.path.display().to_string()).collect(),
                    calls: m
                        .calls
                        .iter()
                        .map(|state| SavedCall {
                            id: state.call.id.clone(),
                            name: state.call.name.clone(),
                            arguments: state.call.arguments.clone(),
                            echo: state.call.echo.clone(),
                            result: state.result.clone(),
                            failed: state.status != CallStatus::Done,
                        })
                        .collect(),
                })
                .collect(),
        };
        let dir = crate::config_dir();
        std::thread::spawn(move || {
            if let Err(error) = history::save(&dir, &conversation) {
                info!("history: cannot save: {error}");
            }
        });
    }

    /// Enter in the input box, or the send button.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        // Enter itself lands in the box as a line break; clear that too.
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        // Content that was waiting for a question goes out with it.
        match self.messages.last_mut().filter(|last| last.role == Role::User) {
            Some(waiting) => {
                if !waiting.text.is_empty() && !text.is_empty() {
                    waiting.text.push_str("\n\n");
                }
                waiting.text.push_str(&text);
            }
            None if text.is_empty() => return,
            None => self.push(Turn::user(text), None, cx),
        }
        self.rounds = 0;
        self.request(cx);
    }

    /// The current model draws pictures instead of chatting.
    fn picture_mode(&self) -> bool {
        self.route
            .as_ref()
            .ok()
            .and_then(|route| self.providers.iter().find(|p| p.id == route.provider_id))
            .is_some_and(|provider| provider.image_models.contains(&self.route.as_ref().unwrap().model))
    }

    /// A chat or image request is in flight.
    fn busy(&self) -> bool {
        self.abort.is_some() || self.generating || self.kit_loading || self.tools_open()
    }

    /// The newest reply has tool calls that are not finished.
    fn tools_open(&self) -> bool {
        self.messages.last().is_some_and(|m| m.calls.iter().any(|state| !state.settled()))
    }

    /// Ask for a reply to the conversation so far.
    fn request(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.follow = true;
        if self.picture_mode() {
            return self.generate(cx);
        }
        if let Err(problem) = &self.route {
            self.error = Some(problem.clone());
            return cx.notify();
        }
        // The reply fills in as it streams.
        if !self.messages.last().is_some_and(Message::blank) {
            self.push(Turn::assistant(""), None, cx);
        }
        self.generation += 1;
        let generation = self.generation;
        // Servers start the first time they are needed, which can take a
        // moment; the request goes out once they are up.
        let equipped = !self.assistant.tools.is_empty() || !self.assistant.skills.is_empty();
        if equipped && self.kit.is_none() {
            self.kit_loading = true;
            let (assistant, dir) = (self.assistant.clone(), crate::config_dir());
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(Kit::gather(&dir, &assistant));
            });
            cx.spawn(async move |this, cx| {
                let Ok(kit) = rx.recv().await else { return };
                let _ = this.update(cx, |this, cx| {
                    this.kit_loading = false;
                    let problems = kit.problems.join("；");
                    this.kit = Some(Arc::new(kit));
                    if this.generation == generation {
                        this.request(cx);
                        if !problems.is_empty() {
                            this.error = Some(problems);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            return cx.notify();
        }
        let answered = &self.messages[..self.messages.len() - 1];
        let mut turns: Vec<Turn> = Vec::new();
        for m in answered {
            let calls: Vec<ToolCall> = m.calls.iter().map(|state| state.call.clone()).collect();
            let results: Vec<ToolResult> = m
                .calls
                .iter()
                .map(|state| ToolResult {
                    id: state.call.id.clone(),
                    name: state.call.name.clone(),
                    text: state.result.clone(),
                })
                .collect();
            turns.push(Turn { calls, ..Turn { role: m.role, text: m.text.clone(), pictures: m.pictures.clone(), calls: Vec::new(), results: Vec::new() } });
            if !results.is_empty() {
                turns.push(Turn { results, ..Turn::user("") });
            }
        }
        let (tx, rx) = async_channel::unbounded();
        let Ok(route) = &self.route else { return };
        let info = self.catalog.lookup(&route.model);
        let (reasoning, temperature) = (self.reasoning, self.assistant.temperature);
        let kit = self.kit.clone().unwrap_or_default();
        let system = format!("{}{}", self.system, kit.prompt);
        self.abort = Some(route.send(&system, &turns, reasoning, temperature, info, &kit.tools, move |event| {
            let _ = tx.try_send(event);
        }));
        cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                // Take everything that has arrived, so a burst costs one frame.
                let mut events = vec![first];
                while let Ok(more) = rx.try_recv() {
                    events.push(more);
                }
                if this.update(cx, |this, cx| this.on_events(generation, events, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_events(&mut self, generation: u64, events: Vec<Event>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        for event in events {
            match event {
                Event::Text(piece) => {
                    if let Some(reply) = self.messages.last_mut() {
                        if reply.thought_for.is_none() {
                            reply.thought_for = reply.thinking_since.map(|since| since.elapsed());
                        }
                        reply.text.push_str(&piece);
                        if let Some(view) = &reply.view {
                            view.update(cx, |view, cx| view.push_str(&piece, cx));
                        }
                    }
                }
                Event::Reasoning(piece) => {
                    if let Some(reply) = self.messages.last_mut() {
                        reply.thinking_since.get_or_insert_with(Instant::now);
                        reply.thoughts.push_str(&piece);
                    }
                }
                Event::Call(call) => {
                    if let Some(reply) = self.messages.last_mut() {
                        reply.calls.push(CallState { call, status: CallStatus::Pending, result: String::new(), open: false });
                    }
                }
                Event::Done => {
                    self.abort = None;
                    if let Some(reply) = self.messages.last_mut() {
                        if reply.thought_for.is_none() {
                            reply.thought_for = reply.thinking_since.map(|since| since.elapsed());
                        }
                    }
                    self.drop_empty_reply();
                    self.remember();
                    // The model asked for tools: run them, then ask again.
                    self.advance(cx);
                }
                Event::Error(reason) => {
                    self.abort = None;
                    self.error = Some(reason);
                    self.drop_empty_reply();
                }
            }
        }
        if self.follow {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Draw a picture from the last user message, off the UI thread.
    fn generate(&mut self, cx: &mut Context<Self>) {
        let prompt = self
            .messages
            .last()
            .filter(|m| m.role == Role::User)
            .map(|m| m.text.clone())
            .unwrap_or_default();
        let route = self
            .route
            .as_ref()
            .ok()
            .and_then(|route| {
                self.providers
                    .iter()
                    .find(|p| p.id == route.provider_id)
                    .map(|provider| (provider.clone(), route.model.clone()))
            });
        let Some((provider, model)) = route else {
            self.error = Some("选择生图模型".into());
            return cx.notify();
        };
        self.push(Turn::assistant(""), None, cx);
        self.generation += 1;
        let generation = self.generation;
        self.generating = true;
        let (ratio, quality) = (self.ratio, self.quality);
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let request =
                ImageRequest { model: &model, prompt: &prompt, size: ratio, quality, format: None };
            let _ = tx.send_blocking(images::generate(&provider, &request));
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_generated(generation, result, cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// The picture request answered; a stopped one is dropped on the floor.
    fn on_generated(&mut self, generation: u64, result: Result<Vec<Vec<u8>>, String>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.generating = false;
        match result {
            Err(reason) => {
                self.error = Some(reason);
                self.drop_empty_reply();
            }
            Ok(pictures) => {
                let folder = images::folder();
                let mut shots = Vec::new();
                for (index, bytes) in pictures.iter().enumerate() {
                    match images::save(&folder, bytes, index) {
                        Ok(path) => shots.push(Shot { path, thumb: gpui_image(bytes) }),
                        Err(reason) => self.error = Some(reason),
                    }
                }
                if shots.is_empty() {
                    self.drop_empty_reply();
                } else if let Some(reply) = self.messages.last_mut() {
                    reply.shots = shots;
                    self.remember();
                }
            }
        }
        if self.follow {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// A reply with nothing in it would be sent back as an empty turn.
    fn drop_empty_reply(&mut self) {
        if self.messages.last().is_some_and(Message::blank) {
            self.messages.pop();
        }
    }

    /// Move the newest reply's tool calls along: ask the user where the
    /// server wants that, start the rest, and once every call has an answer
    /// send them all back to the model.
    fn advance(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.messages.len().checked_sub(1) else { return };
        if self.messages[index].calls.is_empty() {
            return;
        }
        let kit = self.kit.clone().unwrap_or_default();
        for n in 0..self.messages[index].calls.len() {
            if self.messages[index].calls[n].status != CallStatus::Pending {
                continue;
            }
            let name = self.messages[index].calls[n].call.name.clone();
            let asks = kit.asks_first(&name)
                && !kit.server(&name).is_some_and(|server| self.trusted.contains(server));
            if asks {
                self.messages[index].calls[n].status = CallStatus::Waiting;
            } else {
                self.run_call(index, n, cx);
            }
        }
        if self.messages[index].calls.iter().all(CallState::settled) && self.abort.is_none() {
            self.remember();
            self.rounds += 1;
            if self.rounds > MAX_ROUNDS {
                self.error = Some(format!("连着调用了 {MAX_ROUNDS} 轮工具，先停在这里"));
            } else {
                self.request(cx);
            }
        }
        cx.notify();
    }

    /// Carry out one tool call off the UI thread.
    fn run_call(&mut self, index: usize, n: usize, cx: &mut Context<Self>) {
        let Some(kit) = self.kit.clone() else {
            let state = &mut self.messages[index].calls[n];
            state.status = CallStatus::Failed;
            state.result = "Error: the tool is not available.".into();
            return;
        };
        let state = &mut self.messages[index].calls[n];
        state.status = CallStatus::Running;
        let (name, arguments) = (state.call.name.clone(), state.call.arguments.clone());
        let generation = self.generation;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(kit.call(&name, &arguments));
        });
        cx.spawn(async move |this, cx| {
            let Ok(outcome) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let Some(state) = this.messages.get_mut(index).and_then(|m| m.calls.get_mut(n)) else { return };
                match outcome {
                    Ok(text) => {
                        state.status = CallStatus::Done;
                        state.result = match text.char_indices().nth(MAX_RESULT) {
                            Some((cut, _)) => format!("{}\n…(truncated)", &text[..cut]),
                            None => text,
                        };
                    }
                    Err(reason) => {
                        state.status = CallStatus::Failed;
                        state.result = format!("Error: {reason}");
                    }
                }
                this.advance(cx);
            });
        })
        .detach();
    }

    /// The user answered the question whether a tool may run. `always`
    /// extends a yes to the rest of the conversation for that server.
    fn decide(&mut self, index: usize, n: usize, allow: bool, always: bool, cx: &mut Context<Self>) {
        let Some(state) = self.messages.get_mut(index).and_then(|m| m.calls.get_mut(n)) else { return };
        if state.status != CallStatus::Waiting {
            return;
        }
        if !allow {
            state.status = CallStatus::Denied;
            state.result = "The user declined this tool call.".into();
        } else if always {
            let name = state.call.name.clone();
            let server = self.kit.as_ref().and_then(|kit| kit.server(&name).map(str::to_string));
            self.trusted.extend(server);
            // Others waiting on the same server go ahead too.
            for state in &mut self.messages[index].calls {
                if state.status == CallStatus::Waiting {
                    state.status = CallStatus::Pending;
                }
            }
        } else {
            self.run_call(index, n, cx);
        }
        self.advance(cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.generating = false;
        self.kit_loading = false;
        // Whatever the stopped request still delivers belongs to nobody.
        self.generation += 1;
        // Every call needs an answer before the conversation can go on.
        for state in self.messages.last_mut().into_iter().flat_map(|m| &mut m.calls) {
            if !state.settled() {
                state.status = CallStatus::Denied;
                state.result = "Cancelled by the user.".into();
            }
        }
        self.drop_empty_reply();
        // What arrived before the stop is kept.
        self.remember();
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.abort.is_some() {
            return;
        }
        // Tool rounds make several replies to one question; redo them all.
        while self.messages.last().is_some_and(|m| m.role == Role::Assistant) {
            self.messages.pop();
        }
        if !self.messages.is_empty() {
            self.rounds = 0;
            self.request(cx);
        }
    }

    /// The reasoning levels the current model offers, and whether it can only
    /// be switched on and off. Models the catalog does not know get the
    /// common set.
    fn reasoning_levels(&self) -> (Vec<Reasoning>, bool) {
        let info = self.route.as_ref().ok().and_then(|route| self.catalog.lookup(&route.model));
        match info {
            Some(info) => (info.levels(), info.switch_only()),
            None => (vec![Reasoning::None, Reasoning::Low, Reasoning::Medium, Reasoning::High], false),
        }
    }

    /// A button under the input box was clicked.
    fn chip_clicked(&mut self, sheet: Sheet, window: &mut Window, cx: &mut Context<Self>) {
        // The press that closed this sheet is the same click; leave it closed.
        if std::mem::replace(&mut self.dismissed, Sheet::None) == sheet {
            return;
        }
        self.sheet = sheet;
        if sheet == Sheet::Models {
            let current = self
                .route
                .as_ref()
                .ok()
                .map(|route| ModelRef { provider: route.provider_id.clone(), model: route.model.clone() });
            self.picker.update(cx, |picker, cx| picker.open(current.as_ref(), window, cx));
        }
        cx.notify();
    }

    fn close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sheet = Sheet::None;
        self.picker.update(cx, |picker, _| picker.sleep());
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn sheet(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let palette = self.palette;
        let body = match self.sheet {
            Sheet::None => return None,
            Sheet::Models => self.picker.clone().into_any_element(),
            Sheet::Reasoning => {
                let (levels, switch_only) = self.reasoning_levels();
                let chosen = self.reasoning.map(|level| catalog::nearest(&levels, level).unwrap_or(level));
                let options = std::iter::once(None).chain(levels.into_iter().map(Some));
                let rows = options.enumerate().map(|(index, value)| {
                    sheet_row(("reasoning", index), palette, false)
                        .child(div().flex_1().child(reasoning_label(value, switch_only)))
                        .when(value == chosen, |row| {
                            row.child(svg().path("icons/check.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.reasoning = value;
                            this.close_sheet(window, cx);
                        }))
                }).collect::<Vec<_>>();
                div().p(px(5.0)).flex().flex_col().children(rows).into_any_element()
            }
            Sheet::Ratio => {
                let rows = [None, Some("1:1"), Some("16:9"), Some("9:16"), Some("4:3"), Some("3:4")]
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| {
                        sheet_row(("ratio", index), palette, false)
                            .child(div().flex_1().child(value.unwrap_or("自动")))
                            .when(value == self.ratio, |row| {
                                row.child(svg().path("icons/check.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.ratio = value;
                                this.close_sheet(window, cx);
                            }))
                    })
                    .collect::<Vec<_>>();
                div().p(px(5.0)).flex().flex_col().children(rows).into_any_element()
            }
            Sheet::Quality => {
                let qualities: [(Option<&'static str>, &str); 4] =
                    [(None, "自动"), (Some("low"), "低"), (Some("medium"), "中"), (Some("high"), "高")];
                let rows = qualities
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, label))| {
                        sheet_row(("quality", index), palette, false)
                            .child(div().flex_1().child(label))
                            .when(value == self.quality, |row| {
                                row.child(svg().path("icons/check.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.quality = value;
                                this.close_sheet(window, cx);
                            }))
                    })
                    .collect::<Vec<_>>();
                div().p(px(5.0)).flex().flex_col().children(rows).into_any_element()
            }
        };
        let wide = self.sheet == Sheet::Models;
        Some(
            div()
                .id("sheet")
                .absolute()
                .bottom(relative(1.0))
                .left(px(14.0))
                // The model sheet spans the input box; the other is a small menu.
                .when(wide, |sheet| sheet.right(px(14.0)))
                .when(!wide, |sheet| sheet.w(px(150.0)))
                .rounded(px(14.0))
                .border_1()
                .border_color(palette.hairline)
                .bg(palette.raised)
                .shadow_lg()
                .overflow_hidden()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismissed = this.sheet;
                    this.close_sheet(window, cx);
                }))
                .child(body),
        )
    }

    fn copy(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(message) = self.messages.get(index) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(message.text.clone()));
        self.copied = Some(index);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1400)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == Some(index) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The provider and model that handle one direction of speech, or none
    /// when it is unset, unmatched or keyless.
    fn speech_route(&self, reference: Option<&String>) -> Option<(Provider, String)> {
        let (id, model) = reference?.split_once('/')?;
        let provider = self.providers.iter().find(|p| p.id == id)?;
        if provider.speech_api.is_none() || secrets::read(&provider.id).is_none() {
            return None;
        }
        Some((provider.clone(), model.to_string()))
    }

    /// Read a reply out loud, or stop its reading. Synthesis runs off the UI
    /// thread; a stale result is dropped like a stopped reply.
    fn speak(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.speaking == Some(index) {
            self.speaking = None;
            speech::silence();
            return cx.notify();
        }
        let Some(route) = self.speech_route(self.speech.tts.as_ref()) else { return };
        let Some(text) =
            self.messages.get(index).map(|m| m.text.clone()).filter(|t| !t.is_empty())
        else { return };
        self.speaking = Some(index);
        self.speak_generation += 1;
        let generation = self.speak_generation;
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(speech::tts(&route.0, &route.1, &text));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.speak_generation != generation {
                    return;
                }
                match result {
                    Ok(wav) => {
                        let seconds = wav_seconds(&wav);
                        speech::play(wav);
                        // Let the button go back to 朗读 when the sound ends.
                        this.speak_generation += 1;
                        let generation = this.speak_generation;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_secs_f32(seconds.min(120.0)))
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                if this.speak_generation == generation {
                                    this.speaking = None;
                                    cx.notify();
                                }
                            });
                        })
                        .detach();
                        this.speaking = Some(index);
                    }
                    Err(reason) => {
                        this.error = Some(reason);
                        this.speaking = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The microphone: click to record, click again to send it to the ears.
    fn dictate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transcribing {
            return;
        }
        if self.recorder.is_some() {
            self.finish_recording(window, cx);
            return;
        }
        match speech::record() {
            Ok(recorder) => {
                self.recorder = Some(recorder);
                // Refresh the elapsed counter until the recording stops.
                cx.spawn(async move |this, cx| {
                    while this
                        .update(cx, |this, cx| {
                            cx.notify();
                            this.recorder.is_some()
                        })
                        .unwrap_or(false)
                    {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                    }
                })
                .detach();
            }
            Err(reason) => self.error = Some(reason),
        }
        cx.notify();
    }

    /// Send the recording to the speech-to-text model, then type it in.
    fn finish_recording(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(recorder) = self.recorder.take() else { return };
        let audio = recorder.stop();
        let Some(route) = self.speech_route(self.speech.asr.as_ref()) else { return };
        self.transcribing = true;
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(speech::stt(&route.0, &route.1, &audio));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                this.transcribing = false;
                match result {
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            this.dictated = Some(text);
                        }
                    }
                    Err(reason) => this.error = Some(reason),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let pin = if self.keep_open { "icons/pin.svg" } else { "icons/pin-off.svg" };
        let pin_color = if self.keep_open { palette.accent } else { palette.faint };
        div()
            .h(px(46.0))
            .flex_none()
            .pl(px(16.0))
            .pr(px(9.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .window_control_area(WindowControlArea::Drag)
            .child(self.glyph.render(20.0, palette.accent))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .text_size(px(13.5))
                    .child(self.assistant.name.clone()),
            )
            .child(div().flex_1())
            .when(self.assistant.window.close_on_blur, |row| {
                row.child(icon_button("pin", pin, pin_color, palette.selected).occlude().on_click(
                    cx.listener(|this, _, _, cx| {
                        this.keep_open = !this.keep_open;
                        cx.notify();
                    }),
                ))
            })
            .child(
                icon_button("history", "icons/clock.svg", palette.faint, palette.selected)
                    .occlude()
                    .on_click(|_, _, _| crate::events::post(crate::events::UiMsg::Page("history"))),
            )
            .child(
                icon_button("close", "icons/x.svg", palette.muted, palette.selected)
                    .occlude()
                    .on_click(|_, window, _| window.remove_window()),
            )
    }

    fn user_row(
        &self,
        index: usize,
        message: &Message,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let long = message.text.chars().count() > 260 || message.text.lines().count() > 6;
        let bubble = div()
            .id(("user", index))
            .max_w(relative(0.86))
            .px(px(13.0))
            .py(px(9.0))
            .rounded(px(14.0))
            .bg(self.palette.selected)
            .text_size(px(13.5))
            .line_height(relative(1.55))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(long, |bubble| {
                bubble.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(message) = this.messages.get_mut(index) {
                        message.expanded = !message.expanded;
                        cx.notify();
                    }
                }))
            })
            .children(
                message
                    .thumb
                    .clone()
                    .map(|picture| img(picture).max_w(px(240.0)).max_h(px(180.0)).rounded(px(8.0))),
            )
            .when(!message.pictures.is_empty() && message.thumb.is_none(), |b| b.child("图片"))
            .when(!message.text.is_empty(), |bubble| {
                bubble.child(
                    div()
                        .when(long && !message.expanded, |text| text.text_ellipsis().line_clamp(6))
                        .child(message.text.clone()),
                )
            });
        div().w_full().flex().justify_end().child(bubble)
    }

    fn reply_row(
        &self,
        index: usize,
        message: &Message,
        last: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = self.palette;
        let streaming = last && self.busy();
        let body = div().w_full().font(theme::serif()).text_size(px(15.0)).line_height(relative(1.7));
        let body = if !message.shots.is_empty() {
            let hwnd = self.hwnd;
            body.children(message.shots.iter().enumerate().map(|(n, shot)| {
                let path = shot.path.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(
                        shot.thumb.clone().map(|thumb| img(thumb).max_w(px(340.0)).max_h(px(260.0)).rounded(px(10.0))),
                    )
                    .child(div().flex().gap(px(4.0)).children({
                        let mut buttons = Vec::new();
                        for (label, action) in [("打开", 0), ("复制", 1), ("文件夹", 2)] {
                            let path = path.clone();
                            buttons.push(
                                div()
                                    .id(("shot", index * 97 + n * 3 + action))
                                    .px(px(9.0))
                                    .py(px(3.0))
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(palette.muted)
                                    .hover(move |style| style.bg(palette.selected))
                                    .child(label)
                                    .on_click(move |_, _, _| match action {
                                        0 => win::shell_open(&path),
                                        1 => win::shell_copy_image(hwnd, &path),
                                        _ => win::shell_select(&path),
                                    })
                                    .into_any_element(),
                            );
                        }
                        buttons
                    }))
                    .into_any_element()
            }))
        } else if message.text.is_empty() {
            body.when(streaming, |body| body.child(waiting_dots(palette)))
        } else {
            body.children(message.view.as_ref().map(|view| TextView::new(view).selectable(true)))
        };
        let copied = self.copied == Some(index);
        let speaking = self.speaking == Some(index);
        let can_speak = self.speech.tts.is_some() && !message.text.is_empty();
        let actions = (!streaming && !message.text.is_empty()).then(|| {
            div()
                .flex()
                .gap(px(2.0))
                .ml(px(-6.0))
                .child(
                    icon_button(
                        ("copy", index),
                        if copied { "icons/check.svg" } else { "icons/copy.svg" },
                        if copied { palette.accent } else { palette.faint },
                        palette.selected,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.copy(index, cx))),
                )
                .when(can_speak, |row| {
                    row.child(
                        icon_button(
                            ("speak", index),
                            "icons/audio-lines.svg",
                            if speaking { palette.accent } else { palette.faint },
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.speak(index, cx))),
                    )
                })
                .when(last, |row| {
                    row.child(
                        icon_button(("retry", index), "icons/refresh-cw.svg", palette.faint, palette.selected)
                            .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    )
                })
        });
        let thoughts = (!message.thoughts.is_empty()).then(|| {
            let thinking = streaming && message.thought_for.is_none();
            let label = match message.thought_for {
                _ if thinking => "正在想".to_string(),
                Some(spent) if spent.as_secs() >= 1 => format!("想了 {} 秒", spent.as_secs()),
                _ => "想了一下".to_string(),
            };
            let open = message.thoughts_open;
            let header = div()
                .id(("thoughts", index))
                .flex()
                .items_center()
                .gap(px(5.0))
                .ml(px(-6.0))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(7.0))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(palette.muted)
                .hover(move |style| style.bg(palette.selected))
                .child(svg().path("icons/lightbulb.svg").size(px(13.0)).text_color(palette.faint))
                .child(label)
                .child(
                    svg()
                        .path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" })
                        .size(px(12.0))
                        .text_color(palette.faint),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(message) = this.messages.get_mut(index) {
                        message.thoughts_open = !message.thoughts_open;
                        cx.notify();
                    }
                }));
            // While it thinks, the newest lines show under the header.
            let shown = if open {
                Some(message.thoughts.trim().to_string())
            } else if thinking {
                let tail: Vec<&str> = message.thoughts.trim().lines().rev().take(2).collect();
                Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
            } else {
                None
            };
            div().flex().flex_col().items_start().gap(px(4.0)).child(header).children(shown.map(|text| {
                div()
                    .w_full()
                    .pl(px(10.0))
                    .border_l_2()
                    .border_color(palette.hairline)
                    .text_size(px(12.5))
                    .line_height(relative(1.6))
                    .text_color(palette.muted)
                    .when(!open, |text| text.line_clamp(2))
                    .child(text)
            }))
        });
        let kit = self.kit.clone();
        let calls = message.calls.iter().enumerate().map(|(n, state)| {
            let label = match &kit {
                Some(kit) => kit.label(&state.call.name),
                None => state.call.name.replace("__", " · "),
            };
            let (status, color) = match state.status {
                CallStatus::Pending | CallStatus::Waiting => ("等你确认", palette.accent),
                CallStatus::Running => ("运行中", palette.muted),
                CallStatus::Done => ("完成", palette.faint),
                CallStatus::Failed => ("失败", palette.accent),
                CallStatus::Denied => ("没有运行", palette.faint),
            };
            let open = state.open || state.status == CallStatus::Waiting;
            let header = div()
                .id(("call", index * 131 + n))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .child(svg().path("icons/wrench.svg").size(px(13.0)).flex_none().text_color(palette.muted))
                .child(div().min_w(px(0.0)).truncate().text_color(palette.ink).child(label))
                .child(div().flex_none().text_color(color).child(status))
                .child(div().flex_1())
                .child(
                    svg()
                        .path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" })
                        .size(px(12.0))
                        .flex_none()
                        .text_color(palette.faint),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(state) = this.messages.get_mut(index).and_then(|m| m.calls.get_mut(n)) {
                        state.open = !state.open;
                        cx.notify();
                    }
                }));
            let block = |text: String| {
                div()
                    .w_full()
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(7.0))
                    .bg(palette.surface)
                    .font_family("Consolas")
                    .text_size(px(11.5))
                    .line_height(relative(1.5))
                    .text_color(palette.muted)
                    .child(text)
            };
            let written = serde_json::from_str::<serde_json::Value>(&state.call.arguments)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .unwrap_or_else(|| state.call.arguments.clone());
            let shown: String = state.result.chars().take(1500).collect();
            let more = state.result.chars().count() > 1500;
            let choice = |id: usize, label: &'static str, primary: bool| {
                div()
                    .id(("decide", (index * 131 + n) * 3 + id))
                    .h(px(26.0))
                    .px(px(11.0))
                    .rounded(px(7.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .when(primary, |button| button.bg(palette.accent).text_color(rgb(0xFFFFFF)))
                    .when(!primary, |button| button.border_1().border_color(palette.hairline).text_color(palette.muted))
                    .hover(|style| style.opacity(0.88))
                    .child(label)
            };
            let asking = (state.status == CallStatus::Waiting).then(|| {
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(choice(0, "允许", true).on_click(cx.listener(move |this, _, _, cx| this.decide(index, n, true, false, cx))))
                    .child(
                        choice(1, "这次对话都允许", false)
                            .on_click(cx.listener(move |this, _, _, cx| this.decide(index, n, true, true, cx))),
                    )
                    .child(choice(2, "拒绝", false).on_click(cx.listener(move |this, _, _, cx| this.decide(index, n, false, false, cx))))
            });
            div()
                .w_full()
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(palette.hairline)
                .bg(palette.raised)
                .font(theme::sans())
                .text_size(px(12.5))
                .line_height(relative(1.4))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(header)
                .when(open, |card| {
                    card.child(block(written))
                        .when(!shown.is_empty(), |card| card.child(block(if more { format!("{shown}…") } else { shown })))
                })
                .children(asking)
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(thoughts)
            .child(body)
            .children(calls)
            .children(actions)
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let streaming = self.busy();
        // The mic appears only when speech recognition is set up and keyed.
        let mic = self
            .speech_route(self.speech.asr.as_ref())
            .map(|_| ((), self.recorder.is_some() || self.transcribing));
        let ready = !self.input.read(cx).value().trim().is_empty()
            || self.messages.last().is_some_and(|m| m.role == Role::User);
        let send = div()
            .id("send")
            .size(px(30.0))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                if this.abort.is_some() { this.stop(cx) } else { this.submit(window, cx) }
            }));
        let send = if streaming {
            send.bg(palette.ink).child(div().size(px(10.0)).rounded(px(2.5)).bg(palette.surface))
        } else {
            let (fill, arrow) =
                if ready { (palette.accent, rgb(0xFFFFFF)) } else { (palette.hairline, palette.faint) };
            send.bg(fill).child(svg().path("icons/arrow-up.svg").size(px(16.0)).text_color(arrow))
        };
        let model = match &self.route {
            Ok(route) => route.model.clone(),
            Err(_) => "选择模型".to_string(),
        };
        let brand = self
            .route
            .as_ref()
            .ok()
            .and_then(|route| crate::brand::model(&route.model, palette.ink))
            .map(|image| crate::brand::badge(image, 14.0));
        let picture = self.picture_mode();
        let (levels, switch_only) = self.reasoning_levels();
        let fitted = self.reasoning.map(|level| catalog::nearest(&levels, level).unwrap_or(level));
        let reasoning = reasoning_label(fitted, switch_only);
        let chips = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                chip_with("model", brand, model, palette, self.sheet == Sheet::Models).on_click(
                    cx.listener(|this, _, window, cx| this.chip_clicked(Sheet::Models, window, cx)),
                ),
            )
            // Picture mode swaps the reasoning chip for ratio and quality.
            .when(picture, |chips| {
                chips
                    .child(
                        chip("ratio", Some("icons/image.svg"), format!("比例 {}", self.ratio.unwrap_or("自动")).into(), palette, self.sheet == Sheet::Ratio)
                            .flex_none()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.chip_clicked(Sheet::Ratio, window, cx);
                            })),
                    )
                    .child(
                        chip(
                            "quality",
                            None,
                            format!("质量 {}", quality_label(self.quality)).into(),
                            palette,
                            self.sheet == Sheet::Quality,
                        )
                        .flex_none()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.chip_clicked(Sheet::Quality, window, cx);
                        })),
                    )
            })
            // Models that do not reason have nothing to set.
            .when(!picture && !levels.is_empty(), |chips| {
                chips.child(
                    chip("reasoning", Some("icons/brain.svg"), reasoning.into(), palette, self.sheet == Sheet::Reasoning)
                        .flex_none()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.chip_clicked(Sheet::Reasoning, window, cx);
                        })),
                )
            });
        div().flex_none().relative().px(px(14.0)).pb(px(14.0)).pt(px(4.0)).children(self.sheet(cx)).child(
            div()
                .rounded(px(18.0))
                .border_1()
                .border_color(palette.hairline)
                .bg(palette.raised)
                .pl(px(6.0))
                .pr(px(7.0))
                .pt(px(5.0))
                .pb(px(7.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .pl(px(2.0))
                        .text_size(px(13.5))
                        .child(Textarea::new(&self.input).appearance(false)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(chips)
                        .children(mic.map(|mic| {
                            let seconds = self.recorder.as_ref().map(|r| r.started.elapsed().as_secs());
                            let label = if self.transcribing {
                                Some("转写…".to_string())
                            } else {
                                seconds.map(|s| format!("{s}″"))
                            };
                            div()
                                .id("mic")
                                .h(px(26.0))
                                .px(px(6.0))
                                .flex_none()
                                .rounded(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .cursor_pointer()
                                .hover(move |style| style.bg(palette.selected))
                                .child(
                                    svg()
                                        .path("icons/mic.svg")
                                        .size(px(15.0))
                                        .flex_none()
                                        .text_color(if mic.1 { palette.accent } else { palette.faint }),
                                )
                                .children(label.map(|text| {
                                    div().text_size(px(11.0)).text_color(palette.accent).child(text)
                                }))
                                .on_click(cx.listener(|this, _, window, cx| this.dictate(window, cx)))
                        }))
                        .child(send),
                ),
        )
    }
}

impl Drop for ChatView {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        if let Some(text) = self.dictated.take() {
            self.input.update(cx, |input, cx| {
                let mut value = input.value().to_string();
                value.push_str(&text);
                input.set_value(value, window, cx);
            });
        }
        let last = self.messages.len().saturating_sub(1);
        let rows: Vec<_> = self
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| match message.role {
                Role::User => self.user_row(index, message, cx).into_any_element(),
                Role::Assistant => self.reply_row(index, message, index == last, cx).into_any_element(),
            })
            .collect();
        let error = self.error.clone().map(|reason| {
            div()
                .px(px(13.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .bg(palette.accent_wash)
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(div().flex_1().min_w(px(0.0)).child(reason))
                .child(
                    div()
                        .id("retry-error")
                        .flex_none()
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .text_color(palette.accent)
                        .font_weight(FontWeight::MEDIUM)
                        .hover(move |style| style.bg(palette.selected))
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))
                        .child("重试"),
                )
        });
        let list = div()
            .id("messages")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, _| {
                // Scrolling back stops the view from following the reply;
                // returning to the end resumes it.
                if event.delta.pixel_delta(px(20.0)).y > px(0.0) {
                    this.follow = false;
                } else if this.scroll.offset().y <= px(48.0) - this.scroll.max_offset().y {
                    this.follow = true;
                }
            }))
            .px(px(20.0))
            .pt(px(4.0))
            .pb(px(14.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .children(rows)
            .children(error);
        div()
            .size_full()
            .flex()
            .flex_col()
            // A fresh press forgets which sheet the previous one closed.
            .capture_any_mouse_down(cx.listener(|this, _, _, _| this.dismissed = Sheet::None))
            .bg(palette.surface)
            .text_color(palette.ink)
            .font(theme::sans())
            .text_size(px(13.0))
            .child(self.header(cx))
            .child(list)
            .child(self.composer(cx))
    }
}

fn icon_button(id: impl Into<ElementId>, icon: &'static str, color: Rgba, hover: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .rounded(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(svg().path(icon).size(px(15.0)).text_color(color))
}

fn reasoning_label(level: Option<Reasoning>, switch_only: bool) -> &'static str {
    match level {
        None => "默认",
        Some(Reasoning::None) => "关闭",
        Some(_) if switch_only => "开启",
        Some(Reasoning::Minimal) => "极低",
        Some(Reasoning::Low) => "低",
        Some(Reasoning::Medium) => "中",
        Some(Reasoning::High) => "高",
        Some(Reasoning::Xhigh) => "很高",
        Some(Reasoning::Max) => "最高",
    }
}

/// A small button under the input box that opens a sheet.
fn chip(
    id: &'static str,
    icon: Option<&'static str>,
    label: String,
    palette: Palette,
    open: bool,
) -> Stateful<Div> {
    let icon = icon.map(|icon| {
        svg().path(icon).size(px(13.0)).flex_none().text_color(palette.muted).into_any_element()
    });
    chip_with(id, icon, label, palette, open)
}

/// A chip led by any small element, such as a brand icon.
fn chip_with(
    id: &'static str,
    lead: Option<gpui_kit::AnyElement>,
    label: String,
    palette: Palette,
    open: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(26.0))
        .min_w(px(0.0))
        .px(px(8.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .cursor_pointer()
        .text_size(px(11.5))
        .text_color(palette.muted)
        .when(open, |chip| chip.bg(palette.selected))
        .hover(move |style| style.bg(palette.selected))
        .children(lead)
        .child(div().min_w(px(0.0)).truncate().child(label))
        .child(svg().path("icons/chevron-down.svg").size(px(12.0)).flex_none().text_color(palette.faint))
}

fn sheet_row(id: impl Into<ElementId>, palette: Palette, picked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.0))
        .flex_none()
        .px(px(9.0))
        .rounded(px(7.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .when(picked, |row| row.bg(palette.selected))
        .hover(move |style| style.bg(palette.selected))
}

/// Three dots that take turns lighting up while the reply has not started.
fn waiting_dots(palette: Palette) -> impl IntoElement {
    div().h(px(24.0)).flex().items_center().gap(px(5.0)).children((0..3usize).map(|dot| {
        div().size(px(6.0)).rounded_full().bg(palette.muted).with_animation(
            ("waiting", dot),
            Animation::new(Duration::from_millis(1100)).repeat(),
            move |element, progress| {
                let phase = (progress - dot as f32 * 0.16).rem_euclid(1.0);
                let level = (phase * std::f32::consts::TAU).cos() * 0.5 + 0.5;
                element.opacity(0.22 + 0.78 * level)
            },
        )
    }))
}

/// What the 质量 chip reads.
fn quality_label(quality: Option<&str>) -> &'static str {
    match quality {
        Some("low") => "低",
        Some("medium") => "中",
        Some("high") => "高",
        _ => "自动",
    }
}

/// How long a WAV file plays, from its header; three seconds when unreadable.
fn wav_seconds(wav: &[u8]) -> f32 {
    if wav.len() < 44 || &wav[..4] != b"RIFF" {
        return 3.0;
    }
    let channels = u16::from_le_bytes(wav[22..24].try_into().unwrap()) as u32;
    let rate = u32::from_le_bytes(wav[24..28].try_into().unwrap());
    let bits = u16::from_le_bytes(wav[34..36].try_into().unwrap()) as u32;
    let bytes_per_second = rate * channels * bits / 8;
    if bytes_per_second == 0 {
        return 3.0;
    }
    (wav.len() - 44) as f32 / bytes_per_second as f32
}

/// A generated picture for the reply, when gpui can read its format.
fn gpui_image(bytes: &[u8]) -> Option<Arc<gpui_kit::Image>> {
    let format = match image::extension(bytes) {
        "png" => gpui_kit::ImageFormat::Png,
        "jpg" | "jpeg" => gpui_kit::ImageFormat::Jpeg,
        "webp" => gpui_kit::ImageFormat::Webp,
        "gif" => gpui_kit::ImageFormat::Gif,
        "bmp" => gpui_kit::ImageFormat::Bmp,
        _ => return None,
    };
    Some(Arc::new(gpui_kit::Image::from_bytes(format, bytes.to_vec())))
}

/// The captured content as the opening user message, with the picture to show.
fn first_turn(content: Content) -> (Turn, Option<Arc<gpui_kit::Image>>) {
    match content {
        Content::Text(text) => (Turn::user(text), None),
        Content::Files(files) => {
            let list: Vec<String> = files.iter().map(|path| path.display().to_string()).collect();
            (Turn::user(list.join("\n")), None)
        }
        Content::Image(image) => {
            let (mime, thumb) = match image.format {
                ImageFormat::Png => (
                    "image/png",
                    Some(Arc::new(gpui_kit::Image::from_bytes(
                        gpui_kit::ImageFormat::Png,
                        image.bytes.clone(),
                    ))),
                ),
                ImageFormat::Dib => ("image/bmp", None),
            };
            let picture = Picture { mime: mime.into(), base64: win::base64(&image.bytes) };
            (Turn { pictures: vec![picture], ..Turn::user("") }, thumb)
        }
    }
}
