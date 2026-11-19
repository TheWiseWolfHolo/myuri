//! The settings window: assistants and their prompts, providers and their
//! keys, and the general options. It edits the same files a text editor
//! would, so the running program picks the changes up by itself.

use std::fs;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Div, ElementId, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, TitlebarOptions, Window, WindowBounds,
    WindowControlArea,
    WindowOptions, anchored, deferred, div, point, px, relative, size, svg,
};

use myuri_core::catalog::Catalog;
use myuri_core::config::{
    self, Api, Assistant, KeyStrategy, ModelRef, Provider, Purpose, Reasoning, Settings, SpeechApi,
    ThemeChoice,
};
use myuri_core::image::ImageApi;
use myuri_core::edit::{self, Field};
use myuri_core::llm;
use myuri_core::speech::Voice;
use myuri_core::uistate::{self, UiState};

use crate::icon::Glyph;
use crate::speech;
use crate::models::{self, Upstream};
use crate::picker::{ModelPicker, PickerEvent, sheet_row};
use crate::secrets;
use crate::theme::{self, Palette};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Assistants,
    Chat,
    Images,
    Speech,
    General,
}

impl Tab {
    fn purpose(self) -> Option<Purpose> {
        match self {
            Self::Chat => Some(Purpose::Chat),
            Self::Images => Some(Purpose::Images),
            Self::Speech => Some(Purpose::Speech),
            Self::Assistants | Self::General => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Assistants => "assistants",
            Self::Chat => "chat",
            Self::Images => "images",
            Self::Speech => "speech",
            Self::General => "general",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "assistants" => Self::Assistants,
            "chat" => Self::Chat,
            "images" => Self::Images,
            "speech" => Self::Speech,
            "general" => Self::General,
            _ => return None,
        })
    }
}

/// One stored key of the open provider.
struct KeyRow {
    secret: String,
    note: Entity<InputState>,
    enabled: bool,
}

/// What is being dragged in a list.
#[derive(Clone)]
struct DragProvider(usize, SharedString);
#[derive(Clone)]
struct DragGroup(String);
#[derive(Clone)]
struct DragAssistant(usize, SharedString);

/// The label that follows the pointer while dragging.
struct DragChip {
    label: SharedString,
    palette: Palette,
}

impl Render for DragChip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(11.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .bg(self.palette.raised)
            .border_1()
            .border_color(self.palette.accent)
            .shadow_md()
            .font(theme::sans())
            .text_size(px(12.5))
            .text_color(self.palette.ink)
            .child(self.label.clone())
    }
}

fn chip(label: SharedString, palette: Palette, cx: &mut App) -> Entity<DragChip> {
    cx.new(|_| DragChip { label, palette })
}

pub struct SettingsView {
    dir: PathBuf,
    palette: Palette,
    catalog: std::sync::Arc<Catalog>,
    tab: Tab,
    assistants: Vec<Assistant>,
    providers: Vec<Provider>,
    settings: Settings,
    /// Index of the assistant and the provider being edited.
    assistant: usize,
    provider: usize,
    /// Provider groups that are folded in the list.
    folded: std::collections::BTreeSet<String>,
    /// Filters the list in the middle column.
    search: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    /// Outcome of the last save: whether it worked, and what to say.
    status: Option<(bool, String)>,
    /// What a second click of a delete button would remove.
    confirm: Option<String>,

    name: Entity<InputState>,
    icon: Entity<InputState>,
    /// The assistant's pending model; the field that sets it is a picker.
    model: Entity<InputState>,
    picker: Option<Entity<ModelPicker>>,
    picker_open: bool,
    /// The picker is choosing the default model rather than an assistant's.
    picker_default: bool,
    /// `[models] default`: what assistants without a model use.
    default_model: Entity<InputState>,
    prompt: Entity<TextareaState>,
    enabled: bool,
    reasoning: Option<Reasoning>,

    provider_id: Entity<InputState>,
    provider_name: Entity<InputState>,
    base_url: Entity<InputState>,
    group: Entity<InputState>,
    /// The keys of the open provider, in the order the credential stores them.
    key_rows: Vec<KeyRow>,
    /// Appends a key for a normal provider, replaces the JSON for a Vertex one.
    new_key: Entity<InputState>,
    api: Api,
    image_api: ImageApi,
    speech_api: Option<SpeechApi>,
    image_models: Entity<TextareaState>,
    asr_models: Entity<TextareaState>,
    tts_models: Entity<TextareaState>,
    /// The voice a speech provider speaks with.
    voice: Entity<InputState>,
    /// Voices fetched from ElevenLabs to pick from.
    voice_options: Vec<Voice>,
    voice_fetching: bool,
    /// A 试听 request in flight.
    testing: bool,
    /// `[speech]` on the general page: which provider/model listens and speaks.
    speech_asr: Option<String>,
    speech_tts: Option<String>,
    /// Which of the two dropdowns is open, if any.
    speech_sheet: Option<&'static str>,
    strategy: KeyStrategy,
    provider_enabled: bool,
    /// How many keys the credential store holds for the provider.
    stored_keys: usize,
    fetching: bool,
    fetched: Option<Result<Upstream, String>>,

    hold: Entity<InputState>,
    hotkey: Entity<InputState>,
    theme: ThemeChoice,
    /// The window size the last render saw; changes are saved, debounced.
    seen_size: gpui_kit::Size<gpui_kit::Pixels>,
    size_generation: u64,
}

/// Open the settings window. Returns its handle, for bringing it to the
/// front the next time.
pub fn open(dir: PathBuf, dark: bool, cx: &mut App) -> Option<gpui_kit::AnyWindowHandle> {
    let remembered = uistate::load(&dir);
    let extent = if remembered.window.width > 0.0 && remembered.window.height > 0.0 {
        size(px(remembered.window.width), px(remembered.window.height))
    } else {
        size(px(980.0), px(680.0))
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            extent,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Myuri 设置".into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_min_size: Some(size(px(760.0), px(480.0))),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SettingsView::new(dir, dark, window, cx))
    })
    .ok()
    .map(|(handle, _)| handle)
}

