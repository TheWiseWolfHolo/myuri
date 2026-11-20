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
use myuri_core::config::{
    self, Assistant, ImageProvider, ModelRef, Provider, Purpose, Reasoning, SpeechApi,
    SpeechProvider, SpeechSettings,
};
use myuri_core::edit::{self, Field};
use myuri_core::image::{self, ImageRequest, ImageSpec, ReferenceImage};
use myuri_core::speech::Voice;

use crate::images::{self, ImageOptions};
use crate::secrets;
use crate::speech;
use myuri_core::gesture::Pos;
use myuri_core::history::{self, Conversation, Entry, SavedAudio, SavedCall};
use myuri_core::llm::{Event, Picture, Role, ToolCall, ToolResult, Turn};

use crate::mcp::Kit;

use crate::applog::info;
use crate::capture::{Content, ImageFormat};
use crate::chat::Route;
use crate::icon::Glyph;
use crate::net::Abort;
use crate::picker::{DefaultScope, ModelPicker, PickerDefaults, PickerEvent};
use crate::theme::{self, Palette};
use crate::{panel, win};

// Keep snapshots in the order the UI produced them, including tool rounds
// that finish while a previous snapshot is still being written.
static HISTORY_WRITER: std::sync::LazyLock<std::sync::mpsc::Sender<(PathBuf, Conversation)>> =
    std::sync::LazyLock::new(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, Conversation)>();
        std::thread::spawn(move || {
            for (dir, conversation) in rx {
                if let Err(error) = history::save(&dir, &conversation) {
                    info!("history: cannot save: {error}");
                }
            }
        });
        tx
    });

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
    pub image_providers: Vec<ImageProvider>,
    pub speech_providers: Vec<SpeechProvider>,
    pub favorites: Vec<ModelRef>,
    pub image_favorites: Vec<ModelRef>,
    /// `[models] default` and `[image] default` from config.toml.
    pub default_chat: Option<ModelRef>,
    pub default_image: Option<ModelRef>,
    /// `[speech]` from config.toml: which provider/model listens and speaks.
    pub speech: SpeechSettings,
    pub catalog: Arc<Catalog>,
    /// A saved conversation to continue.
    pub resume: Option<Conversation>,
    pub resume_mode: Option<(Purpose, ModelRef)>,
}

/// What is open above the input box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sheet {
    None,
    Models,
    Reasoning,
    Image(ImageParameter),
    Voice,
    Speed,
    Stability,
    Style,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageParameter {
    Size,
    Resolution,
    Quality,
    Format,
}

#[derive(Clone, Default)]
enum Mode {
    #[default]
    Chat,
    Image {
        provider_id: String,
        model: String,
    },
    Speech {
        provider_id: String,
        model: String,
    },
}

impl ImageOptions {
    fn get(&self, parameter: ImageParameter) -> Option<&'static str> {
        match parameter {
            ImageParameter::Size => self.size,
            ImageParameter::Resolution => self.resolution,
            ImageParameter::Quality => self.quality,
            ImageParameter::Format => self.format,
        }
    }

    fn set(&mut self, parameter: ImageParameter, value: &'static str) {
        let slot = match parameter {
            ImageParameter::Size => &mut self.size,
            ImageParameter::Resolution => &mut self.resolution,
            ImageParameter::Quality => &mut self.quality,
            ImageParameter::Format => &mut self.format,
        };
        *slot = Some(value);
    }
}

impl ImageParameter {
    fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::Size => ("image-size", "画幅"),
            Self::Resolution => ("image-resolution", "分辨率"),
            Self::Quality => ("image-quality", "质量"),
            Self::Format => ("image-format", "格式"),
        }
    }

    fn choices(self, spec: ImageSpec) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::Size => spec.sizes.to_vec(),
            Self::Quality => spec.qualities.to_vec(),
            Self::Resolution => spec
                .resolutions
                .iter()
                .map(|&value| (value, value))
                .collect(),
            Self::Format => spec.formats.iter().map(|&value| (value, value)).collect(),
        }
    }
}

struct Attachment {
    image: ReferenceImage,
    thumb: Option<Arc<gpui_kit::Image>>,
}

struct AudioCard {
    path: PathBuf,
    duration: Duration,
    summary: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Playing {
    Reply(usize),
    Clip(usize),
}

enum Reading {
    Saved(PathBuf),
    Wav(Vec<u8>, Duration),
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
        matches!(
            self.status,
            CallStatus::Done | CallStatus::Failed | CallStatus::Denied
        )
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
    audio: Option<AudioCard>,
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
            && self.audio.is_none()
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
    mode: Mode,
    /// For this conversation; starts as the assistant's setting.
    reasoning: Option<Reasoning>,
    providers: Vec<Provider>,
    image_providers: Vec<ImageProvider>,
    speech_providers: Vec<SpeechProvider>,
    catalog: Arc<Catalog>,
    picker: Entity<ModelPicker>,
    sheet: Sheet,
    /// The sheet a mouse press just closed, so the same click on its button
    /// does not open it again.
    dismissed: Sheet,
    palette: Palette,
    messages: Vec<Message>,
    input: Entity<TextareaState>,
    editing: Option<usize>,
    edit_input: Entity<TextareaState>,
    scroll: ScrollHandle,
    /// Keep the newest text in view while it streams.
    follow: bool,
    /// The request in flight.
    abort: Option<Abort>,
    /// Counts requests, so events from a stopped one are recognized.
    generation: u64,
    image_options: ImageOptions,
    attachments: Vec<Attachment>,
    captured_reference: Option<ReferenceImage>,
    pending_reference: Option<ReferenceImage>,
    last_image_model: Option<ModelRef>,
    default_chat: Option<ModelRef>,
    default_image: Option<ModelRef>,
    default_speech: Option<ModelRef>,
    tts_options: speech::TtsOptions,
    voices: Vec<Voice>,
    voice_loading: bool,
    voice_error: Option<String>,
    voice_generation: u64,
    error: Option<String>,
    notice: Option<String>,
    notice_generation: u64,
    /// The user pinned the window: losing focus no longer closes it.
    keep_open: bool,
    /// Whether the window has had the keyboard yet.
    activated: bool,
    /// The reply whose copy button was just used.
    copied: Option<usize>,
    /// Complete routes, checked once when the window opens.
    asr: Option<(SpeechProvider, String)>,
    tts: Option<(SpeechProvider, String)>,
    /// The message being read out, and a counter against late results.
    speaking: Option<Playing>,
    speak_generation: u64,
    /// Reply index currently exporting TTS audio to disk.
    saving_speech: Option<usize>,
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
    if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| ChatView::new(opening, window, cx))
    }) {
        info!("window: cannot open \"{name}\": {error}");
    }
}

