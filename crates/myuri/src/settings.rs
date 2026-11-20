//! The settings window: assistants and their prompts, providers and their
//! keys, and the general options. It edits the same files a text editor
//! would, so the running program picks the changes up by itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Div, ElementId, Entity,
    FontWeight, InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, TitlebarOptions, Window, WindowBounds,
    WindowControlArea, WindowOptions, anchored, deferred, div, point, px, relative, size, svg,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindowAsync};

use myuri_core::catalog::{Catalog, Kind};
use myuri_core::config::{
    self, Api, Assistant, ImageProvider, KeyNote, KeyStrategy, ModelRef, Provider, Purpose, Reasoning, Settings,
    SpeechApi, SpeechProvider, ThemeChoice,
};
use myuri_core::image::{ImageApi, ImageSpec};
use myuri_core::edit::{self, Field};
use myuri_core::llm;
use myuri_core::speech::Voice;
use myuri_core::uistate::{self, UiState};

use crate::autostart::{self, Status as AutostartStatus};
use crate::icon::Glyph;
use crate::speech;
use crate::models::{self, Upstream};
use crate::picker::{ModelPicker, PickerEvent, sheet_row};
use crate::secrets;
use crate::theme::{self, Palette};
use crate::updates::{self, ReleaseStatus};

mod speech_form;
mod catalog_form;
use speech_form::SpeechInputs;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Assistants,
    Chat,
    Images,
    Speech,
    General,
    History,
    Sync,
    Tools,
}

impl Tab {
    fn channel(self) -> Option<usize> {
        match self {
            Self::Chat => Some(0),
            Self::Images => Some(1),
            Self::Speech => Some(2),
            _ => None,
        }
    }

    fn provider_file(self) -> &'static str {
        match self {
            Self::Images => config::IMAGE_PROVIDERS_FILE,
            Self::Speech => config::SPEECH_PROVIDERS_FILE,
            _ => config::PROVIDERS_FILE,
        }
    }

    fn namespace(self) -> &'static str {
        match self {
            Self::Images => secrets::NS_IMAGE,
            Self::Speech => secrets::NS_SPEECH,
            _ => secrets::NS_CHAT,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Assistants => "assistants",
            Self::Chat => "chat",
            Self::Images => "images",
            Self::Speech => "speech",
            Self::General => "general",
            Self::History => "history",
            Self::Sync => "sync",
            Self::Tools => "tools",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "assistants" => Self::Assistants,
            "chat" => Self::Chat,
            "images" => Self::Images,
            "speech" => Self::Speech,
            "general" => Self::General,
            "history" => Self::History,
            "sync" => Self::Sync,
            "tools" => Self::Tools,
            _ => return None,
        })
    }

    /// The page is a view of its own instead of a list and a form.
    fn own_page(self) -> bool {
        matches!(self, Self::History | Self::Sync | Self::Tools)
    }
}

/// One stored key of the open provider.
struct KeyRow {
    secret: String,
    note: Entity<InputState>,
    enabled: bool,
}

/// Which slice of fetched upstream models is shown in the preview list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpstreamFilter {
    All,
    Added,
    Gone,
}

/// What is being dragged in a list.
#[derive(Clone)]
struct DragProvider(usize, SharedString, Tab);
#[derive(Clone)]
struct DragGroup(String, Tab);
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

#[derive(Clone, Copy)]
struct ChannelRow<'a> {
    id: &'a str,
    name: &'a str,
    group: &'a str,
    base_url: &'a str,
    icon: &'a str,
    enabled: bool,
    model_count: usize,
    keys: &'a [KeyNote],
    strategy: KeyStrategy,
    vertex: bool,
}

pub struct SettingsView {
    dir: PathBuf,
    palette: Palette,
    catalog: std::sync::Arc<Catalog>,
    tab: Tab,
    assistants: Vec<Assistant>,
    providers: Vec<Provider>,
    image_providers: Vec<ImageProvider>,
    speech_providers: Vec<SpeechProvider>,
    settings: Settings,
    /// Index of the assistant and the provider being edited.
    assistant: usize,
    selected_channels: [usize; 3],
    /// Provider groups that are folded in the list.
    folded: [BTreeSet<String>; 3],
    /// Filters the list in the middle column.
    search: Entity<InputState>,
    channel_search: [Entity<InputState>; 3],
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
    picker_subscription: Option<Subscription>,
    picker_open: bool,
    /// The picker is choosing the default model rather than an assistant's.
    picker_default: bool,
    history: Entity<crate::historyview::HistoryView>,
    sync: Entity<crate::syncview::SyncView>,
    tools_page: Entity<crate::toolsview::ToolsView>,
    /// The open assistant's MCP servers and skills, as they will be saved.
    tools: Vec<String>,
    skills: Vec<String>,
    /// What there is to choose from.
    server_names: Vec<String>,
    skill_names: Vec<String>,
    /// `[models] default`: what assistants without a model use.
    default_model: Entity<InputState>,
    prompt: Entity<TextareaState>,
    enabled: bool,
    reasoning: Option<Reasoning>,

    provider_id: Entity<InputState>,
    provider_name: Entity<InputState>,
    base_url: Entity<InputState>,
    group: Entity<InputState>,
    provider_icon: Entity<InputState>,
    /// The keys of the open provider, in the order the credential stores them.
    key_rows: Vec<KeyRow>,
    /// Appends a key for a normal provider, replaces the JSON for a Vertex one.
    new_key: Entity<InputState>,
    api: Api,
    image_api: ImageApi,
    speech_api: SpeechApi,
    vertex: bool,
    image_size: String,
    image_resolution: String,
    image_quality: String,
    image_format: String,
    speech_inputs: SpeechInputs,
    form_generation: u64,
    chat_models: Entity<TextareaState>,
    image_models: Entity<TextareaState>,
    asr_models: Entity<TextareaState>,
    tts_models: Entity<TextareaState>,
    /// Filters the fetched upstream model preview list.
    model_filter: Entity<InputState>,
    upstream_filter: UpstreamFilter,
    /// Model id whose copy button was just clicked.
    copied_model: Option<String>,
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
    official_vendor: Option<String>,

    autostart_status: AutostartStatus,
    catalog_updated: Option<std::time::SystemTime>,
    catalog_refreshing: bool,
    catalog_notice: Option<(bool, String)>,
    checking_release: bool,
    release_status: Option<Result<ReleaseStatus, String>>,

    hold: Entity<InputState>,
    hotkey: Entity<InputState>,
    theme: ThemeChoice,
    /// The window size the last render saw; changes are saved, debounced.
    seen_size: gpui_kit::Size<gpui_kit::Pixels>,
    size_generation: u64,
}