impl SettingsView {
    fn new(dir: PathBuf, dark: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        fn line(hint: &'static str, window: &mut Window, cx: &mut Context<SettingsView>) -> Entity<InputState> {
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        }
        let icon = line("languages、icons/a.png，或者一个字", window, cx);
        let model = line("渠道 id/模型名，留空用默认模型", window, cx);
        let provider_id = line("只能字母、数字、-、_ 和 .", window, cx);
        let provider_name = line("显示的名字", window, cx);
        let base_url = line("https://api.example.com/v1", window, cx);
        let group = line("切换模型时归在哪个分组下", window, cx);
        let new_key =
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("粘贴密钥或服务账号 JSON"));
        let hold = line("220", window, cx);
        let hotkey = line("Ctrl+Alt+Space", window, cx);
        let default_model = line("", window, cx);
        let name = line("助手的名字", window, cx);
        let search = line("搜索", window, cx);
        let subscriptions = vec![cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        let prompt = cx.new(|cx| TextareaState::new(window, cx).auto_grow(8, 22).placeholder("系统提示词"));
        let mut list = |hint: &'static str| {
            cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 14).placeholder(hint))
        };
        let image_models = list("一行一个模型名");
        let asr_models = list("一行一个模型名");
        let tts_models = list("一行一个模型名");
        let voice = line("语音的音色，如声音 id", window, cx);
        let remembered = uistate::load(&dir);
        let mut view = Self {
            catalog: std::sync::Arc::new(models::load(&dir)),
            dir,
            palette: theme::palette(dark),
            tab: Tab::named(&remembered.page).unwrap_or(Tab::Assistants),
            assistants: Vec::new(),
            providers: Vec::new(),
            settings: Settings::default(),
            assistant: 0,
            provider: 0,
            folded: remembered.folded.into_iter().collect(),
            search,
            _subscriptions: subscriptions,
            name,
            status: None,
            confirm: None,
            icon,
            model,
            picker: None,
            picker_open: false,
            picker_default: false,
            default_model,
            prompt,
            enabled: true,
            reasoning: None,
            provider_id,
            provider_name,
            base_url,
            group,
            key_rows: Vec::new(),
            new_key,
            api: Api::ChatCompletions,
            image_api: ImageApi::Openai,
            speech_api: None,
            image_models,
            asr_models,
            tts_models,
            voice,
            voice_options: Vec::new(),
            voice_fetching: false,
            testing: false,
            speech_asr: None,
            speech_tts: None,
            speech_sheet: None,
            strategy: KeyStrategy::Fallback,
            provider_enabled: true,
            stored_keys: 0,
            fetching: false,
            fetched: None,
            hold,
            hotkey,
            theme: ThemeChoice::System,
            seen_size: Default::default(),
            size_generation: 0,
        };
        view.reload(window, cx);
        window.activate_window();
        view
    }

    /// Write the fold, page and window size back to ui-state.toml.
    fn save_ui_state(&self) {
        let state = UiState {
            folded: self.folded.iter().cloned().collect(),
            window: uistate::UiWindow {
                width: f32::from(self.seen_size.width),
                height: f32::from(self.seen_size.height),
            },
            page: self.tab.name().to_string(),
        };
        uistate::save(&self.dir, &state);
    }

    /// Read everything from disk again and refill the forms.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut problems = Vec::new();
        match config::load_assistants(&self.dir) {
            Ok(assistants) => self.assistants = assistants,
            Err(problem) => problems.push(problem.to_string()),
        }
        match config::load_providers(&self.dir) {
            Ok(providers) => self.providers = providers,
            Err(problem) => problems.push(problem.to_string()),
        }
        match config::load_settings(&self.dir) {
            Ok(settings) => self.settings = settings,
            Err(problem) => problems.push(problem.to_string()),
        }
        if !problems.is_empty() {
            self.status = Some((false, problems.join("；")));
        }
        self.assistant = self.assistant.min(self.assistants.len().saturating_sub(1));
        self.provider = self.provider.min(self.providers.len().saturating_sub(1));
        self.fill_assistant(window, cx);
        self.fill_provider(window, cx);
        let hold = self.settings.gesture.hold_ms.to_string();
        let hotkey = self.settings.hotkey.panel.clone();
        self.hold.update(cx, |input, cx| input.set_value(hold, window, cx));
        let default_model = self.settings.models.get("default").cloned().unwrap_or_default();
        self.default_model.update(cx, |input, cx| input.set_value(default_model, window, cx));
        self.hotkey.update(cx, |input, cx| input.set_value(hotkey, window, cx));
        self.theme = self.settings.panel.theme;
        self.speech_asr = self.settings.speech.asr.clone();
        self.speech_tts = self.settings.speech.tts.clone();
        cx.notify();
    }

    /// The model picker overlay goes away when the entry under it changes.
    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        if let Some(picker) = &self.picker {
            picker.update(cx, |picker, _| picker.sleep());
        }
    }

    fn fill_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.picker_open = false;
        self.picker = None;
        let Some(assistant) = self.assistants.get(self.assistant).cloned() else { return };
        let prompt = config::load_prompt(&self.dir, &assistant);
        self.name.update(cx, |input, cx| input.set_value(assistant.name.clone(), window, cx));
        self.icon.update(cx, |input, cx| input.set_value(assistant.icon.clone(), window, cx));
        let model = assistant.model.clone().unwrap_or_default();
        self.model.update(cx, |input, cx| input.set_value(model, window, cx));
        self.prompt.update(cx, |input, cx| input.set_value(prompt, window, cx));
        self.enabled = assistant.enabled;
        self.reasoning = assistant.reasoning;
    }

    /// After switching pages, show a provider that belongs on the page.
    fn pick_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(purpose) = self.tab.purpose() else { return };
        let fits = |provider: &Provider| provider.serves(purpose);
        if !self.providers.get(self.provider).is_some_and(fits) {
            if let Some(index) = self.providers.iter().position(fits) {
                self.provider = index;
            }
        }
        self.fill_provider(window, cx);
    }

    fn fill_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.close_picker(cx);
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        self.provider_id.update(cx, |input, cx| input.set_value(provider.id.clone(), window, cx));
        self.provider_name.update(cx, |input, cx| input.set_value(provider.name.clone(), window, cx));
        self.base_url.update(cx, |input, cx| input.set_value(provider.base_url.clone(), window, cx));
        self.group.update(cx, |input, cx| input.set_value(provider.group.clone(), window, cx));
        self.new_key.update(cx, |input, cx| input.set_value("", window, cx));
        self.api = provider.api;
        self.image_api = provider.image_api();
        self.speech_api = provider.speech_api;
        for (input, names) in [
            (&self.image_models, &provider.image_models),
            (&self.asr_models, &provider.asr_models),
            (&self.tts_models, &provider.tts_models),
        ] {
            let text = names.join("\n");
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        self.voice.update(cx, |input, cx| input.set_value(provider.voice.clone(), window, cx));
        self.voice_options.clear();
        self.testing = false;
        self.strategy = provider.key_strategy;
        self.provider_enabled = provider.enabled;
        let stored = secrets::read(&provider.id).unwrap_or_default();
        self.stored_keys = llm::split_keys(&stored).len();
        self.key_rows = if provider.vertex {
            Vec::new()
        } else {
            llm::split_keys(&stored)
                .into_iter()
                .enumerate()
                .map(|(index, secret)| {
                    let note = cx.new(|cx| InputState::new(window, cx).placeholder("备注"));
                    let text = provider.keys.get(index).map(|key| key.note.clone()).unwrap_or_default();
                    note.update(cx, |note, cx| note.set_value(text, window, cx));
                    KeyRow {
                        secret: secret.to_string(),
                        note,
                        enabled: provider.keys.get(index).is_none_or(|key| key.enabled),
                    }
                })
                .collect()
        };
        self.fetched = None;
        self.fetching = false;
    }

    /// Write `updated` to `file` if `check` accepts it.
    fn write_checked(
        &self,
        file: &str,
        updated: Result<String, String>,
        check: impl Fn(&str) -> Result<(), String>,
    ) -> Result<(), String> {
        let updated = updated?;
        check(&updated)?;
        fs::write(self.dir.join(file), updated).map_err(|error| error.to_string())
    }

    fn finish(&mut self, outcome: Result<(), String>, window: &mut Window, cx: &mut Context<Self>) {
        self.status = Some(match outcome {
            Ok(()) => (true, "已保存".to_string()),
            Err(reason) => (false, reason),
        });
        let failed = self.status.as_ref().is_some_and(|(ok, _)| !ok);
        if !failed {
            self.reload(window, cx);
        }
        cx.notify();
    }

    fn save_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(assistant) = self.assistants.get(self.assistant).cloned() else { return };
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = read(&self.name);
        if name.is_empty() {
            self.status = Some((false, "助手要有名字".into()));
            cx.notify();
            return;
        }
        if name != assistant.name && self.assistants.iter().any(|other| other.name == name) {
            self.status = Some((false, format!("已经有叫“{name}”的助手了")));
            cx.notify();
            return;
        }
        let renamed = Assistant { name: name.clone(), ..assistant.clone() };
        let changes = [
            ("name", Field::Text(name.clone())),
            ("enabled", Field::Flag(self.enabled)),
            ("icon", Field::optional(&read(&self.icon))),
            ("model", Field::optional(&read(&self.model))),
            ("reasoning", match self.reasoning {
                Some(level) => Field::Text(level.as_str().into()),
                None => Field::Absent,
            }),
        ];
        let prompt = self.prompt.read(cx).value().to_string();
        let outcome = fs::read_to_string(self.dir.join(config::ASSISTANTS_FILE))
            .map_err(|error| error.to_string())
            .and_then(|source| {
                let updated = edit::set_entry(&source, "assistant", "name", &assistant.name, &changes);
                self.write_checked(config::ASSISTANTS_FILE, updated, |text| {
                    config::parse_assistants(text).map(|_| ())
                })
            })
            .and_then(|()| {
                let path = self.dir.join(renamed.prompt_path());
                if let Some(folder) = path.parent() {
                    let _ = fs::create_dir_all(folder);
                }
                fs::write(&path, prompt.trim()).map_err(|error| error.to_string())?;
                // A prompt file named after the assistant follows the new name.
                let old = self.dir.join(assistant.prompt_path());
                if old != path {
                    let _ = fs::remove_file(old);
                }
                Ok(())
            });
        self.finish(outcome, window, cx);
        if let Some(index) = self.assistants.iter().position(|assistant| assistant.name == name) {
            self.assistant = index;
            self.fill_assistant(window, cx);
        }
    }

    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        // The id is the part of a model reference before the slash.
        let new_id = read(&self.provider_id);
        let valid = !new_id.is_empty()
            && new_id.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid {
            self.status = Some((false, "渠道 id 只能用字母、数字、-、_ 和 .".into()));
            cx.notify();
            return;
        }
        if new_id != provider.id && self.providers.iter().any(|other| other.id == new_id) {
            self.status = Some((false, format!("已经有叫“{new_id}”的渠道了")));
            cx.notify();
            return;
        }
        let renaming = new_id != provider.id;
        let api = match self.api {
            Api::ChatCompletions => "chat_completions",
            Api::Responses => "responses",
            Api::Anthropic => "anthropic",
            Api::Gemini => "gemini",
        };
        let strategy = match self.strategy {
            KeyStrategy::Fallback => Field::Absent,
            KeyStrategy::RoundRobin => Field::Text("round_robin".into()),
            KeyStrategy::Sticky => Field::Text("sticky".into()),
        };
        let lines = |input: &Entity<TextareaState>| {
            let text = input.read(cx).value();
            text.lines().map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect::<Vec<_>>()
        };
        let mut changes = vec![
            ("name", Field::optional(&read(&self.provider_name))),
            ("base_url", Field::Text(read(&self.base_url))),
            ("group", Field::optional(&read(&self.group))),
            ("key_strategy", strategy),
            ("enabled", if self.provider_enabled { Field::Absent } else { Field::Flag(false) }),
        ];
        match self.tab {
            Tab::Images => {
                changes.push(("image_api", Field::Text(self.image_api.as_str().into())));
                changes.push(("image_models", Field::List(lines(&self.image_models))));
            }
            Tab::Speech => {
                let speech = self.speech_api.map(|api| Field::Text(api.as_str().into()));
                changes.push(("speech_api", speech.unwrap_or(Field::Absent)));
                changes.push(("asr_models", Field::List(lines(&self.asr_models))));
                changes.push(("tts_models", Field::List(lines(&self.tts_models))));
                let voice = read(&self.voice);
                changes.push(("voice", if voice.is_empty() { Field::Absent } else { Field::Text(voice) }));
            }
            _ => changes.push(("api", Field::Text(api.into()))),
        }
        // One `keys` entry per stored key, in the credential's order. Entries
        // that say nothing are left out; if all say nothing so is `keys`.
        let notes: Vec<Vec<(&'static str, Field)>> = self
            .key_rows
            .iter()
            .map(|row| {
                let mut fields = Vec::new();
                let note = row.note.read(cx).value().trim().to_string();
                if !note.is_empty() {
                    fields.push(("note", Field::Text(note)));
                }
                if !row.enabled {
                    fields.push(("enabled", Field::Flag(false)));
                }
                fields
            })
            .collect();
        if !provider.vertex {
            let keys = if notes.iter().all(|row| row.is_empty()) {
                Field::Absent
            } else {
                Field::Table(notes)
            };
            changes.push(("keys", keys));
        }
        let joined = self.key_rows.iter().map(|row| row.secret.clone()).collect::<Vec<_>>().join(",");
        let pasted = read(&self.new_key);
        let vertex = provider.vertex;
        let outcome = fs::read_to_string(self.dir.join(config::PROVIDERS_FILE))
            .map_err(|error| error.to_string())
            .and_then(|providers| {
                if !renaming {
                    let updated = edit::set_entry(&providers, "provider", "id", &provider.id, &changes);
                    return self.write_checked(config::PROVIDERS_FILE, updated, |text| {
                        config::parse_providers(text).map(|_| ())
                    });
                }
                let assistants = fs::read_to_string(self.dir.join(config::ASSISTANTS_FILE))
                    .map_err(|error| error.to_string())?;
                let settings = fs::read_to_string(self.dir.join(config::SETTINGS_FILE))
                    .map_err(|error| error.to_string())?;
                let (providers, assistants, settings) =
                    edit::rename_provider(&providers, &assistants, &settings, &provider.id, &new_id)?;
                let updated = edit::set_entry(&providers, "provider", "id", &new_id, &changes)?;
                // All three files have to check out before any is written.
                config::parse_providers(&updated).map(|_| ())?;
                config::parse_assistants(&assistants).map(|_| ())?;
                config::parse_settings(&settings).map(|_| ())?;
                fs::write(self.dir.join(config::PROVIDERS_FILE), updated)
                    .map_err(|error| error.to_string())?;
                fs::write(self.dir.join(config::ASSISTANTS_FILE), assistants)
                    .map_err(|error| error.to_string())?;
                fs::write(self.dir.join(config::SETTINGS_FILE), settings)
                    .map_err(|error| error.to_string())?;
                // The credential follows the id.
                if let Some(stored) = secrets::read(&provider.id) {
                    secrets::write(&new_id, &stored)?;
                    secrets::delete(&provider.id)?;
                }
                Ok(())
            })
            .and_then(|()| {
                let id = if renaming { &new_id } else { &provider.id };
                if vertex {
                    if pasted.is_empty() { Ok(()) } else { secrets::write(id, &pasted) }
                } else if joined.is_empty() {
                    secrets::delete(id).map(|_| ())
                } else {
                    secrets::write(id, &joined)
                }
            });
        self.finish(outcome, window, cx);
        if renaming {
            self.select_provider(&new_id, window, cx);
        }
    }

    fn save_general(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hold = self.hold.read(cx).value().trim().parse::<i64>();
        let hotkey = self.hotkey.read(cx).value().trim().to_string();
        let theme = match self.theme {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        };
        let outcome = match hold {
            Err(_) => Err("长按时长要写一个整数，单位是毫秒".to_string()),
            Ok(hold) => fs::read_to_string(self.dir.join(config::SETTINGS_FILE))
                .map_err(|error| error.to_string())
                .and_then(|source| edit::set_setting(&source, "gesture", "hold_ms", Field::Number(hold)))
                .and_then(|source| edit::set_setting(&source, "hotkey", "panel", Field::Text(hotkey)))
                .and_then(|source| {
                    let model = self.default_model.read(cx).value().trim().to_string();
                    edit::set_setting(&source, "models", "default", Field::optional(&model))
                })
                .and_then(|source| {
                    let field = self.speech_asr.clone().map(Field::Text).unwrap_or(Field::Absent);
                    edit::set_setting(&source, "speech", "asr", field)
                })
                .and_then(|source| {
                    let field = self.speech_tts.clone().map(Field::Text).unwrap_or(Field::Absent);
                    edit::set_setting(&source, "speech", "tts", field)
                })
                .and_then(|source| {
                    let updated = edit::set_setting(&source, "panel", "theme", Field::Text(theme.into()));
                    self.write_checked(config::SETTINGS_FILE, updated, |text| {
                        config::parse_settings(text).map(|_| ())
                    })
                }),
        };
        self.finish(outcome, window, cx);
    }

    /// Rewrite one file from its current text, checking the result.
    fn rewrite(
        &self,
        file: &'static str,
        change: impl FnOnce(String) -> Result<String, String>,
    ) -> Result<(), String> {
        let source = fs::read_to_string(self.dir.join(file)).map_err(|error| error.to_string())?;
        self.write_checked(file, change(source), |text| match file {
            config::ASSISTANTS_FILE => config::parse_assistants(text).map(|_| ()),
            _ => config::parse_providers(text).map(|_| ()),
        })
    }

    /// Show the provider with this id again after the list changed.
    fn select_provider(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.providers.iter().position(|provider| provider.id == id) {
            self.provider = index;
            self.fill_provider(window, cx);
        }
    }

    /// Put provider `from` next to provider `to`, in `to`'s group.
    fn drop_provider(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dragged), Some(target)) = (self.providers.get(from).cloned(), self.providers.get(to)) else {
            return;
        };
        if from == to {
            return;
        }
        let order = edit::moved(self.providers.len(), &[from], to, from < to);
        let group = target.group.clone();
        self.place_provider(&dragged, &order, &group, window, cx);
    }

    /// Put provider `from` at the top of `group`.
    fn drop_into_group(&mut self, from: usize, group: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dragged) = self.providers.get(from).cloned() else { return };
        let Some(first) = self.providers.iter().position(|provider| provider.group == group) else { return };
        let order = edit::moved(self.providers.len(), &[from], first, false);
        self.place_provider(&dragged, &order, group, window, cx);
    }

    fn place_provider(
        &mut self,
        dragged: &Provider,
        order: &[usize],
        group: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let outcome = self.rewrite(config::PROVIDERS_FILE, |source| {
            let source = edit::reorder(&source, "provider", order)?;
            if dragged.group == group {
                return Ok(source);
            }
            edit::set_entry(&source, "provider", "id", &dragged.id, &[("group", Field::optional(group))])
        });
        self.finish(outcome, window, cx);
        self.select_provider(&dragged.id.clone(), window, cx);
    }

    /// Move a whole group next to another one.
    fn drop_group(&mut self, dragged: &str, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        let members = |name: &str| {
            self.providers.iter().enumerate().filter(|(_, p)| p.group == name).map(|(index, _)| index).collect::<Vec<_>>()
        };
        let (block, aim) = (members(dragged), members(target));
        let (Some(&start), Some(&first), Some(&last)) = (block.first(), aim.first(), aim.last()) else { return };
        if dragged == target {
            return;
        }
        let order = if start < first {
            edit::moved(self.providers.len(), &block, last, true)
        } else {
            edit::moved(self.providers.len(), &block, first, false)
        };
        let id = self.providers.get(self.provider).map(|provider| provider.id.clone()).unwrap_or_default();
        let outcome = self.rewrite(config::PROVIDERS_FILE, |source| edit::reorder(&source, "provider", &order));
        self.finish(outcome, window, cx);
        self.select_provider(&id, window, cx);
    }

    fn drop_assistant(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if from == to || from >= self.assistants.len() || to >= self.assistants.len() {
            return;
        }
        let name = self.assistants[from].name.clone();
        let order = edit::moved(self.assistants.len(), &[from], to, from < to);
        let outcome = self.rewrite(config::ASSISTANTS_FILE, |source| edit::reorder(&source, "assistant", &order));
        self.finish(outcome, window, cx);
        if let Some(index) = self.assistants.iter().position(|assistant| assistant.name == name) {
            self.assistant = index;
            self.fill_assistant(window, cx);
        }
    }

    /// The first click arms a delete; the second carries it out.
    fn armed(&mut self, what: String, cx: &mut Context<Self>) -> bool {
        if self.confirm.as_ref() == Some(&what) {
            self.confirm = None;
            return true;
        }
        self.confirm = Some(what);
        cx.notify();
        false
    }

    fn delete_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        let prefix = format!("{}/", provider.id);
        let users: Vec<String> = self
            .assistants
            .iter()
            .filter(|assistant| assistant.model.as_deref().is_some_and(|model| model.starts_with(&prefix)))
            .map(|assistant| assistant.name.clone())
            .chain(self.settings.models.iter().filter(|(_, model)| model.starts_with(&prefix)).map(|(tier, _)| format!("默认模型 {tier}")))
            .collect();
        if !users.is_empty() {
            self.status = Some((false, format!("{} 还在用这个渠道，先给它们换个模型", users.join("、"))));
            cx.notify();
            return;
        }
        if !self.armed(format!("provider:{}", provider.id), cx) {
            return;
        }
        let outcome = self
            .rewrite(config::PROVIDERS_FILE, |source| edit::remove_entry(&source, "provider", "id", &provider.id))
            .and_then(|()| secrets::delete(&provider.id).map(|_| ()));
        self.finish(outcome, window, cx);
    }

    fn delete_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(assistant) = self.assistants.get(self.assistant).cloned() else { return };
        if !self.armed(format!("assistant:{}", assistant.name), cx) {
            return;
        }
        let outcome = self.rewrite(config::ASSISTANTS_FILE, |source| {
            edit::remove_entry(&source, "assistant", "name", &assistant.name)
        });
        self.finish(outcome, window, cx);
    }

    /// Take every provider out of a group. The providers stay.
    fn dissolve_group(&mut self, group: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.armed(format!("group:{group}"), cx) {
            return;
        }
        let members: Vec<String> =
            self.providers.iter().filter(|provider| provider.group == group).map(|provider| provider.id.clone()).collect();
        let outcome = self.rewrite(config::PROVIDERS_FILE, |mut source| {
            for id in &members {
                source = edit::set_entry(&source, "provider", "id", id, &[("group", Field::Absent)])?;
            }
            Ok(source)
        });
        self.folded.remove(group);
        self.finish(outcome, window, cx);
    }

    /// The delete button in a form's heading.
    fn delete_button(
        &self,
        what: String,
        on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let palette = self.palette;
        let armed = self.confirm.as_ref() == Some(&what);
        div()
            .id("delete")
            .h(px(28.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(if armed { palette.accent } else { palette.muted })
            .when(armed, |button| button.bg(palette.accent_wash))
            .hover(move |style| style.bg(palette.selected))
            .child(svg().path("icons/trash.svg").size(px(13.0)).text_color(if armed { palette.accent } else { palette.faint }))
            .child(if armed { "再点一次删除" } else { "删除" })
            .on_click(on_click)
    }

    /// Add an entry for the page that is open: an assistant, or a provider
    /// set up for chat, pictures or speech.
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let numbered = |base: &str, taken: &dyn Fn(&str) -> bool| {
            (1..).map(|n| if n == 1 { base.to_string() } else { format!("{base} {n}") }).find(|name| !taken(name)).unwrap_or_default()
        };
        if self.tab == Tab::Assistants {
            let name = numbered("新助手", &|name| self.assistants.iter().any(|a| a.name == name));
            let entry = format!("\n[[assistant]]\nname = {name:?}\nicon = \"sparkles\"\n");
            let outcome = self.rewrite(config::ASSISTANTS_FILE, |source| Ok(format!("{}\n{entry}", source.trim_end())));
            self.finish(outcome, window, cx);
            if let Some(index) = self.assistants.iter().position(|assistant| assistant.name == name) {
                self.assistant = index;
                self.fill_assistant(window, cx);
            }
            return;
        }
        let id = (1..).map(|n| format!("channel-{n}")).find(|id| !self.providers.iter().any(|p| &p.id == id)).unwrap_or_default();
        let name = numbered("新渠道", &|name| self.providers.iter().any(|p| p.name == name));
        let purpose = match self.tab {
            Tab::Images => "image_api = \"openai\"\n",
            Tab::Speech => "speech_api = \"elevenlabs\"\n",
            _ => "",
        };
        let entry = format!("\n[[provider]]\nid = {id:?}\nname = {name:?}\nbase_url = \"https://\"\n{purpose}");
        let outcome = self.rewrite(config::PROVIDERS_FILE, |source| Ok(format!("{}\n{entry}", source.trim_end())));
        self.finish(outcome, window, cx);
        self.select_provider(&id, window, cx);
    }

    /// Ask the provider for its model list, off the UI thread.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        self.fetching = true;
        self.fetched = None;
        let id = provider.id.clone();
        cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(async move { models::upstream(&provider) }).await;
            let _ = this.update(cx, |this, cx| {
                // The answer belongs to the provider that was open when asked.
                if this.providers.get(this.provider).is_some_and(|open| open.id == id) {
                    this.fetching = false;
                    this.fetched = Some(answer);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(provider), Some(Ok(upstream))) = (self.providers.get(self.provider), &self.fetched)
        else {
            return;
        };
        let outcome = models::sync(&self.dir, provider, upstream, &self.catalog.clone());
        self.finish(outcome, window, cx);
    }

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let entries = [
            (Tab::Assistants, "icons/bot.svg", "助手"),
            (Tab::Chat, "icons/message-circle.svg", "对话"),
            (Tab::Images, "icons/image.svg", "生图"),
            (Tab::Speech, "icons/audio-lines.svg", "语音"),
            (Tab::General, "icons/sliders-horizontal.svg", "通用"),
        ];
        div()
            .w(px(150.0))
            .flex_none()
            .h_full()
            .px(px(8.0))
            .pb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(entries.into_iter().enumerate().map(|(index, (tab, icon, label))| {
                let open = self.tab == tab;
                row(("nav", index), palette, open)
                    .h(px(34.0))
                    .child(svg().path(icon).size(px(15.0)).flex_none().text_color(if open {
                        palette.accent
                    } else {
                        palette.muted
                    }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.tab = tab;
                        this.status = None;
                        this.confirm = None;
                        this.close_picker(cx);
                        this.pick_provider(window, cx);
                        this.save_ui_state();
                        cx.notify();
                    }))
            }))
    }

    fn list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = self.palette;
        let query = self.search.read(cx).value().trim().to_lowercase();
        let found = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let rows: Vec<AnyElement> = match self.tab {
            Tab::General => return None,
            Tab::Assistants => self
                .assistants
                .iter()
                .enumerate()
                .filter(|(_, assistant)| found(&assistant.name))
                .map(|(index, assistant)| {
                    let open = index == self.assistant;
                    let tint = if open { palette.accent } else { palette.muted };
                    let label: SharedString = assistant.name.clone().into();
                    row(("assistant", index), palette, open)
                        .when(!assistant.enabled, |row| row.opacity(0.45))
                        .on_drag(DragAssistant(index, label), move |dragged, _, _, cx| chip(dragged.1.clone(), palette, cx))
                        .drag_over::<DragAssistant>(move |style, _, _, _| style.bg(palette.accent_wash))
                        .on_drop(cx.listener(move |this, dragged: &DragAssistant, window, cx| {
                            this.drop_assistant(dragged.0, index, window, cx);
                        }))
                        .child(Glyph::of(assistant).render(18.0, tint))
                        .child(div().flex_1().min_w(px(0.0)).truncate().child(assistant.name.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.assistant = index;
                            this.status = None;
                            this.fill_assistant(window, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
            Tab::Chat | Tab::Images | Tab::Speech => {
                let purpose = self.tab.purpose().unwrap_or(Purpose::Chat);
                let mut rows = Vec::new();
                let mut group: Option<&str> = None;
                let listed = self
                    .providers
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.serves(purpose) && (found(&p.name) || found(&p.id)));
                for (index, provider) in listed {
                    // While searching, matches are listed flat.
                    let folded = query.is_empty() && self.folded.contains(&provider.group);
                    if query.is_empty() && group != Some(provider.group.as_str()) && !provider.group.is_empty() {
                        let name = provider.group.clone();
                        let members =
                            self.providers.iter().filter(|p| p.group == name && p.serves(purpose)).count();
                        let arrow = if folded { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" };
                        let armed = self.confirm.as_deref() == Some(&format!("group:{name}"));
                        let (into, onto, dissolve) = (name.clone(), name.clone(), name.clone());
                        let dissolve_button = div()
                            .id(("dissolve", index))
                            .flex_none()
                            .h(px(20.0))
                            .px(px(4.0))
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .gap(px(3.0))
                            .when(!armed, |button| button.opacity(0.0).group_hover("group-header", |style| style.opacity(1.0)))
                            .text_color(palette.accent)
                            .hover(move |style| style.bg(palette.accent_wash))
                            .child(svg().path("icons/trash.svg").size(px(12.0)).text_color(if armed { palette.accent } else { palette.faint }))
                            .when(armed, |button| button.child("解散分组？"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.dissolve_group(&dissolve, window, cx);
                            }));
                        rows.push(
                            div()
                                .id(("group", index))
                                .group("group-header")
                                .on_drag(DragGroup(name.clone()), move |dragged, _, _, cx| chip(dragged.0.clone().into(), palette, cx))
                                .drag_over::<DragGroup>(move |style, _, _, _| style.bg(palette.accent_wash))
                                .drag_over::<DragProvider>(move |style, _, _, _| style.bg(palette.accent_wash))
                                .on_drop(cx.listener(move |this, dragged: &DragGroup, window, cx| {
                                    this.drop_group(&dragged.0, &onto, window, cx);
                                }))
                                .on_drop(cx.listener(move |this, dragged: &DragProvider, window, cx| {
                                    this.drop_into_group(dragged.0, &into, window, cx);
                                }))
                                .flex_none()
                                .h(px(28.0))
                                .mt(px(6.0))
                                .px(px(6.0))
                                .rounded(px(7.0))
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .cursor_pointer()
                                .text_size(px(11.0))
                                .text_color(palette.faint)
                                .hover(move |style| style.bg(palette.selected))
                                .child(svg().path(arrow).size(px(12.0)).flex_none().text_color(palette.faint))
                                .child(div().flex_1().min_w(px(0.0)).truncate().child(name.clone()))
                                .child(members.to_string())
                                .child(dissolve_button)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.folded.remove(&name) {
                                        this.folded.insert(name.clone());
                                    }
                                    this.save_ui_state();
                                    cx.notify();
                                }))
                                .into_any_element(),
                        );
                    }
                    group = Some(provider.group.as_str());
                    if folded && !provider.group.is_empty() {
                        continue;
                    }
                    let name = if provider.name.is_empty() { &provider.id } else { &provider.name };
                    rows.push(
                        row(("provider", index), palette, index == self.provider)
                            .when(!provider.enabled, |row| row.opacity(0.45))
                            .on_drag(DragProvider(index, name.clone().into()), move |dragged, _, _, cx| {
                                chip(dragged.1.clone(), palette, cx)
                            })
                            .drag_over::<DragProvider>(move |style, _, _, _| style.bg(palette.accent_wash))
                            .on_drop(cx.listener(move |this, dragged: &DragProvider, window, cx| {
                                this.drop_provider(dragged.0, index, window, cx);
                            }))
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(name.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.5))
                                    .text_color(palette.faint)
                                    .child(provider.model_count(purpose).to_string()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.provider = index;
                                this.status = None;
                                this.fill_provider(window, cx);
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
                rows
            }
        };
        let bar = div()
            .flex_none()
            .p(px(6.0))
            .pb(px(2.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(30.0))
                    .rounded(px(8.0))
                    .bg(palette.raised)
                    .border_1()
                    .border_color(palette.hairline)
                    .flex()
                    .items_center()
                    .pl(px(8.0))
                    .child(svg().path("icons/search.svg").size(px(13.0)).flex_none().text_color(palette.faint))
                    .child(div().flex_1().min_w(px(0.0)).text_size(px(12.5)).child(Input::new(&self.search).appearance(false))),
            )
            .child(
                div()
                    .id("create")
                    .size(px(30.0))
                    .flex_none()
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .border_1()
                    .border_color(palette.hairline)
                    .hover(move |style| style.bg(palette.selected))
                    .child(svg().path("icons/plus.svg").size(px(15.0)).text_color(palette.muted))
                    .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
            );
        Some(
            div()
                .w(px(232.0))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_r_1()
                .border_color(palette.hairline)
                .flex()
                .flex_col()
                .child(bar)
                .child(
                    div()
                        .id("list")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .p(px(6.0))
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// Toggle the model picker under the model field.
    fn open_model_picker(&mut self, default: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker_open {
            self.close_picker(cx);
            cx.notify();
            return;
        }
        self.picker_default = default;
        let picker = cx.new(|cx| {
            ModelPicker::new(
                self.providers.clone(),
                config::load_favorites(&self.dir),
                self.catalog.clone(),
                Purpose::Chat,
                self.palette,
                window,
                cx,
            )
        });
        self._subscriptions.push(cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, window, cx| {
            match event {
                PickerEvent::Chosen(reference) => {
                    let model = format!("{}/{}", reference.provider, reference.model);
                    let target = if this.picker_default { &this.default_model } else { &this.model };
                    target.update(cx, |input, cx| input.set_value(model, window, cx));
                    this.picker_open = false;
                }
                PickerEvent::Dismissed => this.picker_open = false,
            }
            cx.notify();
        }));
        let target = if default { &self.default_model } else { &self.model };
        let written = target.read(cx).value().trim().to_string();
        // A tier name resolves to its provider/model before it can be picked.
        let current = ModelRef::parse(&written)
            .or_else(|| self.settings.models.get(&written).and_then(|tier| ModelRef::parse(tier)));
        picker.update(cx, |picker, cx| picker.open(current.as_ref(), window, cx));
        self.picker = Some(picker);
        self.picker_open = true;
        cx.notify();
    }

    /// The model field as a button, with the picker hanging below it while open.
    /// `default` is the field of the default model on the general page.
    fn model_field(&self, default: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let target = if default { &self.default_model } else { &self.model };
        let written = target.read(cx).value().trim().to_string();
        let (display, empty) = if written.is_empty() {
            (if default { "还没选" } else { "默认模型" }.to_string(), true)
        } else {
            let shown = ModelRef::parse(&written)
                .map(|m| {
                    let name = self
                        .providers
                        .iter()
                        .find(|p| p.id == m.provider)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| m.provider.clone());
                    format!("{name} · {}", m.model)
                })
                .unwrap_or_else(|| written.clone());
            (shown, false)
        };
        let button = div()
            .id("model-field")
            .flex_1()
            .min_w(px(0.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(palette.hairline)
            .bg(palette.raised)
            .px(px(10.0))
            .h(px(34.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .text_size(px(13.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(if empty { palette.faint } else { palette.ink })
                    .child(display),
            )
            .child(svg().path("icons/chevron-down.svg").size(px(14.0)).flex_none().text_color(palette.faint))
            .on_click(cx.listener(move |this, _, window, cx| this.open_model_picker(default, window, cx)));
        let clear = (!empty).then(|| {
            div()
                .id("clear-model")
                .flex_none()
                .h(px(26.0))
                .px(px(9.0))
                .rounded(px(7.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(palette.muted)
                .hover(move |style| style.bg(palette.selected))
                .child(if default { "清除" } else { "用默认模型" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    let target = if default { &this.default_model } else { &this.model };
                    target.update(cx, |input, cx| input.set_value("", window, cx));
                    cx.notify();
                }))
        });
        let overlay = self.picker.clone().filter(|_| self.picker_open && self.picker_default == default).map(|picker| {
            deferred(
                anchored()
                    .snap_to_window()
                    .offset(point(px(0.0), px(4.0)))
                    .child(
                        div()
                            .occlude()
                            .w(px(520.0))
                            .rounded(px(14.0))
                            .border_1()
                            .border_color(palette.hairline)
                            .bg(palette.raised)
                            .shadow_lg()
                            .overflow_hidden()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.close_picker(cx);
                                cx.notify();
                            }))
                            .child(picker),
                    ),
            )
            .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .child(div().flex().items_center().gap(px(8.0)).child(button).children(clear))
            .children(overlay)
            .into_any_element()
    }

    fn assistant_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let Some(assistant) = self.assistants.get(self.assistant) else {
            return vec![div().text_color(palette.faint).child("还没有助手").into_any_element()];
        };
        // The levels the bound model accepts; the common ones when unknown.
        let model = self.model.read(cx).value().to_string();
        let model = model.rsplit_once('/').map(|(_, name)| name.to_string()).unwrap_or(model);
        let levels = match self.catalog.lookup(&model) {
            Some(info) => info.levels(),
            None => vec![Reasoning::None, Reasoning::Low, Reasoning::Medium, Reasoning::High],
        };
        let reasoning: Vec<_> = std::iter::once(None).chain(levels.into_iter().map(Some)).enumerate().map(
            |(index, level)| {
                choice(("level", index), reasoning_label(level), level == self.reasoning, palette)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.reasoning = level;
                        cx.notify();
                    }))
            },
        ).collect();
        let model_field = self.model_field(false, cx);
        vec![
            heading(assistant.name.clone(), palette)
                .child(div().flex_1())
                .child(self.delete_button(
                    format!("assistant:{}", assistant.name),
                    cx.listener(|this, _, window, cx| this.delete_assistant(window, cx)),
                ))
                .child(div().text_size(px(12.0)).text_color(palette.muted).child("在面板里显示"))
                .child(switch("enabled", self.enabled, palette).on_click(cx.listener(|this, _, _, cx| {
                    this.enabled = !this.enabled;
                    cx.notify();
                })))
                .into_any_element(),
            field("名称", boxed(Input::new(&self.name).appearance(false), palette), palette),
            field("模型", model_field, palette),
            field("思考强度", div().flex().flex_wrap().gap(px(6.0)).children(reasoning), palette),
            field("图标", boxed(Input::new(&self.icon).appearance(false), palette), palette),
            field("提示词", boxed(Textarea::new(&self.prompt).appearance(false), palette), palette),
            self.footer("save-assistant", cx.listener(|this, _, window, cx| this.save_assistant(window, cx))),
        ]
    }

    fn provider_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let Some(provider) = self.providers.get(self.provider) else {
            return vec![div().text_color(palette.faint).child("还没有渠道").into_any_element()];
        };
        let apis = [
            (Api::ChatCompletions, "Chat Completions"),
            (Api::Responses, "Responses"),
            (Api::Anthropic, "Anthropic Messages"),
            (Api::Gemini, "Gemini"),
        ];
        let strategies = [
            (KeyStrategy::Fallback, "出问题才换下一把"),
            (KeyStrategy::RoundRobin, "轮着用"),
            (KeyStrategy::Sticky, "同一对话用同一把"),
        ];
        let api = apis.into_iter().enumerate().map(|(index, (api, label))| {
            choice(("api", index), label, api == self.api, palette).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.api = api;
                    cx.notify();
                },
            ))
        }).collect::<Vec<_>>();
        let strategy = strategies.into_iter().enumerate().map(|(index, (strategy, label))| {
            choice(("strategy", index), label, strategy == self.strategy, palette).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.strategy = strategy;
                    cx.notify();
                }),
            )
        }).collect::<Vec<_>>();
        let fetched = match (&self.fetched, self.fetching) {
            (_, true) => "正在向渠道要模型列表…".to_string(),
            (None, false) => format!("现有 {} 个对话模型", provider.models.len()),
            (Some(Err(reason)), _) => reason.lines().next().unwrap_or("").chars().take(120).collect(),
            (Some(Ok(upstream)), _) => format!(
                "上游 {} 个，新增 {} 个，下架 {} 个",
                upstream.names.len(),
                upstream.added.len(),
                upstream.gone.len()
            ),
        };
        let can_sync = matches!(&self.fetched, Some(Ok(upstream))
            if !upstream.names.is_empty() && (!upstream.added.is_empty() || !upstream.gone.is_empty()));
        let models = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(button("fetch", "拉取模型列表", false, palette).on_click(cx.listener(|this, _, _, cx| this.fetch(cx))))
            .when(can_sync, |row| {
                row.child(
                    button("sync", "同步到本地", true, palette)
                        .on_click(cx.listener(|this, _, window, cx| this.sync(window, cx))),
                )
            })
            .child(div().flex_1().min_w(px(0.0)).text_size(px(12.0)).text_color(palette.muted).child(fetched));
        vec![
            heading(provider.id.clone(), palette)
                .child(div().flex_1())
                .child(self.delete_button(
                    format!("provider:{}", provider.id),
                    cx.listener(|this, _, window, cx| this.delete_provider(window, cx)),
                ))
                .child(div().text_size(px(12.0)).text_color(palette.muted).child("启用"))
                .child(switch("provider-enabled", self.provider_enabled, palette).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.provider_enabled = !this.provider_enabled;
                        cx.notify();
                    },
                )))
                .into_any_element(),
            field("ID", boxed(Input::new(&self.provider_id).appearance(false), palette), palette),
            field("名称", boxed(Input::new(&self.provider_name).appearance(false), palette), palette),
            field("地址", boxed(Input::new(&self.base_url).appearance(false), palette), palette),
        ]
        .into_iter()
        .chain(match self.tab {
            Tab::Images => self.image_fields(cx),
            Tab::Speech => self.speech_fields(cx),
            _ => vec![field("请求格式", div().flex().flex_wrap().gap(px(6.0)).children(api), palette)],
        })
        .chain([
            field("分组", boxed(Input::new(&self.group).appearance(false), palette), palette),
            field("密钥", self.keys_field(provider, cx), palette),
            field("多把密钥怎么用", div().flex().flex_wrap().gap(px(6.0)).children(strategy), palette),
        ])
        .chain((self.tab == Tab::Chat).then(|| field("模型", models, palette)))
        .chain([self.footer("save-provider", cx.listener(|this, _, window, cx| this.save_provider(window, cx)))])
        .collect()
    }

    /// Paste a new key into the list.
    fn add_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let secret = self.new_key.read(cx).value().trim().to_string();
        if secret.is_empty() {
            return;
        }
        let note = cx.new(|cx| InputState::new(window, cx).placeholder("备注"));
        self.key_rows.push(KeyRow { secret, note, enabled: true });
        self.new_key.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// The stored keys as editable rows; a Vertex provider only takes a fresh
    /// service account.
    fn keys_field(&self, provider: &Provider, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        if provider.vertex {
            let note = if self.stored_keys > 0 {
                "服务账号已存，粘贴新的服务账号 JSON 替换"
            } else {
                "还没有服务账号"
            };
            return div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(boxed(Input::new(&self.new_key).appearance(false), palette))
                .child(div().text_size(px(11.5)).text_color(palette.faint).child(note))
                .into_any_element();
        }
        let count = self.key_rows.len();
        let rows: Vec<AnyElement> = self
            .key_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let up = key_button(("key-up", index), "icons/chevron-up.svg", index > 0, palette)
                    .when(index > 0, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.key_rows.swap(index, index - 1);
                            cx.notify();
                        }))
                    });
                let down = key_button(("key-down", index), "icons/chevron-down.svg", index + 1 < count, palette)
                    .when(index + 1 < count, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.key_rows.swap(index, index + 1);
                            cx.notify();
                        }))
                    });
                div()
                    .h(px(36.0))
                    .flex_none()
                    .rounded(px(8.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .hover(move |style| style.bg(palette.selected))
                    .child(
                        div()
                            .flex_none()
                            .w(px(128.0))
                            .truncate()
                            .font(theme::mono())
                            .text_size(px(11.5))
                            .text_color(palette.muted)
                            .child(mask(&row.secret)),
                    )
                    .child(div().flex_1().min_w(px(0.0)).text_size(px(12.5)).child(
                        Input::new(&row.note).appearance(false),
                    ))
                    .child(switch(("key-enabled", index), row.enabled, palette).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(row) = this.key_rows.get_mut(index) {
                                row.enabled = !row.enabled;
                            }
                            cx.notify();
                        },
                    )))
                    .child(up)
                    .child(down)
                    .child(
                        key_button(("key-remove", index), "icons/x.svg", true, palette).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.key_rows.remove(index);
                                cx.notify();
                            }),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .when(rows.is_empty(), |column| {
                column.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(palette.faint)
                        .child("还没有密钥"),
                )
            })
            .children(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pt(px(2.0))
                    .child(div().flex_1().min_w(px(0.0)).child(
                        boxed(Input::new(&self.new_key).appearance(false), palette),
                    ))
                    .child(
                        button("add-key", "添加", false, palette)
                            .on_click(cx.listener(|this, _, window, cx| this.add_key(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn image_fields(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let apis = [
            (ImageApi::Openai, "OpenAI"),
            (ImageApi::Gemini, "Gemini"),
            (ImageApi::Xai, "Grok"),
            (ImageApi::Evolink, "EvoLink"),
            (ImageApi::Wavespeed, "WaveSpeed"),
        ];
        let apis = apis.into_iter().enumerate().map(|(index, (api, label))| {
            choice(("image-api", index), label, api == self.image_api, palette).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.image_api = api;
                    cx.notify();
                },
            ))
        });
        vec![
            field("生图接口", div().flex().flex_wrap().gap(px(6.0)).children(apis), palette),
            field("生图模型", boxed(Textarea::new(&self.image_models).appearance(false), palette), palette),
        ]
    }

    fn speech_fields(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let apis = [
            (SpeechApi::Elevenlabs, "ElevenLabs"),
            (SpeechApi::Soniox, "Soniox"),
            (SpeechApi::Openai, "OpenAI"),
        ];
        let apis = apis.into_iter().enumerate().map(|(index, (api, label))| {
            choice(("speech-api", index), label, Some(api) == self.speech_api, palette).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.speech_api = Some(api);
                    cx.notify();
                }),
            )
        });
        let can_fetch = self.speech_api == Some(SpeechApi::Elevenlabs);
        let elevenlabs = can_fetch;
        let voice_buttons = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                button("fetch-voices", if self.voice_fetching { "拉取中…" } else { "拉取音色" }, false, palette)
                    .when(elevenlabs, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.fetch_voices(cx)))
                    }),
            )
            .child(
                button("preview-voice", if self.testing { "合成中…" } else { "试听" }, false, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.preview_voice(window, cx))),
            );
        let voice_options = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(self.voice_options.iter().enumerate().map(|(index, voice)| {
                let name =
                    if voice.name.is_empty() { voice.id.clone() } else { format!("{} · {}", voice.name, voice.id) };
                let chosen = voice.id == self.voice.read(cx).value();
                sheet_row(("voice", index), palette, chosen)
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(name))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let id = this.voice_options[index].id.clone();
                        this.voice.update(cx, |input, cx| input.set_value(id, window, cx));
                        cx.notify();
                    }))
            }));
        vec![
            field("语音接口", div().flex().flex_wrap().gap(px(6.0)).children(apis), palette),
            field("语音识别模型", boxed(Textarea::new(&self.asr_models).appearance(false), palette), palette),
            field("语音合成模型", boxed(Textarea::new(&self.tts_models).appearance(false), palette), palette),
            field(
                "音色",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(boxed(Input::new(&self.voice).appearance(false), palette))
                    .child(voice_buttons)
                    .child(voice_options),
                palette,
            ),
        ]
    }

    /// Fetch the provider's voices into the choice list.
    fn fetch_voices(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        self.voice_fetching = true;
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(speech::voices(&provider));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                this.voice_fetching = false;
                match result {
                    Ok(voices) => this.voice_options = voices,
                    Err(reason) => this.status = Some((false, reason)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Synthesize a sample with this provider and play it.
    fn preview_voice(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.testing {
            return;
        }
        let Some(provider) = self.providers.get(self.provider).cloned() else { return };
        let Some(model) = provider.tts_models.first().cloned() else {
            self.status = Some((false, "还没有语音合成模型".into()));
            return cx.notify();
        };
        let voice = self.voice.read(cx).value().trim().to_string();
        let mut provider = provider;
        provider.voice = voice;
        self.testing = true;
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(speech::tts(&provider, &model, "你好，我是缪里。"));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                this.testing = false;
                match result {
                    Ok(wav) => speech::play(wav),
                    Err(reason) => this.status = Some((false, reason)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A 语音识别/语音合成 field: value button plus the matching provider·model
    /// choices, inline below it.
    fn speech_field(&self, kind: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let current = if kind == "asr" { self.speech_asr.clone() } else { self.speech_tts.clone() };
        let open = self.speech_sheet == Some(kind);
        let mut options: Vec<(String, String)> = Vec::new();
        for provider in &self.providers {
            if !provider.enabled || provider.speech_api.is_none() {
                continue;
            }
            let models: &[String] =
                if kind == "asr" { &provider.asr_models } else { &provider.tts_models };
            for model in models {
                options.push((format!("{}/{}", provider.id, model), format!("{} · {}", provider.name, model)));
            }
        }
        let label = current
            .as_deref()
            .and_then(|ref_| {
                let (id, model) = ref_.split_once('/')?;
                let provider = self.providers.iter().find(|p| p.id == id)?;
                Some(format!("{} · {}", provider.name, model))
            })
            .unwrap_or_else(|| "未设置".to_string());
        let mut field_div = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .id(kind)
                    .h(px(36.0))
                    .flex_none()
                    .rounded(px(8.0))
                    .px(px(12.0))
                    .border_1()
                    .border_color(palette.hairline)
                    .bg(palette.surface)
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(move |style| style.border_color(palette.accent))
                    .child(div().flex_1().min_w(px(0.0)).truncate().text_size(px(13.0)).child(label))
                    .child(svg().path("icons/chevron-down.svg").size(px(13.0)).flex_none().text_color(palette.faint))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.speech_sheet = if open { None } else { Some(kind) };
                        cx.notify();
                    })),
            );
        if open {
            let rows = options.into_iter().enumerate().map(|(index, (value, label))| {
                sheet_row(("speech-option", index), palette, Some(&value) == current.as_ref())
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if kind == "asr" { this.speech_asr = Some(value.clone()) } else { this.speech_tts = Some(value.clone()) };
                        this.speech_sheet = None;
                        cx.notify();
                    }))
            });
            let clear = sheet_row(("speech-clear", 0usize), palette, current.is_none())
                .child(div().flex_1().child("不启用"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if kind == "asr" { this.speech_asr = None } else { this.speech_tts = None };
                    this.speech_sheet = None;
                    cx.notify();
                }));
            field_div = field_div.child(
                div()
                    .p(px(4.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(palette.hairline)
                    .bg(palette.surface)
                    .flex()
                    .flex_col()
                    .child(clear)
                    .children(rows),
            );
        }
        field_div.into_any_element()
    }

    fn general_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let themes =
            [(ThemeChoice::System, "跟随系统"), (ThemeChoice::Light, "浅色"), (ThemeChoice::Dark, "深色")];
        let default_model = self.model_field(true, cx);
        let theme = themes.into_iter().enumerate().map(|(index, (theme, label))| {
            choice(("theme", index), label, theme == self.theme, palette).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.theme = theme;
                    cx.notify();
                },
            ))
        });
        vec![
            heading("通用".into(), palette).into_any_element(),
            field("默认模型", default_model, palette),
            field("右键长按多久弹出面板（毫秒）", boxed(Input::new(&self.hold).appearance(false), palette), palette),
            field("弹出面板的快捷键", boxed(Input::new(&self.hotkey).appearance(false), palette), palette),
            field("外观", div().flex().gap(px(6.0)).children(theme), palette),
            field("语音识别", self.speech_field("asr", cx), palette),
            field("语音合成", self.speech_field("tts", cx), palette),
            self.footer("save-general", cx.listener(|this, _, window, cx| this.save_general(window, cx))),
        ]
    }

    /// The save button and what the last save did.
    fn footer(
        &self,
        id: &'static str,
        on_save: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let palette = self.palette;
        div()
            .pt(px(4.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(button(id, "保存", true, palette).on_click(on_save))
            .children(self.status.clone().map(|(ok, text)| {
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .text_color(if ok { palette.muted } else { palette.accent })
                    .child(text)
            }))
            .into_any_element()
    }
}

impl SettingsView {
    /// The window size changed: remember it once the dragging stops.
    fn size_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        if window.is_maximized() {
            return;
        }
        let size = window.viewport_size();
        if size == self.seen_size {
            return;
        }
        let first = self.seen_size.width <= px(0.0);
        self.seen_size = size;
        if first {
            return;
        }
        self.size_generation += 1;
        let generation = self.size_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(600)).await;
            let _ = this.update(cx, |this, _| {
                if this.size_generation == generation {
                    this.save_ui_state();
                }
            });
        })
        .detach();
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.size_changed(window, cx);
        let maximized = window.is_maximized();
        let palette = self.palette;
        let form = match self.tab {
            Tab::Assistants => self.assistant_form(cx),
            Tab::Chat | Tab::Images | Tab::Speech => self.provider_form(cx),
            Tab::General => self.general_form(cx),
        };
        let title = div()
            .h(px(44.0))
            .flex_none()
            .pl(px(18.0))
            .flex()
            .items_center()
            .window_control_area(WindowControlArea::Drag)
            .child(div().font_weight(FontWeight::MEDIUM).text_size(px(13.5)).child("设置"))
            .child(div().flex_1())
            .children([
                (WindowControlArea::Min, "icons/window-minimize.svg"),
                (WindowControlArea::Max, if maximized { "icons/window-restore.svg" } else { "icons/window-maximize.svg" }),
                (WindowControlArea::Close, "icons/window-close.svg"),
            ]
            .into_iter()
            .map(move |(area, icon)| {
                // Windows handles these clicks itself, snap layouts included.
                div()
                    .h_full()
                    .w(px(44.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .window_control_area(area)
                    .when(area == WindowControlArea::Close, |button| {
                        button.hover(|style| style.bg(gpui_kit::rgb(0xC42B1C)).text_color(gpui_kit::rgb(0xFFFFFF)))
                    })
                    .when(area != WindowControlArea::Close, |button| button.hover(move |style| style.bg(palette.selected)))
                    .group("window-button")
                    .child(svg().path(icon).size(px(14.0)).text_color(palette.muted).when(area == WindowControlArea::Close, |icon| {
                        icon.group_hover("window-button", |style| style.text_color(gpui_kit::rgb(0xFFFFFF)))
                    }))
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.surface)
            .text_color(palette.ink)
            .font(theme::sans())
            .text_size(px(13.0))
            .child(title)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(self.nav(cx))
                    .children(self.list(cx))
                    .child(
                        div()
                            .id("form")
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .overflow_y_scroll()
                            .px(px(26.0))
                            .pt(px(6.0))
                            .pb(px(26.0))
                            .flex()
                            .flex_col()
                            .gap(px(18.0))
                            .children(form),
                    ),
            )
    }
}

fn reasoning_label(level: Option<Reasoning>) -> &'static str {
    match level {
        None => "模型默认",
        Some(Reasoning::None) => "关闭",
        Some(Reasoning::Minimal) => "极低",
        Some(Reasoning::Low) => "低",
        Some(Reasoning::Medium) => "中",
        Some(Reasoning::High) => "高",
        Some(Reasoning::Xhigh) => "很高",
        Some(Reasoning::Max) => "最高",
    }
}