impl ChatView {
    fn new(opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hwnd = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .placeholder("继续问")
        });
        let edit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .placeholder("编辑消息内容")
        });
        let catalog = opening.catalog;
        let palette = theme::palette(opening.dark);
        let default_chat = opening.default_chat;
        let default_image = opening.default_image.filter(|reference| {
            opening
                .image_providers
                .iter()
                .any(|provider| provider.enabled && provider.id == reference.provider)
        });
        let default_speech = opening
            .speech
            .tts
            .as_deref()
            .and_then(ModelRef::parse);
        let initial_defaults = build_picker_defaults(
            &opening.assistant,
            default_chat.as_ref(),
            default_image.as_ref(),
            default_speech.as_ref(),
            opening.route.as_ref().ok(),
        );
        let picker = cx.new(|cx| {
            ModelPicker::new(
                opening.providers.clone(),
                opening.image_providers.clone(),
                opening.speech_providers.clone(),
                opening.favorites.clone(),
                opening.image_favorites.clone(),
                catalog.clone(),
                Purpose::Chat,
                palette,
                window,
                cx,
            )
            .with_defaults(initial_defaults)
        });
        let handle = Window::window_handle(window);
        let view = cx.weak_entity();
        let subscriptions = vec![
            cx.intercept_keystrokes(move |event, window, cx| {
                let keystroke = &event.keystroke;
                if Window::window_handle(window) != handle
                    || !keystroke.modifiers.control
                    || keystroke.modifiers.number_of_modifiers() != 1
                {
                    return;
                }
                let Some(view) = view.upgrade() else { return };
                match keystroke.key.as_str() {
                    "v" => {
                        view.update(cx, |this, cx| {
                            if this.sheet != Sheet::None
                                || this.editing.is_some()
                                || !this.image_spec().is_some_and(|spec| spec.supports_edit)
                            {
                                return;
                            }
                            if let Some(image) =
                                crate::clipboard::read_image(HWND(this.hwnd as *mut _))
                                    .and_then(|image| ReferenceImage::from_bytes(image.bytes))
                            {
                                this.attach(image);
                                cx.stop_propagation();
                                cx.notify();
                            }
                        });
                    }
                    "m" => {
                        view.update(cx, |this, cx| {
                            if this.sheet == Sheet::Models {
                                this.close_sheet(window, cx);
                            } else {
                                this.chip_clicked(Sheet::Models, window, cx);
                            }
                            cx.stop_propagation();
                        });
                    }
                    "enter" => {
                        view.update(cx, |this, cx| {
                            if let Some(index) = this.editing {
                                let is_user = this
                                    .messages
                                    .get(index)
                                    .is_some_and(|m| m.role == Role::User);
                                this.save_edit(is_user, window, cx);
                                cx.stop_propagation();
                            }
                        });
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(
                &picker,
                window,
                |this, _, event: &PickerEvent, window, cx| match event {
                    PickerEvent::Chosen(purpose, reference) => {
                        // Allow switching models during an active request without
                        // interrupting it; the new selection applies to the next turn.
                        this.choose_model(*purpose, reference, window, cx);
                        this.close_sheet(window, cx);
                    }
                    PickerEvent::SetDefault {
                        purpose,
                        model,
                        scope,
                    } => {
                        this.set_default(*purpose, model, *scope, window, cx);
                    }
                    PickerEvent::ToggleFavorite(purpose, model) => {
                        this.toggle_favorite(*purpose, model, cx);
                    }
                    PickerEvent::Dismissed => this.close_sheet(window, cx),
                },
            ),
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    // Enter ends a dictation; the second Enter sends the message.
                    InputEvent::PressEnter { shift: false, .. } if this.recorder.is_some() => {
                        this.finish_recording(window, cx)
                    }
                    InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
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
                if keystroke.key.as_str() != "escape" {
                    return;
                }
                if this.editing.is_some() {
                    this.cancel_edit(window, cx);
                } else if this.sheet != Sheet::None {
                    this.close_sheet(window, cx);
                } else if this.busy() {
                    this.stop(cx);
                } else {
                    window.remove_window();
                }
            }),
        ];
        let asr = speech_route(
            &opening.speech_providers,
            opening.speech.asr.as_deref(),
        );
        let tts = speech_route(
            &opening.speech_providers,
            opening.speech.tts.as_deref(),
        );
        let last_image_model = default_image.clone();
        let mut view = Self {
            handle,
            hwnd,
            glyph: Glyph::of(&opening.assistant),
            reasoning: opening.assistant.reasoning,
            assistant: opening.assistant,
            system: opening.system,
            route: opening.route,
            mode: Mode::Chat,
            providers: opening.providers,
            image_providers: opening.image_providers,
            speech_providers: opening.speech_providers,
            catalog,
            picker,
            sheet: Sheet::None,
            dismissed: Sheet::None,
            palette,
            messages: Vec::new(),
            input,
            editing: None,
            edit_input,
            scroll: ScrollHandle::new(),
            follow: true,
            abort: None,
            generation: 0,
            image_options: ImageOptions::default(),
            attachments: Vec::new(),
            captured_reference: None,
            pending_reference: None,
            last_image_model,
            default_chat,
            default_image,
            default_speech,
            tts_options: speech::TtsOptions::default(),
            voices: Vec::new(),
            voice_loading: false,
            voice_error: None,
            voice_generation: 0,
            error: None,
            notice: None,
            notice_generation: 0,
            keep_open: false,
            activated: false,
            copied: None,
            asr,
            tts,
            speaking: None,
            speak_generation: 0,
            saving_speech: None,
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
        if let Some((purpose, model)) = opening.resume_mode {
            view.choose_model(purpose, &model, window, cx);
        }
        if let Some(content) = opening.content {
            let kind = content.kind();
            if let Content::Image(image) = &content {
                view.captured_reference = ReferenceImage::from_bytes(image.bytes.clone());
            }
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
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                anchor.x,
                anchor.y,
                0,
                0,
                quiet | SWP_NOSIZE,
            );
            let scale = win::scale_at(anchor);
            let want = (
                (prefs.width as f32 * scale).round() as i32,
                (prefs.height as f32 * scale).round() as i32,
            );
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                0,
                0,
                want.0,
                want.1,
                quiet | SWP_NOMOVE,
            );
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
            let above = if prefs.pinned {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            };
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
            audio: None,
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
            let role = if entry.role == "user" {
                Role::User
            } else {
                Role::Assistant
            };
            let pictures: Vec<Picture> = entry
                .pictures
                .into_iter()
                .map(|(mime, base64)| Picture { mime, base64 })
                .collect();
            let thumb = pictures.first().and_then(|picture| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&picture.base64)
                    .ok()?;
                gpui_image(&bytes)
            });
            let turn = match role {
                Role::User => Turn {
                    pictures,
                    ..Turn::user(entry.text)
                },
                Role::Assistant => Turn::assistant(entry.text),
            };
            self.push(turn, thumb, cx);
            if let Some(message) = self.messages.last_mut() {
                message.calls = entry
                    .calls
                    .into_iter()
                    .map(|saved| CallState {
                        status: if saved.failed {
                            CallStatus::Failed
                        } else {
                            CallStatus::Done
                        },
                        call: ToolCall {
                            id: saved.id,
                            name: saved.name,
                            arguments: saved.arguments,
                            echo: saved.echo,
                        },
                        result: saved.result,
                        open: false,
                    })
                    .collect();
                message.thoughts = entry.thoughts;
                message.shots = entry
                    .shots
                    .into_iter()
                    .map(PathBuf::from)
                    .map(|path| Shot {
                        thumb: std::fs::read(&path)
                            .ok()
                            .and_then(|bytes| gpui_image(&bytes)),
                        path,
                    })
                    .collect();
                message.audio = entry.audio.map(|saved| AudioCard {
                    path: PathBuf::from(saved.path),
                    duration: Duration::from_millis(saved.duration_ms),
                    summary: saved.summary,
                });
            }
        }
    }

    /// Save the conversation as it stands, off the UI thread.
    fn remember(&self) {
        let answered = self.messages.iter().any(|m| m.role == Role::Assistant);
        if !answered {
            return;
        }
        let (purpose, model) = self.model_reference().map_or((None, String::new()), |(purpose, reference)| {
            (Some(purpose), format!("{}/{}", reference.provider, reference.model))
        });
        let conversation = Conversation {
            id: self.conversation.clone(),
            assistant: self.assistant.name.clone(),
            model,
            purpose,
            created: self.created,
            updated: crate::historyview::now(),
            messages: self
                .messages
                .iter()
                .map(|m| Entry {
                    role: if m.role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    }
                    .into(),
                    text: m.text.clone(),
                    thoughts: m.thoughts.clone(),
                    pictures: m
                        .pictures
                        .iter()
                        .map(|p| (p.mime.clone(), p.base64.clone()))
                        .collect(),
                    shots: m
                        .shots
                        .iter()
                        .map(|shot| shot.path.display().to_string())
                        .collect(),
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
                    audio: m.audio.as_ref().map(|card| SavedAudio {
                        path: card.path.display().to_string(),
                        duration_ms: card.duration.as_millis().min(u128::from(u64::MAX)) as u64,
                        summary: card.summary.clone(),
                    }),
                })
                .collect(),
        };
        if HISTORY_WRITER.send((crate::config_dir(), conversation)).is_err() {
            info!("history: could not queue save");
        }
    }

    /// Enter in the input box, or the send button.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.editing = None;
        let text = self.input.read(cx).value().trim().to_string();
        // Enter itself lands in the box as a line break; clear that too.
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Content that was waiting for a question goes out with it.
        match self
            .messages
            .last_mut()
            .filter(|last| last.role == Role::User)
        {
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

    fn model_reference(&self) -> Option<(Purpose, ModelRef)> {
        let (purpose, provider, model) = match &self.mode {
            Mode::Chat => {
                let route = self.route.as_ref().ok()?;
                (Purpose::Chat, &route.provider_id, &route.model)
            }
            Mode::Image { provider_id, model } => (Purpose::Images, provider_id, model),
            Mode::Speech { provider_id, model } => (Purpose::Speech, provider_id, model),
        };
        Some((
            purpose,
            ModelRef {
                provider: provider.clone(),
                model: model.clone(),
            },
        ))
    }

    fn image_route(&self) -> Option<(&ImageProvider, &str)> {
        let Mode::Image { provider_id, model } = &self.mode else {
            return None;
        };
        let provider = self
            .image_providers
            .iter()
            .find(|provider| provider.id == *provider_id && provider.enabled)?;
        Some((provider, model))
    }

    fn direct_speech_route(&self) -> Option<(&SpeechProvider, &str)> {
        let Mode::Speech { provider_id, model } = &self.mode else {
            return None;
        };
        let provider = self
            .speech_providers
            .iter()
            .find(|provider| provider.id == *provider_id && provider.enabled)?;
        Some((provider, model))
    }

    fn image_spec(&self) -> Option<ImageSpec> {
        let (provider, model) = self.image_route()?;
        Some(ImageSpec::for_model(provider.api, model))
    }

    fn choose_model(
        &mut self,
        purpose: Purpose,
        reference: &ModelRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        self.voice_generation += 1;
        self.voice_loading = false;
        self.voice_error = None;
        let ModelRef { provider, model } = reference.clone();
        let placeholder = match purpose {
            Purpose::Chat => {
                self.mode = Mode::Chat;
                self.route = Route::to(&self.providers, reference);
                self.error = self.route.as_ref().err().cloned();
                "继续问"
            }
            Purpose::Images => {
                self.mode = Mode::Image {
                    provider_id: provider,
                    model,
                };
                if let Some((provider, model)) = self.image_route() {
                    let spec = ImageSpec::for_model(provider.api, model);
                    let is_xai = provider.api == image::ImageApi::Xai;
                    self.image_options = ImageOptions::new(provider, spec);
                    if is_xai {
                        self.attachments.truncate(5);
                    }
                    if spec.supports_edit {
                        self.last_image_model = Some(reference.clone());
                        if let Some(image) = self.pending_reference.take() {
                            self.attachments.clear();
                            self.attach(image);
                        } else if let Some(image) = self.captured_reference.take() {
                            self.attach(image);
                        }
                    } else {
                        self.pending_reference = None;
                    }
                }
                "描述要生成或修改的图片"
            }
            Purpose::Speech => {
                self.mode = Mode::Speech {
                    provider_id: provider,
                    model,
                };
                self.voices.clear();
                if let Some((provider, _)) = self.direct_speech_route() {
                    let provider = provider.clone();
                    self.tts_options =
                        speech::tts_options(&provider, &speech::TtsOptions::default());
                    if provider.api == SpeechApi::Elevenlabs {
                        self.fetch_voices(provider, cx);
                    }
                }
                "输入要合成语音的文字"
            }
        };
        self.input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
    }

    fn attach(&mut self, image: ReferenceImage) {
        if !self
            .attachments
            .iter()
            .any(|attachment| attachment.image == image)
        {
            if self
                .image_route()
                .is_some_and(|(provider, _)| provider.api == image::ImageApi::Xai)
                && self.attachments.len() >= 5
            {
                self.attachments.remove(0);
            }
            self.attachments.push(Attachment {
                thumb: gpui_image(&image.bytes),
                image,
            });
        }
    }

    fn edit_shot(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let image = std::fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                ReferenceImage::from_bytes(bytes).ok_or_else(|| "参考图格式无法识别".into())
            });
        match image {
            Ok(image) => {
                if !self.image_spec().is_some_and(|spec| spec.supports_edit) {
                    if let Some(reference) = self.last_image_model.clone() {
                        self.choose_model(Purpose::Images, &reference, window, cx);
                    } else {
                        self.pending_reference = Some(image);
                        self.sheet = Sheet::Models;
                        self.picker.update(cx, |picker, cx| {
                            picker.set_purpose(Purpose::Images, window, cx)
                        });
                        return cx.notify();
                    }
                }
                self.attachments.clear();
                self.attach(image);
                self.input.update(cx, |input, cx| input.focus(window, cx));
            }
            Err(reason) => self.error = Some(reason),
        }
        cx.notify();
    }

    /// A chat or image request is in flight.
    fn busy(&self) -> bool {
        self.abort.is_some() || self.kit_loading || self.tools_open()
    }

    /// The newest reply has tool calls that are not finished.
    fn tools_open(&self) -> bool {
        self.messages.last().is_some_and(|m| m.calls.iter().any(|state| !state.settled()))
    }

    /// Ask for a reply to the conversation so far.
    fn request(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.follow = true;
        match &self.mode {
            Mode::Image { .. } => return self.generate(cx),
            Mode::Speech { .. } => return self.synthesize(cx),
            Mode::Chat => {}
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
                    let problems = kit.problems.join("；");
                    this.kit = Some(Arc::new(kit));
                    if this.generation == generation {
                        this.kit_loading = false;
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
        let is_media_reply = |message: &Message| {
            message.role == Role::Assistant
                && (message.audio.is_some() || !message.shots.is_empty())
                && message.text.is_empty()
        };
        for (index, m) in answered.iter().enumerate() {
            if is_media_reply(m)
                || (m.role == Role::User
                    && answered.get(index + 1).is_some_and(is_media_reply))
            {
                continue;
            }
            // After individual message deletions, skip leading orphan assistant
            // turns and merge consecutive plain same-role turns so strict APIs
            // (Anthropic / Gemini) always receive alternating roles.
            if turns.is_empty() && m.role == Role::Assistant && m.calls.is_empty() {
                continue;
            }
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
            if calls.is_empty()
                && let Some(prev) = turns
                    .last_mut()
                    .filter(|prev| prev.role == m.role && prev.calls.is_empty() && prev.results.is_empty())
            {
                if !prev.text.is_empty() && !m.text.is_empty() {
                    prev.text.push_str("\n\n");
                }
                prev.text.push_str(&m.text);
                prev.pictures.extend(m.pictures.iter().cloned());
                continue;
            }
            turns.push(Turn {
                calls,
                ..Turn {
                    role: m.role,
                    text: m.text.clone(),
                    pictures: m.pictures.clone(),
                    calls: Vec::new(),
                    results: Vec::new(),
                }
            });
            if !results.is_empty() {
                turns.push(Turn {
                    results,
                    ..Turn::user("")
                });
            }
        }
        let (tx, rx) = async_channel::unbounded();
        let Ok(route) = &self.route else { return };
        let info = self.catalog.lookup(&route.model);
        let (reasoning, temperature) = (self.reasoning, self.assistant.temperature);
        let kit = self.kit.clone().unwrap_or_default();
        let system = format!("{}{}", self.system, kit.prompt);
        self.abort = Some(route.send(
            &system,
            &turns,
            reasoning,
            temperature,
            info,
            &kit.tools,
            move |event| {
                let _ = tx.try_send(event);
            },
        ));
        cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                // Take everything that has arrived, so a burst costs one frame.
                let mut events = vec![first];
                while let Ok(more) = rx.try_recv() {
                    events.push(more);
                }
                if this
                    .update(cx, |this, cx| this.on_events(generation, events, cx))
                    .is_err()
                {
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
                        reply.calls.push(CallState {
                            call,
                            status: CallStatus::Pending,
                            result: String::new(),
                            open: false,
                        });
                    }
                }
                Event::Done => {
                    self.abort = None;
                    if let Some(reply) = self.messages.last_mut()
                        && reply.thought_for.is_none()
                    {
                        reply.thought_for = reply.thinking_since.map(|since| since.elapsed());
                    }
                    self.drop_empty_reply();
                    self.remember();
                    // The model asked for tools: run them, then ask again.
                    self.advance(cx);
                }
                Event::Error(reason) => {
                    self.abort = None;
                    self.error = Some(reason);
                    for state in self
                        .messages
                        .last_mut()
                        .into_iter()
                        .flat_map(|message| &mut message.calls)
                    {
                        if !state.settled() {
                            state.status = CallStatus::Failed;
                            state.result = "The request failed before the tool completed.".into();
                        }
                    }
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
        let Some((provider, model)) = self.image_route() else {
            self.error = Some("选择生图模型".into());
            return cx.notify();
        };
        if prompt.is_empty() {
            self.error = Some("输入生图提示词".into());
            return cx.notify();
        }
        let references = if ImageSpec::for_model(provider.api, model).supports_edit {
            self.attachments
                .iter()
                .map(|attachment| attachment.image.clone())
                .collect()
        } else {
            Vec::new()
        };
        let (provider, model) = (provider.clone(), model.to_string());
        self.push(Turn::assistant(""), None, cx);
        self.generation += 1;
        let generation = self.generation;
        let abort = Abort::new();
        self.abort = Some(abort.clone());
        let ImageOptions {
            size,
            resolution,
            quality,
            format,
        } = self.image_options;
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let request = ImageRequest {
                model: &model,
                prompt: &prompt,
                size,
                resolution,
                quality,
                format,
                references: &references,
            };
            let _ = tx.send_blocking(images::generate_with_abort(&provider, &request, &abort));
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
    fn on_generated(
        &mut self,
        generation: u64,
        result: Result<Vec<Vec<u8>>, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.abort = None;
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
                        Ok(path) => shots.push(Shot {
                            path,
                            thumb: gpui_image(bytes),
                        }),
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

    fn synthesize(&mut self, cx: &mut Context<Self>) {
        let Some((provider, model)) = self.direct_speech_route() else {
            self.error = Some("选择语音模型".into());
            return cx.notify();
        };
        let Some(text) = self
            .messages
            .last()
            .filter(|message| message.role == Role::User && !message.text.is_empty())
            .map(|message| message.text.clone())
        else {
            return;
        };
        let (provider, model) = (provider.clone(), model.to_string());
        let (options, voice) = (self.tts_options.clone(), self.voice_label());
        self.push(Turn::assistant(""), None, cx);
        self.generation += 1;
        let generation = self.generation;
        let abort = Abort::new();
        self.abort = Some(abort.clone());
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result =
                speech::tts_with_abort(&provider, &model, &text, &options, &abort).and_then(
                    |clip| {
                        if abort.is_stopped() {
                            return Err("请求已停止".into());
                        }
                        let path = speech::save(&speech::folder(), &clip.bytes, clip.ext)?;
                        let duration = speech::duration_saved(&path)?;
                        let summary = voice_summary(&voice, provider.api, &options);
                        Ok(AudioCard {
                            path,
                            duration,
                            summary,
                        })
                    },
                );
            let _ = tx.send_blocking(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.abort = None;
                match result {
                    Ok(audio) => {
                        if let Some(reply) = this.messages.last_mut() {
                            reply.audio = Some(audio);
                        }
                        this.remember();
                    }
                    Err(reason) => {
                        this.error = Some(reason);
                        this.drop_empty_reply();
                    }
                }
                if this.follow {
                    this.scroll.scroll_to_bottom();
                }
                cx.notify();
            });
        })
        .detach();
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
        let Some(index) = self.messages.len().checked_sub(1) else {
            return;
        };
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
                && !kit
                    .server(&name)
                    .is_some_and(|server| self.trusted.contains(server));
            if asks {
                self.messages[index].calls[n].status = CallStatus::Waiting;
            } else {
                self.run_call(index, n, cx);
            }
        }
        if self.messages[index].calls.iter().all(CallState::settled) && self.abort.is_none() {
            self.remember();
            if !matches!(self.mode, Mode::Chat) {
                return cx.notify();
            }
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
                let Some(state) = this
                    .messages
                    .get_mut(index)
                    .and_then(|m| m.calls.get_mut(n))
                else {
                    return;
                };
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
    fn decide(
        &mut self,
        index: usize,
        n: usize,
        allow: bool,
        always: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self
            .messages
            .get_mut(index)
            .and_then(|m| m.calls.get_mut(n))
        else {
            return;
        };
        if state.status != CallStatus::Waiting {
            return;
        }
        if !allow {
            state.status = CallStatus::Denied;
            state.result = "The user declined this tool call.".into();
        } else if always {
            let name = state.call.name.clone();
            let server = self
                .kit
                .as_ref()
                .and_then(|kit| kit.server(&name).map(str::to_string));
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
        self.kit_loading = false;
        // Whatever the stopped request still delivers belongs to nobody.
        self.generation += 1;
        // Every call needs an answer before the conversation can go on.
        for state in self
            .messages
            .last_mut()
            .into_iter()
            .flat_map(|m| &mut m.calls)
        {
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
        if self.busy() {
            return;
        }
        // Tool rounds make several replies to one question; redo them all.
        while self
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Assistant)
        {
            self.messages.pop();
        }
        if !self.messages.is_empty() {
            self.editing = None;
            self.rounds = 0;
            self.request(cx);
        }
    }

    fn retry_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy() || index >= self.messages.len() {
            return;
        }
        self.stop_playing();
        self.editing = None;
        let cut = if self.messages[index].role == Role::User {
            index + 1
        } else {
            let mut cut = index;
            while cut > 0 && self.messages[cut - 1].role == Role::Assistant {
                cut -= 1;
            }
            cut
        };
        self.messages.truncate(cut);
        if self.messages.last().is_some_and(|m| m.role == Role::User) {
            self.rounds = 0;
            self.request(cx);
        } else {
            self.remember();
            cx.notify();
        }
    }

    fn delete_message(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy() || index >= self.messages.len() {
            return;
        }
        self.stop_playing();
        if self.editing == Some(index) {
            self.editing = None;
        } else if let Some(editing) = self.editing
            && editing > index
        {
            self.editing = Some(editing - 1);
        }
        self.messages.remove(index);
        self.error = None;
        self.remember();
        cx.notify();
    }

    fn start_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(text) = self.messages.get(index).map(|m| m.text.clone()) else {
            return;
        };
        self.editing = Some(index);
        self.edit_input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn save_edit(&mut self, resend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.editing.take() else {
            return;
        };
        if self.busy() || index >= self.messages.len() {
            return cx.notify();
        }
        let updated = self.edit_input.read(cx).value().trim().to_string();
        if updated.is_empty() && self.messages[index].pictures.is_empty() {
            self.input.update(cx, |input, cx| input.focus(window, cx));
            return cx.notify();
        }
        let is_user = self.messages[index].role == Role::User;
        self.messages[index].text = updated.clone();
        if !is_user {
            self.messages[index].view = Some(cx.new(|cx| TextViewState::markdown(&updated, cx)));
        }
        self.input.update(cx, |input, cx| input.focus(window, cx));
        if resend && is_user {
            self.stop_playing();
            self.messages.truncate(index + 1);
            self.rounds = 0;
            self.request(cx);
        } else {
            self.remember();
            cx.notify();
        }
    }

    fn clear_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember();
        if self.busy() {
            self.stop(cx);
        }
        self.stop_playing();
        self.messages.clear();
        self.editing = None;
        self.error = None;
        self.attachments.clear();
        self.pending_reference = None;
        self.rounds = 0;
        self.conversation = history::new_id(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default(),
        );
        self.created = crate::historyview::now();
        self.input.update(cx, |input, cx| input.focus(window, cx));
        self.show_notice("已清空上下文", cx);
    }

    fn show_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice_generation = self.notice_generation.wrapping_add(1);
        let generation = self.notice_generation;
        self.notice = Some(text.into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(2200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.notice_generation == generation {
                    this.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn picker_defaults(&self) -> PickerDefaults {
        build_picker_defaults(
            &self.assistant,
            self.default_chat.as_ref(),
            self.default_image.as_ref(),
            self.default_speech.as_ref(),
            self.route.as_ref().ok(),
        )
    }

    fn set_default(
        &mut self,
        purpose: Purpose,
        model: &ModelRef,
        scope: DefaultScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = crate::config_dir();
        let written = format!("{}/{}", model.provider, model.model);
        let outcome: Result<String, String> = match (purpose, scope) {
            (Purpose::Chat, DefaultScope::Global) => {
                let path = dir.join(config::SETTINGS_FILE);
                std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|source| {
                        let next = edit::set_setting(&source, "models", "default", Field::Text(written.clone()))?;
                        config::parse_settings(&next)?;
                        std::fs::write(&path, next).map_err(|e| e.to_string())?;
                        Ok("已设为全局默认".into())
                    })
            }
            (Purpose::Chat, DefaultScope::Assistant) => {
                let path = dir.join(config::ASSISTANTS_FILE);
                std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|source| {
                        let next = edit::set_entry(
                            &source,
                            "assistant",
                            "name",
                            &self.assistant.name,
                            &[("model", Field::Text(written.clone()))],
                        )?;
                        config::parse_assistants(&next)?;
                        std::fs::write(&path, next).map_err(|e| e.to_string())?;
                        Ok(format!("已设为「{}」默认", self.assistant.name))
                    })
            }
            (Purpose::Images, _) => {
                let path = dir.join(config::SETTINGS_FILE);
                std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|source| {
                        let next = edit::set_setting(&source, "image", "default", Field::Text(written.clone()))?;
                        config::parse_settings(&next)?;
                        std::fs::write(&path, next).map_err(|e| e.to_string())?;
                        Ok("已设为默认生图".into())
                    })
            }
            (Purpose::Speech, _) => {
                let path = dir.join(config::SETTINGS_FILE);
                std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|source| {
                        let next = edit::set_setting(&source, "speech", "tts", Field::Text(written.clone()))?;
                        config::parse_settings(&next)?;
                        std::fs::write(&path, next).map_err(|e| e.to_string())?;
                        Ok("已设为默认语音".into())
                    })
            }
        };
        match outcome {
            Ok(label) => {
                match (purpose, scope) {
                    (Purpose::Chat, DefaultScope::Global) => {
                        self.default_chat = Some(model.clone());
                    }
                    (Purpose::Chat, DefaultScope::Assistant) => {
                        self.assistant.model = Some(written);
                    }
                    (Purpose::Images, _) => {
                        self.default_image = Some(model.clone());
                        self.last_image_model = Some(model.clone());
                    }
                    (Purpose::Speech, _) => {
                        self.default_speech = Some(model.clone());
                        self.tts = speech_route(&self.speech_providers, Some(&written));
                    }
                }
                self.choose_model(purpose, model, window, cx);
                let defaults = self.picker_defaults();
                let picker_notice = label.clone();
                self.picker.update(cx, |picker, cx| {
                    picker.set_defaults(defaults, Some(picker_notice), cx);
                });
                self.show_notice(label, cx);
            }
            Err(reason) => {
                self.error = Some(reason);
                cx.notify();
            }
        }
    }

    fn toggle_favorite(&mut self, purpose: Purpose, model: &ModelRef, cx: &mut Context<Self>) {
        let dir = crate::config_dir();
        let file = match purpose {
            Purpose::Chat => config::PROVIDERS_FILE,
            Purpose::Images => config::IMAGE_PROVIDERS_FILE,
            Purpose::Speech => return,
        };
        let path = dir.join(file);
        let reference = format!("{}/{}", model.provider, model.model);
        let outcome = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|source| {
                let (next, added) = edit::toggle_favorite(&source, &reference)?;
                match purpose {
                    Purpose::Chat => config::parse_providers(&next).map(|_| ()),
                    Purpose::Images => config::parse_image_providers(&next).map(|_| ()),
                    Purpose::Speech => Ok(()),
                }?;
                std::fs::write(&path, next).map_err(|e| e.to_string())?;
                Ok(added)
            });
        if let Err(reason) = outcome {
            self.error = Some(reason);
            cx.notify();
        }
    }

    /// The reasoning levels the current model offers, and whether it can only
    /// be switched on and off. Models the catalog does not know get the
    /// common set.
    fn reasoning_levels(&self) -> (Vec<Reasoning>, bool) {
        if !matches!(self.mode, Mode::Chat) {
            return (Vec::new(), false);
        }
        let info = self
            .route
            .as_ref()
            .ok()
            .and_then(|route| self.catalog.lookup(&route.model));
        match info {
            Some(info) => (info.levels(), info.switch_only()),
            None => (
                vec![
                    Reasoning::None,
                    Reasoning::Low,
                    Reasoning::Medium,
                    Reasoning::High,
                ],
                false,
            ),
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
            let current = self.model_reference();
            self.picker.update(cx, |picker, cx| {
                picker.open(
                    current
                        .as_ref()
                        .map(|(purpose, reference)| (*purpose, reference)),
                    window,
                    cx,
                )
            });
        }
        cx.notify();
    }

    fn close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sheet = Sheet::None;
        self.pending_reference = None;
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
                let chosen = self
                    .reasoning
                    .map(|level| catalog::nearest(&levels, level).unwrap_or(level));
                let options = std::iter::once(None).chain(levels.into_iter().map(Some));
                let rows = options
                    .enumerate()
                    .map(|(index, value)| {
                        sheet_row(("reasoning", index), palette, false)
                            .child(div().flex_1().child(reasoning_label(value, switch_only)))
                            .when(value == chosen, |row| {
                                row.child(
                                    svg()
                                        .path("icons/check.svg")
                                        .size(px(14.0))
                                        .flex_none()
                                        .text_color(palette.accent),
                                )
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reasoning = value;
                                this.close_sheet(window, cx);
                            }))
                    })
                    .collect::<Vec<_>>();
                div()
                    .p(px(5.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element()
            }
            Sheet::Image(parameter) => {
                let spec = self.image_spec()?;
                let chosen = self.image_options.get(parameter);
                let rows = parameter.choices(spec).into_iter().enumerate().map(
                    |(index, (value, label))| {
                        option_row(
                            ("image-option", index),
                            palette,
                            Some(value) == chosen,
                            label,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.image_options.set(parameter, value);
                                this.close_sheet(window, cx);
                            },
                        ))
                    },
                );
                div()
                    .p(px(5.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element()
            }
            Sheet::Voice => {
                let rows = self.voices.iter().enumerate().map(|(index, voice)| {
                    let id = voice.id.clone();
                    let label = if voice.name.is_empty() {
                        voice.id.clone()
                    } else {
                        voice.name.clone()
                    };
                    option_row(
                        ("voice", index),
                        palette,
                        id == self.tts_options.voice,
                        label,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.tts_options.voice.clone_from(&id);
                        this.close_sheet(window, cx);
                    }))
                });
                let status = if self.voice_loading {
                    Some("正在拉取音色…".to_string())
                } else if let Some(reason) = &self.voice_error {
                    Some(reason.clone())
                } else {
                    self.voices.is_empty().then(|| self.voice_label())
                };
                div()
                    .id("voices")
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .p(px(5.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .children(status.map(|status| {
                        div()
                            .p(px(7.0))
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .child(status)
                    }))
                    .into_any_element()
            }
            sheet @ (Sheet::Speed | Sheet::Stability | Sheet::Style) => {
                let chosen = self.speech_parameter(sheet);
                let api = self
                    .direct_speech_route()
                    .map_or(SpeechApi::Elevenlabs, |(provider, _)| provider.api);
                let rows = speech_choices(sheet, api).iter().copied().enumerate().map(
                    |(index, (value, label))| {
                        option_row(("speech-option", index), palette, value == chosen, label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                match sheet {
                                    Sheet::Speed => this.tts_options.speed = Some(value),
                                    Sheet::Stability => this.tts_options.stability = Some(value),
                                    Sheet::Style => this.tts_options.style = Some(value),
                                    _ => unreachable!(),
                                }
                                this.close_sheet(window, cx);
                            }))
                    },
                );
                div()
                    .p(px(5.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element()
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
                .when(!wide, |sheet| {
                    sheet.w(px(if self.sheet == Sheet::Voice {
                        225.0
                    } else {
                        150.0
                    }))
                })
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
        let Some(message) = self.messages.get(index) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(message.text.clone()));
        self.copied = Some(index);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == Some(index) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn fetch_voices(&mut self, provider: SpeechProvider, cx: &mut Context<Self>) {
        self.voice_loading = true;
        let generation = self.voice_generation;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(speech::voices(&provider));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.voice_generation == generation {
                    this.voice_loading = false;
                    match result {
                        Ok(voices) => this.voices = voices,
                        Err(reason) => this.voice_error = Some(reason),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn voice_label(&self) -> String {
        let selected = &self.tts_options.voice;
        if selected.is_empty() {
            return "默认音色".into();
        }
        self.voices
            .iter()
            .find(|voice| &voice.id == selected)
            .map(|voice| {
                if voice.name.is_empty() {
                    voice.id.clone()
                } else {
                    voice.name.clone()
                }
            })
            .unwrap_or_else(|| selected.clone())
    }

    fn speech_parameter(&self, sheet: Sheet) -> f32 {
        match sheet {
            Sheet::Speed => self.tts_options.speed.unwrap_or(1.0),
            Sheet::Stability => self.tts_options.stability.unwrap_or(0.5),
            Sheet::Style => self.tts_options.style.unwrap_or(0.0),
            _ => unreachable!(),
        }
    }

    fn stop_playing(&mut self) {
        self.speaking = None;
        self.speak_generation += 1;
        speech::silence();
    }

    fn playback_timer(&mut self, duration: Duration, cx: &mut Context<Self>) {
        let generation = self.speak_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            let _ = this.update(cx, |this, cx| {
                if this.speak_generation == generation {
                    this.speaking = None;
                    cx.notify();
                }
            });
            // Wait a brief tail before closing MCI so the final samples finish
            // without keeping the saved MP3 file locked on disk.
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, _| {
                if this.speak_generation == generation {
                    speech::silence();
                }
            });
        })
        .detach();
    }

    fn play_clip(&mut self, index: usize, cx: &mut Context<Self>) {
        let target = Playing::Clip(index);
        if self.speaking == Some(target) {
            self.stop_playing();
        } else if let Some(card) = self
            .messages
            .get(index)
            .and_then(|message| message.audio.as_ref())
        {
            let (path, duration) = (card.path.clone(), card.duration);
            self.stop_playing();
            self.speaking = Some(target);
            let generation = self.speak_generation;
            let rx = speech::play_saved_async(path);
            cx.spawn(async move |this, cx| {
                let Ok(result) = rx.recv().await else { return };
                let _ = this.update(cx, |this, cx| {
                    if this.speak_generation != generation {
                        return;
                    }
                    match result {
                        Ok(()) => this.playback_timer(duration, cx),
                        Err(reason) => {
                            this.speaking = None;
                            this.error = Some(reason);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Synthesis runs off the UI thread; stopping discards any late playback.
    fn speak(&mut self, index: usize, save: bool, cx: &mut Context<Self>) {
        let target = Playing::Reply(index);
        if save && self.saving_speech == Some(index) {
            return;
        }
        if !save && self.speaking == Some(target) {
            self.stop_playing();
            return cx.notify();
        }
        let Some((provider, model)) = self.tts.clone() else {
            return;
        };
        let Some(text) = self
            .messages
            .get(index)
            .map(|m| m.text.clone())
            .filter(|t| !t.is_empty())
        else {
            return;
        };

        let mut options = speech::TtsOptions::default();
        if save {
            self.saving_speech = Some(index);
        } else {
            self.stop_playing();
            self.speaking = Some(target);
            if provider.api == SpeechApi::Elevenlabs {
                options.output_format = "pcm_24000".into();
            }
        }
        let generation = self.speak_generation;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = speech::tts(&provider, &model, &text, &options).and_then(|clip| {
                if save {
                    speech::save(&speech::folder(), &clip.bytes, clip.ext).map(Reading::Saved)
                } else {
                    speech::wav_duration(&clip.bytes)
                        .map(|duration| Reading::Wav(clip.bytes, duration))
                }
            });
            let _ = tx.send_blocking(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if save && this.saving_speech == Some(index) {
                    this.saving_speech = None;
                }
                if !save && this.speak_generation != generation {
                    return;
                }
                match result {
                    Ok(Reading::Saved(path)) => win::shell_select(&path),
                    Ok(Reading::Wav(bytes, duration)) => match speech::play(bytes) {
                        Ok(()) => this.playback_timer(duration, cx),
                        Err(reason) => {
                            this.speaking = None;
                            this.error = Some(reason);
                        }
                    },
                    Err(reason) => {
                        this.error = Some(reason);
                        if !save {
                            this.speaking = None;
                        }
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
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        let audio = recorder.stop();
        let Some(route) = self.asr.clone() else {
            return;
        };
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
        let pin = if self.keep_open {
            "icons/pin.svg"
        } else {
            "icons/pin-off.svg"
        };
        let pin_color = if self.keep_open {
            palette.accent
        } else {
            palette.faint
        };
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
            .children(self.notice.clone().map(|text| {
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .bg(palette.accent_wash)
                    .text_size(px(11.0))
                    .text_color(palette.accent)
                    .child(text)
            }))
            .when(!self.messages.is_empty(), |row| {
                row.child(
                    icon_button(
                        "clear-context",
                        "icons/eraser.svg",
                        palette.faint,
                        palette.selected,
                    )
                    .occlude()
                    .on_click(cx.listener(|this, _, window, cx| this.clear_context(window, cx))),
                )
            })
            .when(self.assistant.window.close_on_blur, |row| {
                row.child(
                    icon_button("pin", pin, pin_color, palette.selected)
                        .occlude()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.keep_open = !this.keep_open;
                            cx.notify();
                        })),
                )
            })
            .child(
                icon_button(
                    "history",
                    "icons/clock.svg",
                    palette.faint,
                    palette.selected,
                )
                .occlude()
                .on_click(|_, _, _| crate::events::post(crate::events::UiMsg::Page("history"))),
            )
            .child(
                icon_button("close", "icons/x.svg", palette.muted, palette.selected)
                    .occlude()
                    .on_click(|_, window, _| window.remove_window()),
            )
    }

    fn edit_box(&self, index: usize, is_user: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let btn = |id: (&'static str, usize), label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(26.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.0))
                .when(primary, |b| b.bg(palette.accent).text_color(rgb(0xFFFFFF)))
                .when(!primary, |b| {
                    b.text_color(palette.muted)
                        .border_1()
                        .border_color(palette.hairline)
                        .hover(move |s| s.bg(palette.selected))
                })
                .child(label)
        };
        div()
            .w_full()
            .p(px(10.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(palette.accent)
            .bg(palette.raised)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(13.5))
                    .child(Textarea::new(&self.edit_input).appearance(false)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(6.0))
                    .child(
                        btn(("edit-cancel", index), "取消", false)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx))),
                    )
                    .child(
                        btn(("edit-save", index), "保存", !is_user).on_click(
                            cx.listener(|this, _, window, cx| this.save_edit(false, window, cx)),
                        ),
                    )
                    .when(is_user, |row| {
                        row.child(
                            btn(("edit-resend", index), "保存并重发", true).on_click(
                                cx.listener(|this, _, window, cx| this.save_edit(true, window, cx)),
                            ),
                        )
                    }),
            )
    }

    fn user_row(
        &self,
        index: usize,
        message: &Message,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        if self.editing == Some(index) {
            return div().w_full().child(self.edit_box(index, true, cx));
        }
        let palette = self.palette;
        let long = message.text.chars().count() > 260 || message.text.lines().count() > 6;
        let bubble = div()
            .id(("user", index))
            .max_w(relative(0.86))
            .px(px(13.0))
            .py(px(9.0))
            .rounded(px(14.0))
            .bg(palette.selected)
            .text_size(px(13.5))
            .line_height(relative(1.55))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(long, |bubble| {
                bubble
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(message) = this.messages.get_mut(index) {
                            message.expanded = !message.expanded;
                            cx.notify();
                        }
                    }))
            })
            .children(message.thumb.clone().map(|picture| {
                img(picture)
                    .max_w(px(240.0))
                    .max_h(px(180.0))
                    .rounded(px(8.0))
            }))
            .when(
                !message.pictures.is_empty() && message.thumb.is_none(),
                |b| b.child("图片"),
            )
            .when(!message.text.is_empty(), |bubble| {
                bubble.child(
                    div()
                        .when(long && !message.expanded, |text| {
                            text.text_ellipsis().line_clamp(6)
                        })
                        .child(message.text.clone()),
                )
            });
        let copied = self.copied == Some(index);
        let idle = !self.busy();
        let actions = (!message.text.is_empty() || idle).then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .when(!message.text.is_empty(), |row| {
                    row.child(
                        icon_button(
                            ("copy-user", index),
                            if copied {
                                "icons/check.svg"
                            } else {
                                "icons/copy.svg"
                            },
                            if copied {
                                palette.accent
                            } else {
                                palette.faint
                            },
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.copy(index, cx))),
                    )
                })
                .when(idle, |row| {
                    row.child(
                        icon_button(
                            ("edit-user", index),
                            "icons/pencil.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_edit(index, window, cx)
                        })),
                    )
                    .child(
                        icon_button(
                            ("retry-user", index),
                            "icons/refresh-cw.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.retry_at(index, cx))),
                    )
                    .child(
                        icon_button(
                            ("delete-user", index),
                            "icons/trash.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_message(index, cx))),
                    )
                })
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(3.0))
            .child(bubble)
            .children(actions)
    }

    fn reply_row(
        &self,
        index: usize,
        message: &Message,
        last: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        if self.editing == Some(index) {
            return div().w_full().child(self.edit_box(index, false, cx));
        }
        let palette = self.palette;
        let streaming = last && self.busy();
        let idle = !self.busy();
        let body = div()
            .w_full()
            .font(theme::serif())
            .text_size(px(15.0))
            .line_height(relative(1.7));
        let body = if !message.shots.is_empty() {
            let hwnd = self.hwnd;
            body.children(message.shots.iter().enumerate().map(|(n, shot)| {
                let path = shot.path.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(shot.thumb.clone().map(|thumb| {
                        img(thumb)
                            .max_w(px(340.0))
                            .max_h(px(260.0))
                            .rounded(px(10.0))
                    }))
                    .child(div().flex().gap(px(4.0)).children({
                        let mut buttons = Vec::new();
                        for (action, label) in
                            ["打开", "复制", "文件夹", "改图"].into_iter().enumerate()
                        {
                            let path = path.clone();
                            buttons.push(
                                media_button(format!("shot-{index}-{n}-{action}"), label, palette)
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| match action {
                                            0 => win::shell_open(&path),
                                            1 => win::shell_copy_image(hwnd, &path),
                                            2 => win::shell_select(&path),
                                            _ => this.edit_shot(&path, window, cx),
                                        },
                                    ))
                                    .into_any_element(),
                            );
                        }
                        buttons
                    }))
                    .into_any_element()
            }))
        } else if let Some(card) = &message.audio {
            body.child(self.audio_card(index, card, cx))
        } else if message.text.is_empty() {
            body.when(streaming, |body| body.child(waiting_dots(palette)))
        } else {
            body.children(
                message
                    .view
                    .as_ref()
                    .map(|view| TextView::new(view).selectable(true)),
            )
        };
        let copied = self.copied == Some(index);
        let speaking = self.speaking == Some(Playing::Reply(index));
        let can_speak = self.tts.is_some() && !message.text.is_empty();
        let has_content =
            !message.text.is_empty() || !message.shots.is_empty() || message.audio.is_some();
        let can_retry = idle && has_content;
        let can_edit = idle && !message.text.is_empty();
        let can_delete = idle && (has_content || !message.calls.is_empty());
        let actions = (!streaming && (!message.text.is_empty() || can_retry || can_delete)).then(|| {
            div()
                .flex()
                .gap(px(2.0))
                .ml(px(-6.0))
                .when(!message.text.is_empty(), |row| {
                    row.child(
                        icon_button(
                            ("copy", index),
                            if copied {
                                "icons/check.svg"
                            } else {
                                "icons/copy.svg"
                            },
                            if copied {
                                palette.accent
                            } else {
                                palette.faint
                            },
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.copy(index, cx))),
                    )
                })
                .when(can_speak, |row| {
                    row.child(
                        icon_button(
                            ("speak", index),
                            "icons/audio-lines.svg",
                            if speaking {
                                palette.accent
                            } else {
                                palette.faint
                            },
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.speak(index, false, cx))),
                    )
                    .child(
                        media_button(
                            ("save-speech", index),
                            if self.saving_speech == Some(index) {
                                "保存中…"
                            } else {
                                "保存音频"
                            },
                            palette,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.speak(index, true, cx)),
                        ),
                    )
                })
                .when(can_edit, |row| {
                    row.child(
                        icon_button(
                            ("edit-reply", index),
                            "icons/pencil.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_edit(index, window, cx)
                        })),
                    )
                })
                .when(can_retry, |row| {
                    row.child(
                        icon_button(
                            ("retry", index),
                            "icons/refresh-cw.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.retry_at(index, cx))),
                    )
                })
                .when(can_delete, |row| {
                    row.child(
                        icon_button(
                            ("delete-reply", index),
                            "icons/trash.svg",
                            palette.faint,
                            palette.selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_message(index, cx))),
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
                .child(
                    svg()
                        .path("icons/lightbulb.svg")
                        .size(px(13.0))
                        .text_color(palette.faint),
                )
                .child(label)
                .child(
                    svg()
                        .path(if open {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        })
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
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(4.0))
                .child(header)
                .children(shown.map(|text| {
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
                .child(
                    svg()
                        .path("icons/wrench.svg")
                        .size(px(13.0))
                        .flex_none()
                        .text_color(palette.muted),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(palette.ink)
                        .child(label),
                )
                .child(div().flex_none().text_color(color).child(status))
                .child(div().flex_1())
                .child(
                    svg()
                        .path(if open {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        })
                        .size(px(12.0))
                        .flex_none()
                        .text_color(palette.faint),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(state) = this
                        .messages
                        .get_mut(index)
                        .and_then(|m| m.calls.get_mut(n))
                    {
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
                    .when(primary, |button| {
                        button.bg(palette.accent).text_color(rgb(0xFFFFFF))
                    })
                    .when(!primary, |button| {
                        button
                            .border_1()
                            .border_color(palette.hairline)
                            .text_color(palette.muted)
                    })
                    .hover(|style| style.opacity(0.88))
                    .child(label)
            };
            let asking = (state.status == CallStatus::Waiting).then(|| {
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(choice(0, "允许", true).on_click(
                        cx.listener(move |this, _, _, cx| this.decide(index, n, true, false, cx)),
                    ))
                    .child(choice(1, "这次对话都允许", false).on_click(
                        cx.listener(move |this, _, _, cx| this.decide(index, n, true, true, cx)),
                    ))
                    .child(choice(2, "拒绝", false).on_click(
                        cx.listener(move |this, _, _, cx| this.decide(index, n, false, false, cx)),
                    ))
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
                    card.child(block(written)).when(!shown.is_empty(), |card| {
                        card.child(block(if more { format!("{shown}…") } else { shown }))
                    })
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

    fn audio_card(
        &self,
        index: usize,
        card: &AudioCard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let palette = self.palette;
        let playing = self.speaking == Some(Playing::Clip(index));
        let elapsed = card.duration.as_secs();
        let duration = format!("{}:{:02}", elapsed / 60, elapsed % 60);
        let open = card.path.clone();
        let select = card.path.clone();
        div()
            .max_w(px(380.0))
            .p(px(10.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(palette.hairline)
            .bg(palette.raised)
            .font(theme::sans())
            .text_size(px(12.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(palette.muted)
                    .child(
                        svg()
                            .path("icons/audio-lines.svg")
                            .size(px(15.0))
                            .flex_none()
                            .text_color(palette.accent),
                    )
                    .child(div().flex_none().child(duration))
                    .child(div().min_w(px(0.0)).truncate().child(card.summary.clone())),
            )
            .child(
                div()
                    .flex()
                    .gap(px(3.0))
                    .child(
                        media_button(
                            ("play-clip", index),
                            if playing { "停止" } else { "播放" },
                            palette,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.play_clip(index, cx))),
                    )
                    .child(
                        media_button(("open-clip", index), "打开", palette)
                            .on_click(move |_, _, _| win::shell_open(&open)),
                    )
                    .child(
                        media_button(("show-clip", index), "文件夹", palette)
                            .on_click(move |_, _, _| win::shell_select(&select)),
                    ),
            )
    }

    fn attachments(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.attachments.is_empty() || !self.image_spec().is_some_and(|spec| spec.supports_edit)
        {
            return None;
        }
        let palette = self.palette;
        let images = self
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                div()
                    .size(px(52.0))
                    .relative()
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(palette.selected)
                    .child(match &attachment.thumb {
                        Some(thumb) => img(thumb.clone()).size_full().into_any_element(),
                        None => svg()
                            .path("icons/image.svg")
                            .size(px(20.0))
                            .text_color(palette.muted)
                            .into_any_element(),
                    })
                    .child(
                        div().absolute().top(px(2.0)).right(px(2.0)).child(
                            icon_button(
                                ("remove-reference", index),
                                "icons/x.svg",
                                palette.ink,
                                palette.raised,
                            )
                            .size(px(19.0))
                            .bg(palette.raised)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if index < this.attachments.len() {
                                        this.attachments.remove(index);
                                        cx.notify();
                                    }
                                },
                            )),
                        ),
                    )
            });
        Some(
            div()
                .px(px(4.0))
                .pt(px(3.0))
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .children(images),
        )
    }

    fn parameter_chips(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let palette = self.palette;
        let mut row = div().flex().flex_wrap().items_center().gap(px(2.0));
        if let Some(spec) = self.image_spec() {
            for parameter in [
                ImageParameter::Size,
                ImageParameter::Resolution,
                ImageParameter::Quality,
                ImageParameter::Format,
            ] {
                let Some(value) = self.image_options.get(parameter) else {
                    continue;
                };
                let label = parameter
                    .choices(spec)
                    .into_iter()
                    .find(|(wire, _)| *wire == value)
                    .map_or(value, |(_, label)| label);
                let (id, name) = parameter.label();
                let sheet = Sheet::Image(parameter);
                row =
                    row.child(
                        chip(
                            id,
                            None,
                            format!("{name} {label}"),
                            palette,
                            self.sheet == sheet,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.chip_clicked(sheet, window, cx),
                        )),
                    );
            }
        } else if let Some((provider, _)) = self.direct_speech_route() {
            let elevenlabs = provider.api == SpeechApi::Elevenlabs;
            let voice = self.voice_label();
            if elevenlabs {
                row = row.child(
                    chip(
                        "voice-chip",
                        None,
                        voice,
                        palette,
                        self.sheet == Sheet::Voice,
                    )
                    .max_w(px(150.0))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.chip_clicked(Sheet::Voice, window, cx)
                    })),
                );
            } else {
                row = row.child(
                    div()
                        .px(px(8.0))
                        .max_w(px(150.0))
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(palette.muted)
                        .child(voice),
                );
            }
            for (sheet, id, name) in [
                (Sheet::Speed, "speed-chip", "语速"),
                (Sheet::Stability, "stability-chip", "稳定度"),
                (Sheet::Style, "style-chip", "表现力"),
            ] {
                if !elevenlabs && sheet != Sheet::Speed {
                    continue;
                }
                let value = self.speech_parameter(sheet);
                let label = speech_choices(sheet, provider.api)
                    .iter()
                    .find(|(choice, _)| *choice == value)
                    .map_or_else(|| format!("{value:.2}"), |(_, label)| (*label).to_string());
                row =
                    row.child(
                        chip(
                            id,
                            None,
                            format!("{name} {label}"),
                            palette,
                            self.sheet == sheet,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.chip_clicked(sheet, window, cx),
                        )),
                    );
            }
        } else {
            return None;
        }
        Some(row)
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let streaming = self.busy();
        // The mic appears only when speech recognition is set up and keyed.
        let mic = self
            .asr
            .is_some()
            .then(|| self.recorder.is_some() || self.transcribing);
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
                if this.busy() {
                    this.stop(cx)
                } else {
                    this.submit(window, cx)
                }
            }));
        let send = if streaming {
            send.bg(palette.ink)
                .child(div().size(px(10.0)).rounded(px(2.5)).bg(palette.surface))
        } else {
            let (fill, arrow) = if ready {
                (palette.accent, rgb(0xFFFFFF))
            } else {
                (palette.hairline, palette.faint)
            };
            send.bg(fill).child(
                svg()
                    .path("icons/arrow-up.svg")
                    .size(px(16.0))
                    .text_color(arrow),
            )
        };
        let model = self.model_reference().map(|(_, reference)| reference.model);
        let brand = model
            .as_deref()
            .and_then(|model| crate::brand::model(model, palette.ink))
            .map(|image| crate::brand::badge(image, 14.0));
        let (levels, switch_only) = self.reasoning_levels();
        let fitted = self
            .reasoning
            .map(|level| catalog::nearest(&levels, level).unwrap_or(level));
        let reasoning = reasoning_label(fitted, switch_only);
        let chips = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                chip_with(
                    "model",
                    brand,
                    model.unwrap_or_else(|| "选择模型".into()),
                    palette,
                    self.sheet == Sheet::Models,
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.chip_clicked(Sheet::Models, window, cx)),
                ),
            )
            .when(!levels.is_empty(), |chips| {
                chips.child(
                    chip(
                        "reasoning",
                        Some("icons/brain.svg"),
                        reasoning.into(),
                        palette,
                        self.sheet == Sheet::Reasoning,
                    )
                    .flex_none()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.chip_clicked(Sheet::Reasoning, window, cx)
                    })),
                )
            });
        div()
            .flex_none()
            .relative()
            .px(px(14.0))
            .pb(px(14.0))
            .pt(px(4.0))
            .children(self.sheet(cx))
            .child(
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
                    .children(self.attachments(cx))
                    .child(
                        div()
                            .pl(px(2.0))
                            .text_size(px(13.5))
                            .child(Textarea::new(&self.input).appearance(false)),
                    )
                    .children(self.parameter_chips(cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(chips)
                            .children(mic.map(|mic| {
                                let seconds = self
                                    .recorder
                                    .as_ref()
                                    .map(|r| r.started.elapsed().as_secs());
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
                                            .text_color(if mic {
                                                palette.accent
                                            } else {
                                                palette.faint
                                            }),
                                    )
                                    .children(label.map(|text| {
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(palette.accent)
                                            .child(text)
                                    }))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.dictate(window, cx)),
                                    )
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
        speech::silence();
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

fn icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    color: Rgba,
    hover: Rgba,
) -> Stateful<Div> {
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
        .child(
            svg()
                .path("icons/chevron-down.svg")
                .size(px(12.0))
                .flex_none()
                .text_color(palette.faint),
        )
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
    div()
        .h(px(24.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .children((0..3usize).map(|dot| {
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(palette.muted)
                .with_animation(
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

fn option_row(
    id: impl Into<ElementId>,
    palette: Palette,
    chosen: bool,
    label: impl Into<gpui_kit::SharedString>,
) -> Stateful<Div> {
    sheet_row(id, palette, chosen)
        .child(div().flex_1().min_w(px(0.0)).truncate().child(label.into()))
        .when(chosen, |row| {
            row.child(
                svg()
                    .path("icons/check.svg")
                    .size(px(14.0))
                    .flex_none()
                    .text_color(palette.accent),
            )
        })
}

fn media_button(id: impl Into<ElementId>, label: &'static str, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(9.0))
        .py(px(3.0))
        .rounded(px(7.0))
        .cursor_pointer()
        .font(theme::sans())
        .text_size(px(12.0))
        .text_color(palette.muted)
        .hover(move |style| style.bg(palette.selected))
        .child(label)
}

fn build_picker_defaults(
    assistant: &Assistant,
    chat_global: Option<&ModelRef>,
    image: Option<&ModelRef>,
    speech: Option<&ModelRef>,
    route: Option<&Route>,
) -> PickerDefaults {
    let assistant_uses_global = assistant
        .model
        .as_deref()
        .is_none_or(|model| model.trim().is_empty() || model.trim() == "default");
    let chat_assistant = if assistant_uses_global {
        chat_global.cloned()
    } else {
        assistant
            .model
            .as_deref()
            .and_then(ModelRef::parse)
            .or_else(|| {
                route.map(|route| ModelRef {
                    provider: route.provider_id.clone(),
                    model: route.model.clone(),
                })
            })
    };
    PickerDefaults {
        assistant_name: assistant.name.clone(),
        assistant_uses_global,
        chat_global: chat_global.cloned(),
        chat_assistant,
        image: image.cloned(),
        speech: speech.cloned(),
    }
}

fn speech_choices(sheet: Sheet, api: SpeechApi) -> &'static [(f32, &'static str)] {
    match sheet {
        Sheet::Speed if api == SpeechApi::Elevenlabs => &[
            (0.85, "0.85×"),
            (1.0, "1.0×"),
            (1.1, "1.1×"),
            (1.2, "1.2×"),
        ],
        Sheet::Speed => &[
            (0.85, "0.85×"),
            (1.0, "1.0×"),
            (1.15, "1.15×"),
            (1.3, "1.3×"),
        ],
        Sheet::Stability => &[(0.3, "创意 0.3"), (0.5, "自然 0.5"), (0.8, "稳定 0.8")],
        Sheet::Style => &[(0.0, "关 0.0"), (0.3, "轻 0.3"), (0.6, "强 0.6")],
        _ => &[],
    }
}

fn speech_route(
    providers: &[SpeechProvider],
    reference: Option<&str>,
) -> Option<(SpeechProvider, String)> {
    let reference = ModelRef::parse(reference?)?;
    let provider = providers
        .iter()
        .find(|provider| provider.id == reference.provider && provider.enabled)?;
    let stored = secrets::read_in(secrets::NS_SPEECH, &provider.id)?;
    if provider.keys_to_try(&stored, 0).is_empty() {
        return None;
    }
    Some((provider.clone(), reference.model))
}

fn voice_summary(voice: &str, api: SpeechApi, options: &speech::TtsOptions) -> String {
    let mut fields = vec![voice.to_string()];
    if matches!(api, SpeechApi::Openai | SpeechApi::Elevenlabs) {
        fields.push(format!("语速 {}×", options.speed.unwrap_or(1.0)));
    }
    if api == SpeechApi::Elevenlabs {
        for (label, value) in [
            ("稳定度", options.stability),
            ("相似度", options.similarity_boost),
            ("表现力", options.style),
        ] {
            if let Some(value) = value {
                fields.push(format!("{label} {value}"));
            }
        }
    }
    if api != SpeechApi::Openai && !options.language.is_empty() && options.language != "auto" {
        fields.push(options.language.clone());
    }
    fields.join(" · ")
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
    Some(Arc::new(gpui_kit::Image::from_bytes(
        format,
        bytes.to_vec(),
    )))
}

/// The captured content as the opening user message, with the picture to show.
fn first_turn(content: Content) -> (Turn, Option<Arc<gpui_kit::Image>>) {
    match content {
        Content::Text(text) => (Turn::user(text), None),
        Content::Files(files) => {
            let list: Vec<String> = files
                .iter()
                .map(|path| path.display().to_string())
                .collect();
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
            let picture = Picture {
                mime: mime.into(),
                base64: win::base64(&image.bytes),
            };
            (
                Turn {
                    pictures: vec![picture],
                    ..Turn::user("")
                },
                thumb,
            )
        }
    }
}