/// Open the settings window. Returns its handle, for bringing it to the
/// front the next time.
pub fn open(
    dir: PathBuf,
    dark: bool,
    page: Option<&'static str>,
    cx: &mut App,
) -> Option<(gpui_kit::AnyWindowHandle, gpui_kit::WeakEntity<SettingsView>)> {
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
        cx.new(|cx| {
            let mut view = SettingsView::new(dir, dark, window, cx);
            if let Some(page) = page {
                view.show_page(page, window, cx);
            }
            view
        })
    })
    .ok()
    .map(|(handle, view)| (handle, view.downgrade()))
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
        let provider_icon = line("自动匹配；可填 openai、gemini-color 这样的名字、图片路径或网址", window, cx);
        let new_key =
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("粘贴密钥或服务账号 JSON"));
        let hold = line("220", window, cx);
        let hotkey = line("Ctrl+Alt+Space", window, cx);
        let default_model = line("", window, cx);
        let name = line("助手的名字", window, cx);
        let search = line("搜索", window, cx);
        let channel_search = std::array::from_fn(|_| line("搜索", window, cx));
        let model_filter = line("筛选上游模型 ID", window, cx);
        let prompt = cx.new(|cx| TextareaState::new(window, cx).auto_grow(8, 22).placeholder("系统提示词"));
        let mut list = |hint: &'static str| {
            cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 14).placeholder(hint))
        };
        let chat_models = list("一行一个模型 ID");
        let image_models = list("一行一个模型 ID");
        let asr_models = list("一行一个模型名");
        let tts_models = list("一行一个模型名");
        let mut subscriptions = vec![
            cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&model_filter, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&chat_models, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&image_models, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let voice = line("语音的音色，如声音 id", window, cx);
        let speech_inputs = SpeechInputs::new(window, cx);
        for search in &channel_search {
            subscriptions.push(cx.subscribe_in(search, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_theme(this.theme, window, cx);
        }));
        let remembered = uistate::load(&dir);
        let history = {
            let dir = dir.clone();
            cx.new(|cx| crate::historyview::HistoryView::new(dir, theme::palette(dark), window, cx))
        };
        let sync = {
            let dir = dir.clone();
            cx.new(|cx| crate::syncview::SyncView::new(dir, theme::palette(dark), window, cx))
        };
        let restored = cx.subscribe_in(&sync, window, |this, _, _: &crate::syncview::Restored, window, cx| {
            this.reload(window, cx);
            cx.notify();
        });
        subscriptions.push(restored);
        let tools_page = {
            let dir = dir.clone();
            cx.new(|cx| crate::toolsview::ToolsView::new(dir, theme::palette(dark), window, cx))
        };
        let mut view = Self {
            history,
            sync,
            tools_page,
            tools: Vec::new(),
            skills: Vec::new(),
            server_names: Vec::new(),
            skill_names: Vec::new(),
            catalog: std::sync::Arc::new(models::load(&dir)),
            catalog_updated: models::updated_at(&dir),
            catalog_refreshing: false,
            catalog_notice: None,
            checking_release: false,
            release_status: None,
            autostart_status: autostart::status(),
            dir,
            palette: theme::palette(dark),
            tab: Tab::named(&remembered.page).unwrap_or(Tab::Assistants),
            assistants: Vec::new(),
            providers: Vec::new(),
            image_providers: Vec::new(),
            speech_providers: Vec::new(),
            settings: Settings::default(),
            assistant: 0,
            selected_channels: [0; 3],
            folded: [
                remembered.folded.into_iter().collect(),
                remembered.folded_images.into_iter().collect(),
                remembered.folded_speech.into_iter().collect(),
            ],
            search,
            channel_search,
            _subscriptions: subscriptions,
            name,
            status: None,
            confirm: None,
            icon,
            model,
            picker: None,
            picker_subscription: None,
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
            provider_icon,
            key_rows: Vec::new(),
            new_key,
            api: Api::ChatCompletions,
            image_api: ImageApi::Openai,
            speech_api: SpeechApi::default(),
            vertex: false,
            image_size: String::new(),
            image_resolution: String::new(),
            image_quality: String::new(),
            image_format: String::new(),
            speech_inputs,
            form_generation: 0,
            chat_models,
            image_models,
            asr_models,
            tts_models,
            model_filter,
            upstream_filter: UpstreamFilter::Added,
            copied_model: None,
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
            official_vendor: None,
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
        let width = f32::from(self.seen_size.width);
        let height = f32::from(self.seen_size.height);
        let state = UiState {
            folded: self.folded[0].iter().cloned().collect(),
            folded_images: self.folded[1].iter().cloned().collect(),
            folded_speech: self.folded[2].iter().cloned().collect(),
            window: if width > 0.0 && height > 0.0 {
                uistate::UiWindow { width, height }
            } else {
                uistate::load(&self.dir).window
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
        match config::load_image_providers(&self.dir) {
            Ok(providers) => self.image_providers = providers,
            Err(problem) => problems.push(problem.to_string()),
        }
        match config::load_speech_providers(&self.dir) {
            Ok(providers) => self.speech_providers = providers,
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
        for (index, count) in [self.providers.len(), self.image_providers.len(), self.speech_providers.len()]
            .into_iter().enumerate()
        {
            self.selected_channels[index] = self.selected_channels[index].min(count.saturating_sub(1));
        }
        self.fill_assistant(window, cx);
        self.fill_provider(window, cx);
        let hold = self.settings.gesture.hold_ms.to_string();
        let hotkey = self.settings.hotkey.panel.clone();
        self.hold.update(cx, |input, cx| input.set_value(hold, window, cx));
        let default_model = self.settings.models.get("default").cloned().unwrap_or_default();
        self.default_model.update(cx, |input, cx| input.set_value(default_model, window, cx));
        self.hotkey.update(cx, |input, cx| input.set_value(hotkey, window, cx));
        self.apply_theme(self.settings.panel.theme, window, cx);
        self.speech_asr = self.settings.speech.asr.clone();
        self.speech_tts = self.settings.speech.tts.clone();
        self.autostart_status = autostart::status();
        self.catalog_updated = models::updated_at(&self.dir);
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
        self.tools = assistant.tools.clone();
        self.skills = assistant.skills.clone();
        self.load_tool_names();
        self.name.update(cx, |input, cx| input.set_value(assistant.name.clone(), window, cx));
        self.icon.update(cx, |input, cx| input.set_value(assistant.icon.clone(), window, cx));
        let model = assistant.model.clone().unwrap_or_default();
        self.model.update(cx, |input, cx| input.set_value(model, window, cx));
        self.prompt.update(cx, |input, cx| input.set_value(prompt, window, cx));
        self.enabled = assistant.enabled;
        self.reasoning = assistant.reasoning;
    }

    /// After switching pages, show a provider that belongs on the page.
    fn channel_rows(&self) -> Vec<ChannelRow<'_>> {
        macro_rules! row {
            ($p:ident, $count:expr, $vertex:expr) => {
                ChannelRow {
                    id: &$p.id, name: &$p.name, group: &$p.group, base_url: &$p.base_url,
                    icon: &$p.icon, enabled: $p.enabled, model_count: $count,
                    keys: &$p.keys, strategy: $p.key_strategy, vertex: $vertex,
                }
            };
        }
        match self.tab {
            Tab::Chat => self.providers.iter().map(|p| row!(p, p.models.len(), p.vertex)).collect(),
            Tab::Images => self.image_providers.iter().map(|p| row!(p, p.models.len(), p.vertex)).collect(),
            Tab::Speech => self.speech_providers.iter().map(|p| row!(p, p.asr_models.len() + p.tts_models.len(), false)).collect(),
            _ => Vec::new(),
        }
    }

    fn channel_row(&self) -> Option<ChannelRow<'_>> {
        let index = self.selected_channels[self.tab.channel()?];
        self.channel_rows().get(index).copied()
    }

    fn fill_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.close_picker(cx);
        self.form_generation += 1;
        self.fetched = None;
        self.fetching = false;
        self.official_vendor = None;
        self.voice_options.clear();
        self.voice_fetching = false;
        self.testing = false;
        let Some(channel) = self.tab.channel() else { return };
        let Some(provider) = self.channel_row() else { return };
        let (id, name, url, group, icon, keys, strategy, enabled, vertex) = (
            provider.id.to_string(), provider.name.to_string(), provider.base_url.to_string(),
            provider.group.to_string(), provider.icon.to_string(), provider.keys.to_vec(),
            provider.strategy, provider.enabled, provider.vertex,
        );
        for (input, text) in [
            (&self.provider_id, id.as_str()), (&self.provider_name, &name), (&self.base_url, &url),
            (&self.group, &group), (&self.provider_icon, &icon), (&self.new_key, ""),
        ] {
            input.update(cx, |input, cx| input.set_value(text.to_string(), window, cx));
        }
        let index = self.selected_channels[channel];
        match self.tab {
            Tab::Chat => {
                let p = &self.providers[index];
                self.api = p.api;
                self.chat_models.update(cx, |input, cx| input.set_value(p.models.join("\n"), window, cx));
            }
            Tab::Images => {
                let p = &self.image_providers[index];
                self.image_api = p.api;
                self.image_size.clone_from(&p.default_size);
                self.image_resolution.clone_from(&p.default_resolution);
                self.image_quality.clone_from(&p.default_quality);
                self.image_format.clone_from(&p.default_format);
                self.image_models.update(cx, |input, cx| input.set_value(p.models.join("\n"), window, cx));
            }
            Tab::Speech => {
                let p = &self.speech_providers[index];
                self.speech_api = p.api;
                self.voice.update(cx, |input, cx| input.set_value(p.voice.clone(), window, cx));
                self.speech_inputs.fill(p, window, cx);
                self.asr_models.update(cx, |input, cx| input.set_value(p.asr_models.join("\n"), window, cx));
                self.tts_models.update(cx, |input, cx| input.set_value(p.tts_models.join("\n"), window, cx));
            }
            _ => {}
        }
        self.model_filter.update(cx, |input, cx| input.set_value("", window, cx));
        self.upstream_filter = UpstreamFilter::Added;
        self.copied_model = None;
        self.strategy = strategy;
        self.provider_enabled = enabled;
        self.vertex = vertex;
        let stored = secrets::read_in(self.tab.namespace(), &id).unwrap_or_default();
        self.stored_keys = if vertex { usize::from(!stored.is_empty()) } else { llm::split_keys(&stored).len() };
        self.key_rows = if vertex {
            Vec::new()
        } else {
            llm::split_keys(&stored)
                .into_iter()
                .enumerate()
                .map(|(index, secret)| {
                    let note = cx.new(|cx| InputState::new(window, cx).placeholder("备注"));
                    let text = keys.get(index).map(|key| key.note.clone()).unwrap_or_default();
                    note.update(cx, |note, cx| note.set_value(text, window, cx));
                    KeyRow {
                        secret: secret.to_string(),
                        note,
                        enabled: keys.get(index).is_none_or(|key| key.enabled),
                    }
                })
                .collect()
        };
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
            ("tools", Field::List(self.tools.clone())),
            ("skills", Field::List(self.skills.clone())),
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

    fn read_input(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_string()
    }

    fn draft_keys(&self, cx: &App) -> (Option<String>, Vec<KeyNote>) {
        let pasted = Self::read_input(&self.new_key, cx);
        if self.vertex {
            return ((!pasted.is_empty()).then_some(pasted), Vec::new());
        }
        let mut keys: Vec<String> = self.key_rows.iter().map(|row| row.secret.clone()).collect();
        let mut notes: Vec<_> = self.key_rows.iter().map(|row| KeyNote {
            note: Self::read_input(&row.note, cx),
            enabled: row.enabled,
        }).collect();
        for key in llm::split_keys(&pasted) {
            if !keys.iter().any(|existing| existing == key) {
                keys.push(key.to_string());
                notes.push(KeyNote { note: String::new(), enabled: true });
            }
        }
        (Some(keys.join(",")), notes)
    }

    fn provider_changes(&self, cx: &App, notes: &[KeyNote]) -> Result<Vec<(&'static str, Field)>, String> {
        let read = |input: &Entity<InputState>| Self::read_input(input, cx);
        let strategy = match self.strategy {
            KeyStrategy::Fallback => Field::Absent,
            KeyStrategy::RoundRobin => Field::Text("round_robin".into()),
            KeyStrategy::Sticky => Field::Text("sticky".into()),
        };
        let mut changes = vec![
            ("name", Field::optional(&read(&self.provider_name))),
            ("base_url", Field::Text(read(&self.base_url))),
            ("group", Field::optional(&read(&self.group))),
            ("icon", Field::optional(&read(&self.provider_icon))),
            ("key_strategy", strategy),
            ("enabled", if self.provider_enabled { Field::Absent } else { Field::Flag(false) }),
        ];
        let keys: Vec<_> = notes.iter().map(|note| {
            let mut fields = Vec::new();
            if !note.note.is_empty() {
                fields.push(("note", Field::Text(note.note.clone())));
            }
            if !note.enabled {
                fields.push(("enabled", Field::Flag(false)));
            }
            fields
        }).collect();
        let keys = if keys.iter().all(Vec::is_empty) { Field::Absent } else { Field::Table(keys) };
        changes.push(("keys", keys));
        match self.tab {
            Tab::Images => {
                let models = lines_of(&self.image_models, cx);
                let spec = ImageSpec::for_model(self.image_api, models.first().map(String::as_str).unwrap_or(""));
                let keep_pair = |value: &str, allowed: &[(&str, &str)]| {
                    if allowed.iter().any(|(option, _)| *option == value) {
                        Field::optional(value)
                    } else {
                        Field::Absent
                    }
                };
                let keep_str = |value: &str, allowed: &[&str]| {
                    if allowed.contains(&value) {
                        Field::optional(value)
                    } else {
                        Field::Absent
                    }
                };
                changes.extend([
                    ("api", Field::Text(self.image_api.as_str().into())),
                    ("models", Field::List(models)),
                    ("vertex", if self.vertex { Field::Flag(true) } else { Field::Absent }),
                    ("default_size", keep_pair(&self.image_size, spec.sizes)),
                    ("default_resolution", keep_str(&self.image_resolution, spec.resolutions)),
                    ("default_quality", keep_pair(&self.image_quality, spec.qualities)),
                    ("default_format", keep_str(&self.image_format, spec.formats)),
                ]);
            }
            Tab::Speech => {
                let options = self.speech_inputs.options(self.speech_api, cx)?;
                let number = |value: Option<f32>| value.map_or(Field::Absent, |n| Field::Float(f64::from(n)));
                changes.extend([
                    ("api", Field::Text(self.speech_api.as_str().into())),
                    ("asr_models", Field::List(lines_of(&self.asr_models, cx))),
                    ("tts_models", Field::List(lines_of(&self.tts_models, cx))),
                    ("voice", Field::optional(&read(&self.voice))),
                    ("stability", number(options.stability)),
                    ("similarity_boost", number(options.similarity_boost)),
                    ("style", number(options.style)),
                    ("speed", number(options.speed)),
                    ("language", Field::optional(&options.language)),
                    ("output_format", Field::optional(&options.output_format)),
                ]);
            }
            _ => {
                let api = match self.api {
                    Api::ChatCompletions => "chat_completions",
                    Api::Responses => "responses",
                    Api::Anthropic => "anthropic",
                    Api::Gemini => "gemini",
                };
                changes.extend([
                    ("api", Field::Text(api.into())),
                    ("models", Field::List(lines_of(&self.chat_models, cx))),
                ]);
            }
        }
        Ok(changes)
    }

    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.channel_row() else { return };
        let id = provider.id.to_string();
        let new_id = Self::read_input(&self.provider_id, cx);
        let outcome = self.write_provider(&id, &new_id, provider.vertex, cx);
        let saved = outcome.is_ok();
        self.finish(outcome, window, cx);
        if saved && id != new_id {
            self.select_provider(&new_id, window, cx);
        }
    }

    fn write_provider(&self, old: &str, new: &str, was_vertex: bool, cx: &App) -> Result<(), String> {
        if !config::valid_provider_id(new) {
            return Err("渠道 id 只能用字母、数字、-、_ 和 .".into());
        }
        let renaming = old != new;
        if renaming && self.channel_rows().iter().any(|other| other.id == new) {
            return Err(format!("已经有叫“{new}”的渠道了"));
        }
        let (secret, notes) = self.draft_keys(cx);
        if self.vertex && !was_vertex && secret.is_none() && self.stored_keys > 0 {
            return Err("切换到 Vertex 时请填写服务账号 JSON".into());
        }
        let changes = self.provider_changes(cx, &notes)?;
        let file = self.tab.provider_file();
        let read = |file| fs::read_to_string(self.dir.join(file)).map_err(|error| error.to_string());
        let source = read(file)?;
        let mut writes = Vec::new();
        let renamed = match (self.tab, renaming) {
            (Tab::Chat, true) => {
                let assistants = read(config::ASSISTANTS_FILE)?;
                let settings = read(config::SETTINGS_FILE)?;
                let (providers, next_assistants, next_settings) = edit::rename_provider(&source, &assistants, &settings, old, new)?;
                writes.push((config::ASSISTANTS_FILE, assistants, next_assistants));
                writes.push((config::SETTINGS_FILE, settings, next_settings));
                providers
            }
            (Tab::Images, true) => {
                let settings = read(config::SETTINGS_FILE)?;
                let (providers, next_settings) = edit::rename_image_provider(&source, &settings, old, new)?;
                writes.push((config::SETTINGS_FILE, settings, next_settings));
                providers
            }
            (Tab::Speech, true) => {
                let settings = read(config::SETTINGS_FILE)?;
                let (providers, next_settings) = edit::rename_speech_provider(&source, &settings, old, new)?;
                writes.push((config::SETTINGS_FILE, settings, next_settings));
                providers
            }
            _ => source.clone(),
        };
        let updated = edit::set_entry(&renamed, "provider", "id", new, &changes)?;
        writes.insert(0, (file, source, updated));
        for (file, _, updated) in &writes {
            Self::check_file(file, updated)?;
        }

        let namespace = self.tab.namespace();
        let previous = secrets::read_in(namespace, new);
        let secret = if renaming && secret.is_none() { secrets::read_in(namespace, old) } else { secret };
        let set_key = |id, value: Option<&str>| match value {
            Some(key) if !key.is_empty() => secrets::write_in(namespace, id, key),
            _ => secrets::delete_in(namespace, id).map(|_| ()),
        };
        let key_changed = renaming || secret.is_some();
        if key_changed {
            set_key(new, secret.as_deref())?;
        }
        for (index, (file, _, updated)) in writes.iter().enumerate() {
            if let Err(error) = fs::write(self.dir.join(file), updated) {
                let mut failures = Vec::new();
                for (file, original, _) in writes[..=index].iter().rev() {
                    if let Err(error) = fs::write(self.dir.join(file), original) {
                        failures.push(format!("{file}：{error}"));
                    }
                }
                if key_changed
                    && let Err(error) = set_key(new, previous.as_deref())
                {
                    failures.push(error);
                }
                return Err(if failures.is_empty() { error.to_string() } else {
                    format!("{error}。恢复原配置失败：{}", failures.join("；"))
                });
            }
        }
        if renaming && !old.eq_ignore_ascii_case(new) {
            secrets::delete_in(namespace, old)?;
        }
        Ok(())
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
        self.write_checked(file, change(source), |text| Self::check_file(file, text))
    }

    fn check_file(file: &str, text: &str) -> Result<(), String> {
        let text = text.trim_start_matches('\u{feff}');
        match file {
            config::ASSISTANTS_FILE => config::parse_assistants(text).map(|_| ()),
            config::SETTINGS_FILE => config::parse_settings(text).map(|_| ()),
            config::IMAGE_PROVIDERS_FILE => config::parse_image_providers(text).map(|_| ()),
            config::SPEECH_PROVIDERS_FILE => config::parse_speech_providers(text).map(|_| ()),
            _ => config::parse_providers(text).map(|_| ()),
        }
    }

    /// Show the provider with this id again after the list changed.
    fn select_provider(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.tab.channel() else { return };
        if let Some(index) = self.channel_rows().iter().position(|provider| provider.id == id) {
            self.selected_channels[channel] = index;
            self.fill_provider(window, cx);
        }
    }

    /// Put provider `from` next to provider `to`, in `to`'s group.
    fn drop_provider(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        let channels = self.channel_rows();
        let (Some(dragged), Some(target)) = (channels.get(from), channels.get(to)) else {
            return;
        };
        if from == to {
            return;
        }
        let order = edit::moved(channels.len(), &[from], to, from < to);
        let id = dragged.id.to_string();
        let new_group = (dragged.group != target.group).then(|| target.group.to_string());
        self.place_provider(&id, &order, new_group.as_deref(), window, cx);
    }

    /// Put provider `from` at the top of `group`.
    fn drop_into_group(&mut self, from: usize, group: &str, window: &mut Window, cx: &mut Context<Self>) {
        let providers = self.channel_rows();
        let Some(dragged) = providers.get(from) else { return };
        let Some(first) = providers.iter().position(|provider| provider.group == group) else { return };
        let order = edit::moved(providers.len(), &[from], first, false);
        let id = dragged.id.to_string();
        let new_group = (dragged.group != group).then_some(group);
        self.place_provider(&id, &order, new_group, window, cx);
    }

    fn place_provider(
        &mut self,
        id: &str,
        order: &[usize],
        group: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let outcome = self.rewrite(self.tab.provider_file(), |source| {
            let source = edit::reorder(&source, "provider", order)?;
            match group {
                Some(group) => edit::set_entry(&source, "provider", "id", id, &[("group", Field::optional(group))]),
                None => Ok(source),
            }
        });
        self.finish(outcome, window, cx);
        self.select_provider(id, window, cx);
    }

    /// Move a whole group next to another one.
    fn drop_group(&mut self, dragged: &str, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        let providers = self.channel_rows();
        let members = |name: &str| {
            providers.iter().enumerate().filter(|(_, p)| p.group == name).map(|(index, _)| index).collect::<Vec<_>>()
        };
        let (block, aim) = (members(dragged), members(target));
        let (Some(&start), Some(&first), Some(&last)) = (block.first(), aim.first(), aim.last()) else { return };
        if dragged == target {
            return;
        }
        let order = if start < first {
            edit::moved(providers.len(), &block, last, true)
        } else {
            edit::moved(providers.len(), &block, first, false)
        };
        let id = self.channel_row().map(|provider| provider.id.to_string());
        let outcome = self.rewrite(self.tab.provider_file(), |source| edit::reorder(&source, "provider", &order));
        self.finish(outcome, window, cx);
        if let Some(id) = id {
            self.select_provider(&id, window, cx);
        }
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
        let Some(id) = self.channel_row().map(|provider| provider.id.to_string()) else { return };
        let users = match self.channel_users(&id) {
            Ok(users) => users,
            Err(error) => {
                self.status = Some((false, error.to_string()));
                return cx.notify();
            }
        };
        if !users.is_empty() {
            self.status = Some((false, format!("{} 还在用这个渠道，先给它们换个模型", users.join("、"))));
            cx.notify();
            return;
        }
        if !self.armed(format!("provider:{id}"), cx) {
            return;
        }
        let outcome = self
            .rewrite(self.tab.provider_file(), |source| edit::remove_entry(&source, "provider", "id", &id))
            .and_then(|()| secrets::delete_in(self.tab.namespace(), &id).map(|_| ()));
        self.finish(outcome, window, cx);
    }

    fn channel_users(&self, id: &str) -> Result<Vec<String>, config::Problem> {
        let prefix = format!("{id}/");
        match self.tab {
            Tab::Chat => {
                let assistants = config::load_assistants(&self.dir)?;
                let settings = config::load_settings(&self.dir)?;
                Ok(assistants.into_iter()
                    .filter(|assistant| assistant.model.as_deref().is_some_and(|model| model.starts_with(&prefix)))
                    .map(|assistant| assistant.name)
                    .chain(settings.models.iter().filter(|(_, model)| model.starts_with(&prefix))
                        .map(|(tier, _)| format!("模型档位 {tier}")))
                    .collect())
            }
            Tab::Images => {
                let settings = config::load_settings(&self.dir)?.image;
                Ok(settings
                    .default
                    .is_some_and(|model| model.starts_with(&prefix))
                    .then(|| "默认生图".to_string())
                    .into_iter()
                    .collect())
            }
            Tab::Speech => {
                let settings = config::load_settings(&self.dir)?.speech;
                Ok([("语音识别", settings.asr), ("语音合成", settings.tts)].into_iter()
                    .filter_map(|(label, route)| route.is_some_and(|model| model.starts_with(&prefix)).then(|| label.to_string()))
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
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
        let Some(channel) = self.tab.channel() else { return };
        let members: Vec<String> = self.channel_rows().iter().filter(|provider| provider.group == group)
            .map(|provider| provider.id.to_string()).collect();
        let outcome = self.rewrite(self.tab.provider_file(), |mut source| {
            for id in &members {
                source = edit::set_entry(&source, "provider", "id", id, &[("group", Field::Absent)])?;
            }
            Ok(source)
        });
        if outcome.is_ok() {
            self.folded[channel].remove(group);
            self.save_ui_state();
        }
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
        let providers = self.channel_rows();
        let id = (1..).map(|n| format!("channel-{n}")).find(|id| !providers.iter().any(|p| p.id == id)).unwrap_or_default();
        let name = numbered("新渠道", &|name| providers.iter().any(|p| p.name == name));
        let entry = format!("\n[[provider]]\nid = {id:?}\nname = {name:?}\nbase_url = \"https://api.example.com/v1\"\n");
        let outcome = self.rewrite(self.tab.provider_file(), |source| Ok(format!("{}\n{entry}", source.trim_end())));
        self.finish(outcome, window, cx);
        self.select_provider(&id, window, cx);
    }

    fn current_models(&self, cx: &App) -> Vec<String> {
        let input = match self.tab {
            Tab::Images => &self.image_models,
            _ => &self.chat_models,
        };
        lines_of(input, cx)
    }

    fn set_current_models(&mut self, models: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let text = models.join("\n");
        let input = match self.tab {
            Tab::Images => &self.image_models,
            _ => &self.chat_models,
        };
        input.update(cx, |input, cx| input.set_value(text, window, cx));
        cx.notify();
    }

    /// Ask the provider for its model list, off the UI thread, using the
    /// current form draft so unsaved URL / API / key changes can be tested.
    fn draft_url(&self, cx: &App) -> String {
        Self::read_input(&self.base_url, cx).trim_end_matches('/').to_string()
    }

    fn speech_draft(&self, cx: &App) -> Result<(SpeechProvider, Option<String>), String> {
        let index = self.selected_channels[2];
        let mut draft = self.speech_providers.get(index).cloned()
            .ok_or_else(|| "没有选中的语音渠道".to_string())?;
        let options = self.speech_inputs.options(self.speech_api, cx)?;
        let (stored, keys) = self.draft_keys(cx);
        draft.base_url = self.draft_url(cx);
        draft.api = self.speech_api;
        draft.keys = keys;
        draft.key_strategy = self.strategy;
        draft.voice = Self::read_input(&self.voice, cx);
        draft.asr_models = lines_of(&self.asr_models, cx);
        draft.tts_models = lines_of(&self.tts_models, cx);
        draft.stability = options.stability;
        draft.similarity_boost = options.similarity_boost;
        draft.style = options.style;
        draft.speed = options.speed;
        draft.language = options.language;
        draft.output_format = options.output_format;
        Ok((draft, stored))
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        if self.fetching {
            return;
        }
        let Some(channel) = self.tab.channel() else { return };
        let index = self.selected_channels[channel];
        let (mut stored, keys) = self.draft_keys(cx);
        let url = self.draft_url(cx);
        let strategy = self.strategy;
        let worker = match self.tab {
            Tab::Chat => {
                let Some(mut draft) = self.providers.get(index).cloned() else { return };
                draft.base_url = url;
                draft.api = self.api;
                draft.models = lines_of(&self.chat_models, cx);
                draft.key_strategy = strategy;
                draft.keys = keys;
                cx.background_executor().spawn(async move {
                    models::upstream_with_secret(&draft, stored.as_deref())
                })
            }
            Tab::Images => {
                let Some(mut draft) = self.image_providers.get(index).cloned() else { return };
                if self.vertex && !draft.vertex && stored.is_none() {
                    stored = Some(String::new());
                }
                draft.base_url = url;
                draft.api = self.image_api;
                draft.models = lines_of(&self.image_models, cx);
                draft.vertex = self.vertex;
                draft.key_strategy = strategy;
                draft.keys = keys;
                cx.background_executor().spawn(async move {
                    models::upstream_image(&draft, stored.as_deref())
                })
            }
            _ => return,
        };
        self.fetching = true;
        self.fetched = None;
        self.official_vendor = None;
        let generation = self.form_generation;
        cx.spawn(async move |this, cx| {
            let answer = worker.await;
            let _ = this.update(cx, |this, cx| {
                if this.form_generation != generation {
                    return;
                }
                this.fetching = false;
                if let Ok(upstream) = &answer {
                    let current = this.current_models(cx);
                    let (added, _) = upstream.diff(&current);
                    this.upstream_filter = if added.is_empty() { UpstreamFilter::All } else { UpstreamFilter::Added };
                }
                this.fetched = Some(answer);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn toggle_model(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let mut current = self.current_models(cx);
        if let Some(pos) = current.iter().position(|existing| existing == &name) {
            current.remove(pos);
        } else {
            current.push(name);
        }
        self.set_current_models(&current, window, cx);
    }

    fn add_new_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(upstream)) = &self.fetched else {
            return;
        };
        let mut current = self.current_models(cx);
        let (added, _) = upstream.diff(&current);
        let picked: Vec<String> = match self.tab {
            Tab::Images => added
                .iter()
                .filter(|name| models::is_picture(name, &self.catalog))
                .cloned()
                .collect(),
            _ => added
                .iter()
                .filter(|name| !models::is_non_chat(name, &self.catalog))
                .cloned()
                .collect(),
        };
        if picked.is_empty() {
            let hint = match self.tab {
                Tab::Images => "未自动识别出新的生图模型，可在下方列表手动点选",
                _ => "新增模型均非对话类型，可在下方列表手动点选",
            };
            self.status = Some((false, hint.into()));
            cx.notify();
            return;
        }
        let count = picked.len();
        current.extend(picked);
        self.set_current_models(&current, window, cx);
        self.status = Some((true, format!("已加入 {count} 个新模型，点底部「保存」生效")));
        cx.notify();
    }

    fn align_upstream_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(upstream)) = &self.fetched else {
            return;
        };
        let current = self.current_models(cx);
        let mut next: Vec<String> = current
            .iter()
            .filter(|name| upstream.names.iter().any(|offered| offered == *name))
            .cloned()
            .collect();
        for name in &upstream.names {
            if next.iter().any(|known| known == name) {
                continue;
            }
            let fits = match self.tab {
                Tab::Images => models::is_picture(name, &self.catalog),
                _ => !models::is_non_chat(name, &self.catalog),
            };
            if fits {
                next.push(name.clone());
            }
        }
        self.set_current_models(&next, window, cx);
        self.status = Some((true, format!("已按上游更新为 {} 个模型，点底部「保存」生效", next.len())));
        cx.notify();
    }

    fn copy_model_id(&mut self, name: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
        self.copied_model = Some(name.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1200)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_model.as_deref() == Some(name.as_str()) {
                    this.copied_model = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let enable = self.autostart_status != AutostartStatus::Enabled;
        match autostart::set(enable) {
            Ok(()) => {
                self.autostart_status = autostart::status();
                let msg = if enable { "已开启开机自启" } else { "已关闭开机自启" };
                self.status = Some((true, msg.into()));
            }
            Err(reason) => {
                self.autostart_status = autostart::status();
                self.status = Some((false, reason));
            }
        }
        cx.notify();
    }

    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.catalog_refreshing {
            return;
        }
        self.catalog_refreshing = true;
        self.catalog_notice = None;
        let dir = self.dir.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { models::refresh(&dir) }).await;
            let _ = this.update(cx, |this, cx| {
                this.catalog_refreshing = false;
                match outcome {
                    Ok(count) => {
                        this.catalog = std::sync::Arc::new(models::load(&this.dir));
                        this.catalog_updated = models::updated_at(&this.dir);
                        this.picker = None;
                        this.catalog_notice = Some((true, format!("目录已更新，共 {count} 个模型")));
                    }
                    Err(reason) => {
                        let brief: String =
                            reason.lines().next().unwrap_or("").chars().take(120).collect();
                        this.catalog_notice = Some((false, brief));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn switch_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        self.status = None;
        self.confirm = None;
        self.close_picker(cx);
        self.fill_provider(window, cx);
        if tab == Tab::General {
            self.autostart_status = autostart::status();
            self.catalog_updated = models::updated_at(&self.dir);
        }
        if tab == Tab::History {
            self.history.update(cx, |history, cx| history.refresh(cx));
        }
        if tab == Tab::Tools {
            self.tools_page.update(cx, |tools, cx| tools.refresh(window, cx));
        }
        if tab == Tab::Assistants {
            self.load_tool_names();
        }
        self.save_ui_state();
        cx.notify();
    }

    /// The servers and skills an assistant can be given.
    fn load_tool_names(&mut self) {
        let servers = myuri_core::mcp::load_servers(&self.dir).unwrap_or_default();
        self.server_names = servers.into_iter().filter(|server| server.enabled).map(|server| server.name).collect();
        self.skill_names = myuri_core::mcp::load_skills(&self.dir).into_iter().map(|skill| skill.name).collect();
    }

    /// Go to a page by its name, as other windows ask for it.
    pub fn show_page(&mut self, page: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = Tab::named(page) {
            self.switch_tab(tab, window, cx);
        }
    }

    pub fn apply_theme(&mut self, choice: ThemeChoice, window: &Window, cx: &mut Context<Self>) {
        self.theme = choice;
        let dark = theme::is_dark(choice, window.appearance());
        theme::apply_to_components(dark, cx);
        let palette = theme::palette(dark);
        self.palette = palette;
        self.history.update(cx, |view, cx| view.set_palette(palette, cx));
        self.sync.update(cx, |view, cx| view.set_palette(palette, cx));
        self.tools_page.update(cx, |view, cx| view.set_palette(palette, cx));
        if let Some(picker) = &self.picker {
            picker.update(cx, |picker, cx| {
                picker.set_palette(palette);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let entries = [
            (Tab::Assistants, "icons/bot.svg", "助手"),
            (Tab::Chat, "icons/message-circle.svg", "对话"),
            (Tab::Images, "icons/image.svg", "生图"),
            (Tab::Speech, "icons/audio-lines.svg", "语音"),
            (Tab::Tools, "icons/wrench.svg", "工具"),
            (Tab::General, "icons/sliders-horizontal.svg", "通用"),
            (Tab::History, "icons/clock.svg", "历史"),
            (Tab::Sync, "icons/cloud-upload.svg", "同步"),
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
                    .on_click(cx.listener(move |this, _, window, cx| this.switch_tab(tab, window, cx)))
            }))
    }

    fn list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = self.palette;
        let search = self.tab.channel().map_or(&self.search, |index| &self.channel_search[index]);
        let query = search.read(cx).value().trim().to_lowercase();
        let found = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let rows: Vec<AnyElement> = match self.tab {
            Tab::General | Tab::History | Tab::Sync | Tab::Tools => return None,
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
                let tab = self.tab;
                let channel = tab.channel().expect("provider page");
                let mut rows = Vec::new();
                let mut group: Option<&str> = None;
                let providers = self.channel_rows();
                let listed = providers
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| found(p.name) || found(p.id));
                for (index, provider) in listed {
                    // While searching, matches are listed flat.
                    let folded = query.is_empty() && self.folded[channel].contains(provider.group);
                    if query.is_empty() && group != Some(provider.group) && !provider.group.is_empty() {
                        let name = provider.group.to_string();
                        let members = providers.iter().filter(|p| p.group == name).count();
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
                                .on_drag(DragGroup(name.clone(), tab), move |dragged, _, _, cx| chip(dragged.0.clone().into(), palette, cx))
                                .drag_over::<DragGroup>(move |style, _, _, _| style.bg(palette.accent_wash))
                                .drag_over::<DragProvider>(move |style, _, _, _| style.bg(palette.accent_wash))
                                .on_drop(cx.listener(move |this, dragged: &DragGroup, window, cx| {
                                    if this.tab == dragged.1 {
                                        this.drop_group(&dragged.0, &onto, window, cx);
                                    }
                                }))
                                .on_drop(cx.listener(move |this, dragged: &DragProvider, window, cx| {
                                    if this.tab == dragged.2 {
                                        this.drop_into_group(dragged.0, &into, window, cx);
                                    }
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
                                    if !this.folded[channel].remove(&name) {
                                        this.folded[channel].insert(name.clone());
                                    }
                                    this.save_ui_state();
                                    cx.notify();
                                }))
                                .into_any_element(),
                        );
                    }
                    group = Some(provider.group);
                    if folded && !provider.group.is_empty() {
                        continue;
                    }
                    let name = provider.name;
                    rows.push(
                        row(("provider", index), palette, index == self.selected_channels[channel])
                            .when(!provider.enabled, |row| row.opacity(0.45))
                            .on_drag(DragProvider(index, name.to_string().into(), tab), move |dragged, _, _, cx| {
                                chip(dragged.1.clone(), palette, cx)
                            })
                            .drag_over::<DragProvider>(move |style, _, _, _| style.bg(palette.accent_wash))
                            .on_drop(cx.listener(move |this, dragged: &DragProvider, window, cx| {
                                if this.tab == dragged.2 {
                                    this.drop_provider(dragged.0, index, window, cx);
                                }
                            }))
                            .child(crate::brand::avatar(
                                crate::brand::provider_details(provider.id, provider.name, provider.base_url, provider.icon, palette.ink),
                                name,
                                16.0,
                                palette.hairline,
                                palette.muted,
                            ))
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(name.to_string()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.5))
                                    .text_color(palette.faint)
                                    .child(provider.model_count.to_string()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected_channels[channel] = index;
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
                    .child(div().flex_1().min_w(px(0.0)).text_size(px(12.5)).child(Input::new(search).appearance(false))),
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

    fn resolve_model_field(&self, default: bool, written: &str) -> Option<ModelRef> {
        if default {
            return ModelRef::parse(written);
        }
        let assistant = self.assistants.get(self.assistant)?;
        self.settings.resolve_assistant_model(
            &assistant.name,
            (!written.is_empty()).then_some(written),
        ).ok()
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
                Vec::new(),
                Vec::new(),
                config::load_favorites(&self.dir),
                Vec::new(),
                self.catalog.clone(),
                Purpose::Chat,
                self.palette,
                window,
                cx,
            ).chat_only()
        });
        self.picker_subscription = Some(cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, window, cx| {
            match event {
                PickerEvent::Chosen(Purpose::Chat, reference) => {
                    let model = format!("{}/{}", reference.provider, reference.model);
                    let target = if this.picker_default { &this.default_model } else { &this.model };
                    target.update(cx, |input, cx| input.set_value(model, window, cx));
                    this.picker_open = false;
                }
                PickerEvent::Chosen(..) | PickerEvent::Dismissed => this.picker_open = false,
                PickerEvent::SetDefault { .. } | PickerEvent::ToggleFavorite(..) => {}
            }
            cx.notify();
        }));
        let target = if default { &self.default_model } else { &self.model };
        let written = target.read(cx).value().trim().to_string();
        let current = self.resolve_model_field(default, &written);
        picker.update(cx, |picker, cx| picker.open(current.as_ref().map(|reference| (Purpose::Chat, reference)), window, cx));
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
        let follows_default = !default && (written.is_empty() || written == "default");
        let resolved = self.resolve_model_field(default, &written);
        let display = match &resolved {
            Some(reference) => {
                let channel = self.providers.iter().find(|p| p.id == reference.provider)
                    .map_or(reference.provider.as_str(), |p| p.name.as_str());
                format!("{channel} · {}", reference.model)
            }
            None if follows_default => "默认模型未设置".to_string(),
            None if written.is_empty() => "还没选".to_string(),
            None => format!("档位「{written}」未配置"),
        };
        let source = if resolved.is_some() && follows_default {
            Some("跟随默认".to_string())
        } else if resolved.is_some() && !default && self.settings.models.contains_key(&written) {
            Some(format!("档位 {written}"))
        } else {
            None
        };
        let model_button = div()
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
                    .text_color(if resolved.is_none() { palette.muted } else { palette.ink })
                    .child(display),
            )
            .children(source.map(|label| {
                div().flex_none().text_size(px(11.5)).text_color(palette.muted).child(label)
            }))
            .child(svg().path("icons/chevron-down.svg").size(px(14.0)).flex_none().text_color(palette.faint))
            .on_click(cx.listener(move |this, _, window, cx| this.open_model_picker(default, window, cx)));
        let clear = (!written.is_empty() && !follows_default).then(|| {
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
        let configure_default = (follows_default && resolved.is_none()).then(|| {
            button("configure-default-model", "设置默认模型", false, palette)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.switch_tab(Tab::General, window, cx);
                    this.open_model_picker(true, window, cx);
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
            .child(div().flex().items_center().gap(px(8.0)).child(model_button).children(clear).children(configure_default))
            .children(overlay)
            .into_any_element()
    }

    fn assistant_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let Some(assistant) = self.assistants.get(self.assistant) else {
            return vec![div().text_color(palette.faint).child("还没有助手").into_any_element()];
        };
        // The levels the bound model accepts; the common ones when unknown.
        let written = self.model.read(cx).value();
        let model = self.resolve_model_field(false, written.trim());
        let levels = match model.as_ref().and_then(|reference| self.catalog.lookup(&reference.model)) {
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
            field("工具", self.picks(false, cx), palette),
            field("技能", self.picks(true, cx), palette),
            field("提示词", boxed(Textarea::new(&self.prompt).appearance(false), palette), palette),
            self.footer("save-assistant", cx.listener(|this, _, window, cx| this.save_assistant(window, cx))),
        ]
    }

    /// The MCP servers, or the skills, as chips that switch on and off.
    fn picks(&self, skills: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let (names, chosen) = if skills { (&self.skill_names, &self.skills) } else { (&self.server_names, &self.tools) };
        // What is written but no longer exists still shows, so it can be removed.
        let gone = chosen.iter().filter(|name| !names.contains(name) && *name != "*");
        let all: Vec<String> = names.iter().chain(gone).cloned().collect();
        if all.is_empty() {
            let hint = if skills { "技能文件夹里还没有技能，在「工具」页里打开它" } else { "还没有 MCP 服务，在「工具」页里添加" };
            return div().text_size(px(12.5)).text_color(palette.faint).child(hint).into_any_element();
        }
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(all.into_iter().enumerate().map(|(index, name)| {
                let on = chosen.contains(&name) || chosen.iter().any(|entry| entry == "*");
                let label = name.clone();
                div()
                    .id((if skills { "skill-pick" } else { "tool-pick" }, index))
                    .h(px(30.0))
                    .px(px(12.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .border_1()
                    .text_size(px(12.5))
                    .when(on, |chip| chip.bg(palette.accent_wash).border_color(palette.accent).text_color(palette.accent))
                    .when(!on, |chip| chip.border_color(palette.hairline))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let every: Vec<String> = if skills { this.skill_names.clone() } else { this.server_names.clone() };
                        let list = if skills { &mut this.skills } else { &mut this.tools };
                        // "*" becomes the full list the moment one is taken out.
                        if list.iter().any(|entry| entry == "*") {
                            *list = every;
                        }
                        match list.iter().position(|entry| entry == &name) {
                            Some(position) => {
                                list.remove(position);
                            }
                            None => list.push(name.clone()),
                        }
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }

    fn provider_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let Some(provider) = self.channel_row() else {
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
        let models_label = if self.tab == Tab::Images { "生图模型" } else { "对话模型" };
        let show_models = matches!(self.tab, Tab::Chat | Tab::Images);
        vec![
            heading(provider.id.to_string(), palette)
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
            field("图标", boxed(Input::new(&self.provider_icon).appearance(false), palette), palette),
            field("密钥", self.keys_field(cx), palette),
            field("多把密钥怎么用", div().flex().flex_wrap().gap(px(6.0)).children(strategy), palette),
        ])
        .chain(show_models.then(|| field(models_label, self.models_field(cx), palette)))
        .chain([self.footer("save-provider", cx.listener(|this, _, window, cx| this.save_provider(window, cx)))])
        .collect()
    }

    /// Editable model list paired with live upstream fetch, search, pick and copy.
    fn models_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let is_images = self.tab == Tab::Images;
        let textarea = if is_images { &self.image_models } else { &self.chat_models };
        let current = self.current_models(cx);
        let diff = match &self.fetched {
            Some(Ok(upstream)) => Some(upstream.diff(&current)),
            _ => None,
        };
        let (summary, summary_error) = match (&self.fetched, self.fetching, &diff) {
            (_, true, _) => ("正在向渠道获取最新模型列表…".to_string(), false),
            (None, false, _) => (format!("当前 {} 个模型", current.len()), false),
            (Some(Err(reason)), _, _) => (
                reason.lines().next().unwrap_or("").chars().take(120).collect(),
                true,
            ),
            (Some(Ok(upstream)), _, Some((added, gone))) => (
                format!("上游 {} 个，新增 {} 个，下架 {} 个", upstream.names.len(), added.len(), gone.len()),
                false,
            ),
            (Some(Ok(upstream)), _, None) => (format!("上游 {} 个", upstream.names.len()), false),
        };
        let has_added = diff.as_ref().is_some_and(|(added, _)| !added.is_empty());
        let has_changes = diff
            .as_ref()
            .is_some_and(|(added, gone)| !added.is_empty() || !gone.is_empty());
        let has_upstream = matches!(&self.fetched, Some(Ok(upstream)) if !upstream.names.is_empty());

        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .child(
                button("fetch", if self.fetching { "拉取中…" } else { "拉取最新模型" }, false, palette)
                    .on_click(cx.listener(|this, _, _, cx| this.fetch(cx))),
            )
            .child(
                button("official-models", "从 models.dev 同步", self.official_vendor.is_some(), palette)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_official(cx))),
            )
            .when(self.official_vendor.is_none() && has_added && has_upstream, |row| {
                row.child(
                    button("add-new-models", "加入新增", true, palette)
                        .on_click(cx.listener(|this, _, window, cx| this.add_new_models(window, cx))),
                )
            })
            .when(self.official_vendor.is_none() && has_changes && has_upstream, |row| {
                row.child(
                    button("align-models", "与上游对齐", false, palette)
                        .on_click(cx.listener(|this, _, window, cx| this.align_upstream_models(window, cx))),
                )
            })
            .when(self.official_vendor.is_none(), |row| {
                row.child(
                    div()
                        .flex_1()
                        .min_w(px(120.0))
                        .text_size(px(12.0))
                        .text_color(if summary_error { palette.accent } else { palette.muted })
                        .child(summary),
                )
            });

        let preview = match (&self.fetched, &diff) {
            (Some(Ok(upstream)), Some((added, gone))) if !upstream.names.is_empty() => {
                let query = self.model_filter.read(cx).value().trim().to_lowercase();
                let terms: Vec<&str> = query.split_whitespace().collect();
                let matches_query = |name: &str| {
                    let lower = name.to_lowercase();
                    terms.iter().all(|term| lower.contains(term))
                };
                let mut shown_list: Vec<String> = match self.upstream_filter {
                    UpstreamFilter::Added => {
                        added.iter().filter(|n| matches_query(n)).cloned().collect()
                    }
                    UpstreamFilter::Gone => {
                        gone.iter().filter(|n| matches_query(n)).cloned().collect()
                    }
                    UpstreamFilter::All => {
                        upstream.names.iter().filter(|n| matches_query(n)).cloned().collect()
                    }
                };
                // Prioritize models matching the current page's purpose.
                if self.upstream_filter != UpstreamFilter::Gone {
                    shown_list.sort_by_key(|name| {
                        let preferred = if is_images {
                            models::is_picture(name, &self.catalog)
                        } else {
                            !models::is_non_chat(name, &self.catalog)
                        };
                        usize::from(!preferred)
                    });
                }
                const LIMIT: usize = 80;
                let total_matched = shown_list.len();
                let overflow = total_matched.saturating_sub(LIMIT);
                let visible: Vec<String> = shown_list.into_iter().take(LIMIT).collect();

                let filter_tab = |id: usize, tab: UpstreamFilter, label: String| {
                    let active = self.upstream_filter == tab;
                    div()
                        .id(("upstream-tab", id))
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .when(active, |el| {
                            el.bg(palette.accent_wash)
                                .text_color(palette.accent)
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .when(!active, |el| {
                            el.text_color(palette.muted).hover(move |s| s.bg(palette.selected))
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.upstream_filter = tab;
                            cx.notify();
                        }))
                };

                let bar = div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(palette.hairline)
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(150.0))
                            .h(px(26.0))
                            .rounded(px(7.0))
                            .bg(palette.surface)
                            .border_1()
                            .border_color(palette.hairline)
                            .px(px(7.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .child(svg().path("icons/search.svg").size(px(12.0)).flex_none().text_color(palette.faint))
                            .child(div().flex_1().min_w(px(0.0)).text_size(px(12.0)).child(Input::new(&self.model_filter).appearance(false))),
                    )
                    .child(filter_tab(0, UpstreamFilter::Added, format!("新增 {}", added.len())))
                    .child(filter_tab(1, UpstreamFilter::All, format!("全部 {}", upstream.names.len())))
                    .child(filter_tab(2, UpstreamFilter::Gone, format!("已下架 {}", gone.len())));

                let rows: Vec<AnyElement> = visible
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| {
                        let in_list = current.iter().any(|existing| existing == &name);
                        let is_pic = models::is_picture(&name, &self.catalog);
                        let info = self.catalog.lookup(&name);
                        let vision = info.is_some_and(|i| i.vision);
                        let reasoning = info.is_some_and(|i| i.reasoning);
                        let other_kind = info.is_some_and(|i| !matches!(i.kind, Kind::Chat | Kind::Image));
                        let copied = self.copied_model.as_deref() == Some(name.as_str());
                        let (icon, icon_color) = if self.upstream_filter == UpstreamFilter::Gone {
                            ("icons/x.svg", palette.accent)
                        } else if in_list {
                            ("icons/check.svg", palette.accent)
                        } else {
                            ("icons/plus.svg", palette.faint)
                        };
                        let name_toggle = name.clone();
                        let name_copy = name.clone();
                        div()
                            .id(("upstream-model", index))
                            .h(px(28.0))
                            .flex_none()
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .cursor_pointer()
                            .when(in_list && self.upstream_filter != UpstreamFilter::Gone, |row| {
                                row.bg(palette.selected)
                            })
                            .hover(move |style| style.bg(palette.selected))
                            .child(svg().path(icon).size(px(13.0)).flex_none().text_color(icon_color))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .font(theme::mono())
                                    .text_size(px(12.0))
                                    .text_color(if other_kind { palette.muted } else { palette.ink })
                                    .child(name),
                            )
                            .when(is_pic, |row| {
                                row.child(svg().path("icons/image.svg").size(px(12.0)).flex_none().text_color(palette.faint))
                            })
                            .when(vision && !is_pic, |row| {
                                row.child(svg().path("icons/eye.svg").size(px(12.0)).flex_none().text_color(palette.faint))
                            })
                            .when(reasoning, |row| {
                                row.child(svg().path("icons/brain.svg").size(px(12.0)).flex_none().text_color(palette.faint))
                            })
                            .child(
                                div()
                                    .id(("copy-model", index))
                                    .size(px(22.0))
                                    .flex_none()
                                    .rounded(px(5.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .hover(move |style| style.bg(palette.accent_wash))
                                    .child(
                                        svg()
                                            .path(if copied { "icons/check.svg" } else { "icons/copy.svg" })
                                            .size(px(12.0))
                                            .text_color(if copied { palette.accent } else { palette.faint }),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.copy_model_id(name_copy.clone(), cx);
                                    })),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_model(name_toggle.clone(), window, cx);
                            }))
                            .into_any_element()
                    })
                    .collect();

                let body = if rows.is_empty() {
                    div()
                        .px(px(10.0))
                        .py(px(12.0))
                        .text_size(px(12.0))
                        .text_color(palette.faint)
                        .child("没有匹配的模型")
                        .into_any_element()
                } else {
                    div()
                        .id("upstream-list")
                        .max_h(px(220.0))
                        .overflow_y_scroll()
                        .p(px(4.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .children(rows)
                        .when(overflow > 0, |list| {
                            list.child(
                                div()
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .text_size(px(11.5))
                                    .text_color(palette.faint)
                                    .child(format!("还有 {overflow} 个，输入关键词缩小范围")),
                            )
                        })
                        .into_any_element()
                };

                Some(
                    div()
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(palette.hairline)
                        .bg(palette.raised)
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(bar)
                        .child(body),
                )
            }
            _ => None,
        };

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(actions)
            .children(if self.official_vendor.is_some() {
                self.official_preview(cx)
            } else {
                preview.map(IntoElement::into_any_element)
            })
            .child(boxed(Textarea::new(textarea).appearance(false), palette))
            .into_any_element()
    }

    /// Paste one or more keys (separated by commas or newlines) into the list.
    fn add_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.new_key.read(cx).value();
        let secrets = myuri_core::llm::split_keys(&raw);
        if secrets.is_empty() {
            return;
        }
        for secret in secrets {
            if self.key_rows.iter().any(|row| row.secret == secret) {
                continue;
            }
            let note = cx.new(|cx| InputState::new(window, cx).placeholder("备注"));
            self.key_rows.push(KeyRow {
                secret: secret.to_string(),
                note,
                enabled: true,
            });
        }
        self.new_key.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// The stored keys as editable rows; a Vertex provider only takes a fresh
    /// service account.
    fn keys_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        if self.vertex {
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
                    if this.image_api != api {
                        this.image_api = api;
                        this.vertex = false;
                        this.image_size.clear();
                        this.image_resolution.clear();
                        this.image_quality.clear();
                        this.image_format.clear();
                        this.form_generation += 1;
                        this.fetching = false;
                        this.fetched = None;
                        this.official_vendor = None;
                        cx.notify();
                    }
                },
            ))
        });
        let model = self.current_models(cx).into_iter().next().unwrap_or_default();
        let spec = ImageSpec::for_model(self.image_api, &model);
        let mut fields = vec![field("生图接口", div().flex().flex_wrap().gap(px(6.0)).children(apis), palette)];
        if self.image_api == ImageApi::Gemini {
            fields.push(field(
                "Vertex AI",
                switch("image-vertex", self.vertex, palette).on_click(cx.listener(|this, _, _, cx| {
                    this.vertex = !this.vertex;
                    cx.notify();
                })),
                palette,
            ));
        }
        for (id, label, value, options, update) in [
            ("image-size", "默认比例或尺寸", self.image_size.as_str(), spec.sizes.to_vec(),
                (|this: &mut Self| &mut this.image_size) as fn(&mut Self) -> &mut String),
            ("image-resolution", "默认分辨率", self.image_resolution.as_str(),
                spec.resolutions.iter().map(|&s| (s, s)).collect(), |this| &mut this.image_resolution),
            ("image-quality", "默认质量", self.image_quality.as_str(),
                spec.qualities.to_vec(), |this| &mut this.image_quality),
            ("image-format", "默认格式", self.image_format.as_str(),
                spec.formats.iter().map(|&s| (s, s)).collect(), |this| &mut this.image_format),
        ] {
            if options.is_empty() {
                continue;
            }
            let has_value = options.iter().any(|(option, _)| option == &value);
            let choices = std::iter::once(("", "随模型")).chain(options).enumerate().map(|(index, (option, label))| {
                let selected = if option.is_empty() { !has_value } else { option == value };
                choice((id, index), label, selected, palette).on_click(cx.listener(move |this, _, _, cx| {
                    *update(this) = option.to_string();
                    cx.notify();
                }))
            });
            fields.push(field(label, div().flex().flex_wrap().gap(px(6.0)).children(choices), palette));
        }
        fields
    }


    /// A 语音识别/语音合成 field: value button plus the matching provider·model
    /// choices, inline below it.
    fn speech_field(&self, kind: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let current = if kind == "asr" { self.speech_asr.clone() } else { self.speech_tts.clone() };
        let open = self.speech_sheet == Some(kind);
        let mut options: Vec<(String, String)> = Vec::new();
        for provider in &self.speech_providers {
            if !provider.enabled {
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
                let provider = self.speech_providers.iter().find(|p| p.id == id)?;
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

    fn check_release(&mut self, cx: &mut Context<Self>) {
        if self.checking_release {
            return;
        }
        self.checking_release = true;
        self.release_status = None;
        let request = cx.background_executor().spawn(async { updates::check() });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                this.checking_release = false;
                this.release_status = Some(result);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn release_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let mut row = div().flex().flex_wrap().items_center().gap(px(10.0))
            .child(div().text_size(px(13.0)).child(crate::VERSION))
            .child(button(
                "check-release",
                if self.checking_release { "检查中…" } else { "检查更新" },
                false,
                palette,
            ).on_click(cx.listener(|this, _, _, cx| this.check_release(cx))));
        if let Some(result) = &self.release_status {
            let (text, color) = match result {
                Ok(ReleaseStatus::Current) => ("没有可用更新".into(), palette.muted),
                Ok(ReleaseStatus::Unpublished) => ("暂无正式发布版本".into(), palette.muted),
                Ok(ReleaseStatus::Available { version, .. }) => (format!("新版本 {version}"), palette.accent),
                Err(reason) => (format!("检查更新失败（{reason}）"), palette.accent),
            };
            row = row.child(div().text_size(px(12.0)).text_color(color).child(text));
            if let Ok(ReleaseStatus::Available { page, .. }) = result {
                let page = page.clone();
                row = row.child(button("open-release", "打开下载页", false, palette)
                    .on_click(move |_, _, _| crate::win::shell_open(std::path::Path::new(&page))));
            }
        }
        field("版本", row, palette)
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
        let autostart_on = self.autostart_status == AutostartStatus::Enabled;
        let autostart_hint: Option<String> = match &self.autostart_status {
            AutostartStatus::BlockedByWindows => {
                Some("已在 Windows 启动设置里被禁用，打开开关可重新启用".into())
            }
            AutostartStatus::OtherPath(path) => {
                Some(format!("当前指向 {path}，打开开关可切到当前程序"))
            }
            AutostartStatus::Enabled | AutostartStatus::Disabled => None,
        };
        let autostart_row = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                switch("general-autostart", autostart_on, palette)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_autostart(cx))),
            )
            .children(autostart_hint.map(|hint| {
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .text_color(palette.accent)
                    .child(hint)
            }));
        let (catalog_text, catalog_err) = match (&self.catalog_notice, self.catalog_refreshing) {
            (_, true) => ("正在从 models.dev 拉取最新模型目录…".to_string(), false),
            (Some((ok, text)), false) => (text.clone(), !ok),
            (None, false) => {
                let source = match self.catalog_updated {
                    Some(time) => format!("{}更新", format_age(time)),
                    None => "内置".to_string(),
                };
                (format!("共 {} 个模型 · {source}", self.catalog.len()), false)
            }
        };
        let catalog_row = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                button(
                    "refresh-catalog",
                    if self.catalog_refreshing { "更新中…" } else { "更新模型目录" },
                    false,
                    palette,
                )
                .on_click(cx.listener(|this, _, _, cx| this.refresh_catalog(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .text_color(if catalog_err { palette.accent } else { palette.muted })
                    .child(catalog_text),
            );
        vec![
            heading("通用".into(), palette).into_any_element(),
            field("开机自启", autostart_row, palette),
            field("默认模型", default_model, palette),
            field("模型能力目录", catalog_row, palette),
            field("右键长按多久弹出面板（毫秒）", boxed(Input::new(&self.hold).appearance(false), palette), palette),
            field("弹出面板的快捷键", boxed(Input::new(&self.hotkey).appearance(false), palette), palette),
            field("外观", div().flex().gap(px(6.0)).children(theme), palette),
            field("语音识别", self.speech_field("asr", cx), palette),
            field("语音合成", self.speech_field("tts", cx), palette),
            self.release_field(cx),
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
            Tab::History | Tab::Sync | Tab::Tools => Vec::new(),
        };
        // Pages that are views of their own fill everything right of the nav.
        let page = match self.tab {
            Tab::History => Some(div().flex_1().min_w(px(0.0)).h_full().child(self.history.clone())),
            Tab::Sync => Some(div().flex_1().min_w(px(0.0)).h_full().child(self.sync.clone())),
            Tab::Tools => Some(div().flex_1().min_w(px(0.0)).h_full().child(self.tools_page.clone())),
            _ => None,
        };
        type WindowControl<'a> = (&'a str, &'a str, &'a str, fn(&mut Window));
        let controls: [WindowControl<'_>; 3] = [
            ("settings-window-minimize", "最小化", "icons/window-minimize.svg", |window| {
                window.minimize_window();
            }),
            (
                "settings-window-maximize",
                if maximized { "还原" } else { "最大化" },
                if maximized { "icons/window-restore.svg" } else { "icons/window-maximize.svg" },
                |window| toggle_maximized(window),
            ),
            ("settings-window-close", "关闭", "icons/window-close.svg", Window::remove_window),
        ];
        let title = div()
            .h(px(44.0))
            .flex_none()
            .flex()
            .items_center()
            // Keep hit testing for the drag area away from the client buttons.
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .min_w(px(0.0))
                    .pl(px(18.0))
                    .flex()
                    .items_center()
                    .window_control_area(WindowControlArea::Drag)
                    .child(div().font_weight(FontWeight::MEDIUM).text_size(px(13.5)).child("设置")),
            )
            .children(controls.into_iter().map(move |(id, label, icon, click)| {
                let close = id == "settings-window-close";
                div()
                    .id(id)
                    .role(Role::Button)
                    .aria_label(label)
                    // Leave the top resize border outside the highlighted area.
                    .h(px(32.0))
                    .mt(px(6.0))
                    .w(px(44.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(move |style| style.bg(if close { gpui_kit::rgb(0xC42B1C) } else { palette.selected }))
                    .when(close, |button| button.group("settings-window-close"))
                    .child(svg().path(icon).size(px(14.0)).text_color(palette.muted).when(close, |icon| {
                        icon.group_hover("settings-window-close", |style| style.text_color(gpui_kit::rgb(0xFFFFFF)))
                    }))
                    .on_click(move |_, window, _| click(window))
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
                    .children(page)
                    .when(!self.tab.own_page(), |body| body.child(
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
                    )),
            )
    }
}

fn toggle_maximized(window: &Window) {
    if !window.is_maximized() {
        window.zoom_window();
        return;
    }
    // GPUI's Windows zoom only maximizes. Restore the previous placement.
    if let Ok(RawWindowHandle::Win32(handle)) = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        unsafe {
            let _ = ShowWindowAsync(HWND(handle.hwnd.get() as _), SW_RESTORE);
        }
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

pub(crate) fn heading(text: String, palette: Palette) -> Div {
    div().flex().items_center().gap(px(10.0)).child(
        div().font(theme::serif()).text_size(px(21.0)).text_color(palette.ink).child(text),
    )
}

/// A labelled part of a form.
pub(crate) fn field(label: &'static str, content: impl IntoElement, palette: Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(div().text_size(px(12.0)).text_color(palette.muted).child(label))
        .child(content)
        .into_any_element()
}

/// The frame around a text input.
pub(crate) fn boxed(input: impl IntoElement, palette: Palette) -> Div {
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

pub(crate) fn button(id: &'static str, label: &'static str, primary: bool, palette: Palette) -> Stateful<Div> {
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

fn lines_of(input: &Entity<TextareaState>, cx: &App) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.read(cx).value().lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !out.iter().any(|existing| existing == line) {
            out.push(line.to_string());
        }
    }
    out
}

fn format_age(time: std::time::SystemTime) -> String {
    let secs = time.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        "刚才".into()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}

pub(crate) fn switch(id: impl Into<ElementId>, on: bool, palette: Palette) -> Stateful<Div> {
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