/// A row of a list or of the navigation.
fn row(id: impl Into<ElementId>, palette: Palette, open: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .flex_none()
        .px(px(9.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .cursor_pointer()
        .when(open, |row| row.bg(palette.selected))
        .hover(move |style| style.bg(palette.selected))
}

fn heading(text: String, palette: Palette) -> Div {
    div().flex().items_center().gap(px(10.0)).child(
        div().font(theme::serif()).text_size(px(21.0)).text_color(palette.ink).child(text),
    )
}

/// A labelled part of a form.
fn field(label: &'static str, content: impl IntoElement, palette: Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(div().text_size(px(12.0)).text_color(palette.muted).child(label))
        .child(content)
        .into_any_element()
}

/// The frame around a text input.
fn boxed(input: impl IntoElement, palette: Palette) -> Div {
    div()
        .rounded(px(10.0))
        .border_1()
        .border_color(palette.hairline)
        .bg(palette.raised)
        .px(px(4.0))
        .py(px(2.0))
        .text_size(px(13.0))
        .line_height(relative(1.5))
        .child(input)
}

/// One of several options, of which one is chosen.
fn choice(id: impl Into<ElementId>, label: &'static str, chosen: bool, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(if chosen { palette.accent } else { palette.hairline })
        .when(chosen, |choice| choice.bg(palette.accent_wash).text_color(palette.accent))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(12.5))
        .hover(move |style| style.bg(palette.selected))
        .child(label)
}

fn button(id: &'static str, label: &'static str, primary: bool, palette: Palette) -> Stateful<Div> {
    let label: SharedString = label.into();
    div()
        .id(id)
        .h(px(32.0))
        .flex_none()
        .px(px(16.0))
        .rounded(px(9.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(12.5))
        .when(primary, |button| button.bg(palette.accent).text_color(gpui_kit::rgb(0xFFFFFF)))
        .when(!primary, |button| button.border_1().border_color(palette.hairline))
        .hover(|style| style.opacity(0.88))
        .child(label)
}

/// A key is never shown beyond its first and last four characters.
fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    let n = chars.len();
    if n > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[n - 4..].iter().collect();
        format!("{head}…{tail}")
    } else if n > 4 {
        let tail: String = chars[n - 4..].iter().collect();
        format!("…{tail}")
    } else {
        "…".to_string()
    }
}

/// A tiny square icon button for the key rows.
fn key_button(id: impl Into<ElementId>, icon: &'static str, enabled: bool, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .when(enabled, |button| button.cursor_pointer().hover(move |style| style.bg(palette.accent_wash)))
        .child(
            svg()
                .path(icon)
                .size(px(13.0))
                .flex_none()
                .text_color(if enabled { palette.muted } else { palette.hairline }),
        )
}

fn switch(id: impl Into<ElementId>, on: bool, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(36.0))
        .h(px(20.0))
        .flex_none()
        .rounded_full()
        .p(px(2.0))
        .flex()
        .cursor_pointer()
        .bg(if on { palette.accent } else { palette.hairline })
        .when(on, |switch| switch.justify_end())
        .child(div().size(px(16.0)).rounded_full().bg(gpui_kit::rgb(0xFFFFFF)))
}
