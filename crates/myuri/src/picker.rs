//! The model sheet: providers (and favorites) on the left, the models of the
//! open one on the right. Used by the assistant window and by the assistant
//! form in settings.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext, Context, Div, ElementId, Entity,
    EventEmitter, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, ScrollHandle,
    ScrollStrategy, Stateful, StatefulInteractiveElement, Styled, Subscription, Window, div,
    px, svg, uniform_list,
};
use gpui_kit::UniformListScrollHandle;

use myuri_core::catalog::{Catalog, Kind};
use myuri_core::config::{ImageProvider, ModelRef, Provider, Purpose, SpeechProvider};

use crate::theme::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultScope {
    Global,
    Assistant,
}

#[derive(Clone, Debug, Default)]
pub struct PickerDefaults {
    pub assistant_name: String,
    /// True when the current assistant follows `[models] default` (empty or `"default"`).
    pub assistant_uses_global: bool,
    pub chat_global: Option<ModelRef>,
    pub chat_assistant: Option<ModelRef>,
    pub image: Option<ModelRef>,
    pub speech: Option<ModelRef>,
}

impl PickerDefaults {
    fn is_row_default(&self, purpose: Purpose, provider: &str, model: &str) -> bool {
        let matches = |reference: Option<&ModelRef>| {
            reference.is_some_and(|m| m.provider == provider && m.model == model)
        };
        match purpose {
            Purpose::Chat => matches(self.chat_assistant.as_ref()) || matches(self.chat_global.as_ref()),
            Purpose::Images => matches(self.image.as_ref()),
            Purpose::Speech => matches(self.speech.as_ref()),
        }
    }

    fn default_scope_for(&self, purpose: Purpose) -> DefaultScope {
        match purpose {
            Purpose::Chat if !self.assistant_uses_global => DefaultScope::Assistant,
            _ => DefaultScope::Global,
        }
    }
}

/// What the picker tells whoever opened it.
pub enum PickerEvent {
    Chosen(Purpose, ModelRef),
    SetDefault {
        purpose: Purpose,
        model: ModelRef,
        scope: DefaultScope,
    },
    ToggleFavorite(Purpose, ModelRef),
    Dismissed,
}

/// A model the picker could offer.
struct Listed {
    name: String,
    /// For searching.
    lower: String,
    vision: bool,
    reasoning: bool,
    tool_call: bool,
    context: Option<String>,
}

struct ListedProvider {
    id: String,
    name: String,
    group: String,
    base_url: String,
    icon: String,
    models: Vec<Listed>,
}

impl ListedProvider {
    fn brand(&self, palette: Palette) -> Option<Arc<gpui_kit::Image>> {
        crate::brand::provider_details(&self.id, &self.name, &self.base_url, &self.icon, palette.ink)
    }
}

struct Listing {
    providers: Vec<ListedProvider>,
    favorites: Vec<(usize, usize)>,
}

impl Listing {
    fn new(providers: Vec<ListedProvider>, favorites: &[ModelRef]) -> Self {
        let favorites = favorites
            .iter()
            .filter_map(|favorite| {
                let index = providers.iter().position(|p| p.id == favorite.provider)?;
                let model = providers[index].models.iter().position(|m| m.name == favorite.model)?;
                Some((index, model))
            })
            .collect();
        Self { providers, favorites }
    }
}

/// A column entry: the favorites, or one provider.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shelf {
    Favorites,
    /// Index into the providers.
    Provider(usize),
}

pub struct ModelPicker {
    chat: Listing,
    images: Listing,
    speech: Listing,
    purpose: Purpose,
    chat_only: bool,
    /// The model the caller is using, marked with a check.
    current: Option<(Purpose, ModelRef)>,
    defaults: PickerDefaults,
    notice: Option<String>,
    /// The shelf whose models are listed.
    shelf: Shelf,
    shelves_scroll: ScrollHandle,
    models_scroll: UniformListScrollHandle,
    /// Bring the open shelf into view on the next frame.
    reveal_shelf: Cell<bool>,
    search: Entity<InputState>,
    /// Row of the model list the keyboard is on.
    picked: usize,
    palette: Palette,
    /// Keys only reach the picker while it is shown.
    active: bool,
    handle: Option<AnyWindowHandle>,
    _subscriptions: Vec<Subscription>,
}

fn listed(name: String, catalog: &Catalog) -> Listed {
    let info = catalog.lookup(&name);
    Listed {
        lower: name.to_lowercase(),
        vision: info.is_some_and(|info| info.vision),
        reasoning: info.is_some_and(|info| info.reasoning),
        tool_call: info.is_some_and(|info| info.tool_call),
        context: info.and_then(|info| info.context_label()),
        name,
    }
}

impl ModelPicker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        providers: Vec<Provider>,
        image_providers: Vec<ImageProvider>,
        speech_providers: Vec<SpeechProvider>,
        favorites: Vec<ModelRef>,
        image_favorites: Vec<ModelRef>,
        catalog: Arc<Catalog>,
        purpose: Purpose,
        palette: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型"));
        let to_row = |name| listed(name, &catalog);
        let chat = providers.into_iter().filter(|provider| provider.enabled).map(|provider| ListedProvider {
            id: provider.id,
            name: provider.name,
            group: provider.group,
            base_url: provider.base_url,
            icon: provider.icon,
            models: provider.models.into_iter()
                .filter(|name| catalog.lookup(name).is_none_or(|info| info.kind == Kind::Chat))
                .map(to_row)
                .collect(),
        }).collect();
        let images = image_providers.into_iter().filter(|provider| provider.enabled).map(|provider| ListedProvider {
            id: provider.id,
            name: provider.name,
            group: provider.group,
            base_url: provider.base_url,
            icon: provider.icon,
            models: provider.models.into_iter().map(to_row).collect(),
        }).collect();
        let speech = speech_providers.into_iter().filter(|provider| provider.enabled).map(|provider| ListedProvider {
            id: provider.id,
            name: provider.name,
            group: provider.group,
            base_url: provider.base_url,
            icon: provider.icon,
            models: provider.tts_models.into_iter().map(to_row).collect(),
        }).collect();
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => this.search_changed(cx),
                InputEvent::PressEnter { .. } => {
                    if let Some((provider, model)) = this.listed_models(cx).into_iter().nth(this.picked) {
                        this.choose(provider, model, cx);
                    }
                }
                _ => {}
            }),
            cx.observe_keystrokes(|this, event, window, cx| {
                if !this.active
                    || event.keystroke.modifiers.modified()
                    || Some(Window::window_handle(window)) != this.handle
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.active = false;
                        cx.emit(PickerEvent::Dismissed);
                    }
                    "down" => this.move_pick(1, cx),
                    "up" => this.move_pick(-1, cx),
                    _ => {}
                }
            }),
        ];
        Self {
            chat: Listing::new(chat, &favorites),
            images: Listing::new(images, &image_favorites),
            speech: Listing::new(speech, &[]),
            purpose,
            chat_only: false,
            current: None,
            defaults: PickerDefaults::default(),
            notice: None,
            shelf: Shelf::Favorites,
            shelves_scroll: ScrollHandle::new(),
            models_scroll: UniformListScrollHandle::new(),
            reveal_shelf: Cell::new(false),
            search,
            picked: 0,
            palette,
            active: false,
            handle: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn chat_only(mut self) -> Self {
        self.chat_only = true;
        self.purpose = Purpose::Chat;
        self
    }

    pub fn with_defaults(mut self, defaults: PickerDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn set_defaults(
        &mut self,
        defaults: PickerDefaults,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.defaults = defaults;
        self.notice = notice;
        cx.notify();
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    fn listing(&self) -> &Listing {
        match self.purpose {
            Purpose::Chat => &self.chat,
            Purpose::Images => &self.images,
            Purpose::Speech => &self.speech,
        }
    }

    fn listing_mut(&mut self) -> &mut Listing {
        match self.purpose {
            Purpose::Chat => &mut self.chat,
            Purpose::Images => &mut self.images,
            Purpose::Speech => &mut self.speech,
        }
    }

    /// Show the shelf of `current`'s provider with it picked, clear and focus
    /// the search. Without a current model, keep the selected tab.
    pub fn open(&mut self, current: Option<(Purpose, &ModelRef)>, window: &mut Window, cx: &mut Context<Self>) {
        let current = current.filter(|(purpose, _)| !self.chat_only || *purpose == Purpose::Chat);
        if let Some((purpose, _)) = current {
            self.purpose = purpose;
        }
        self.current = current.map(|(purpose, model)| (purpose, model.clone()));
        self.notice = None;
        self.show_tab(window, cx);
    }

    fn show_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = true;
        self.handle = Some(Window::window_handle(window));
        let provider = self
            .current
            .as_ref()
            .filter(|(purpose, _)| *purpose == self.purpose)
            .and_then(|(_, m)| self.listing().providers.iter().position(|p| p.id == m.provider && !p.models.is_empty()));
        self.shelf = match provider {
            Some(index) => Shelf::Provider(index),
            None => self.shelves().first().copied().unwrap_or(Shelf::Favorites),
        };
        // The column has to be laid out once before it can be scrolled.
        cx.on_next_frame(window, |this, window, cx| {
            this.reveal_shelf.set(true);
            cx.notify();
            // The scroll lands one frame after it is asked for.
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        });
        self.search.update(cx, |search, cx| {
            search.set_value("", window, cx);
            search.focus(window, cx);
        });
        self.pick_current(cx);
        cx.notify();
    }

    /// The host closed the picker from outside: go quiet without an event.
    pub fn sleep(&mut self) {
        self.active = false;
    }

    /// Switch model tabs without selecting a model.
    pub fn set_purpose(&mut self, purpose: Purpose, window: &mut Window, cx: &mut Context<Self>) {
        if (self.active && purpose == self.purpose) || (self.chat_only && purpose != Purpose::Chat) {
            return;
        }
        self.purpose = purpose;
        self.show_tab(window, cx);
    }

    /// The words typed into the model search box, lowercase.
    fn terms(&self, cx: &App) -> Vec<String> {
        self.search.read(cx).value().to_lowercase().split_whitespace().map(String::from).collect()
    }

    /// What the left column offers: the favorites when there are any, then
    /// every provider with something to list, in file order.
    fn shelves(&self) -> Vec<Shelf> {
        let listing = self.listing();
        let providers = listing.providers.iter().enumerate()
            .filter(|(_, provider)| !provider.models.is_empty())
            .map(|(index, _)| Shelf::Provider(index));
        (!listing.favorites.is_empty()).then_some(Shelf::Favorites).into_iter().chain(providers).collect()
    }

    /// The models of one shelf whose names contain every search term, as
    /// provider index and model index.
    fn shelf_models(&self, shelf: Shelf, terms: &[String]) -> Vec<(usize, usize)> {
        let listing = self.listing();
        let fits = |&(provider, model): &(usize, usize)| {
            let name = &listing.providers[provider].models[model].lower;
            terms.iter().all(|term| name.contains(term.as_str()))
        };
        match shelf {
            Shelf::Favorites => listing.favorites.iter().copied().filter(fits).collect(),
            Shelf::Provider(index) => {
                (0..listing.providers[index].models.len()).map(|model| (index, model)).filter(fits).collect()
            }
        }
    }

    fn listed_models(&self, cx: &App) -> Vec<(usize, usize)> {
        self.shelf_models(self.shelf, &self.terms(cx))
    }

    /// Put the keyboard on the model in use, if the open shelf lists it.
    fn pick_current(&mut self, cx: &App) {
        let listed = self.listed_models(cx);
        let position = self
            .current
            .as_ref()
            .filter(|(purpose, _)| *purpose == self.purpose)
            .and_then(|(_, current)| {
                listed.iter().position(|&(provider, model)| {
                    let provider = &self.listing().providers[provider];
                    provider.id == current.provider && provider.models[model].name == current.model
                })
            });
        self.picked = position.unwrap_or(0);
        self.models_scroll.scroll_to_item(self.picked, ScrollStrategy::Center);
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let terms = self.terms(cx);
        if terms.is_empty() {
            self.pick_current(cx);
        } else {
            // Nothing on this shelf: go to the first one that has a match.
            if self.shelf_models(self.shelf, &terms).is_empty() {
                let has_match = |shelf: &Shelf| !self.shelf_models(*shelf, &terms).is_empty();
                if let Some(shelf) = self.shelves().into_iter().find(has_match) {
                    self.shelf = shelf;
                }
            }
            self.picked = 0;
            self.models_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn move_pick(&mut self, step: isize, cx: &mut Context<Self>) {
        let rows = self.listed_models(cx).len();
        if rows > 0 {
            self.picked = self.picked.saturating_add_signed(step).min(rows - 1);
            self.models_scroll.scroll_to_item(self.picked, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn choose(&mut self, provider: usize, model: usize, cx: &mut Context<Self>) {
        let reference = self.model_ref(provider, model);
        self.active = false;
        cx.emit(PickerEvent::Chosen(self.purpose, reference));
        cx.notify();
    }

    fn model_ref(&self, provider: usize, model: usize) -> ModelRef {
        let provider = &self.listing().providers[provider];
        ModelRef {
            provider: provider.id.clone(),
            model: provider.models[model].name.clone(),
        }
    }

    fn emit_default(&mut self, provider: usize, model: usize, scope: DefaultScope, cx: &mut Context<Self>) {
        let reference = self.model_ref(provider, model);
        self.current = Some((self.purpose, reference.clone()));
        cx.emit(PickerEvent::SetDefault {
            purpose: self.purpose,
            model: reference,
            scope,
        });
        cx.notify();
    }

    fn toggle_favorite(&mut self, provider: usize, model: usize, cx: &mut Context<Self>) {
        if !matches!(self.purpose, Purpose::Chat | Purpose::Images) {
            return;
        }
        let reference = self.model_ref(provider, model);
        let entry = (provider, model);
        let listing = self.listing_mut();
        if let Some(position) = listing.favorites.iter().position(|existing| *existing == entry) {
            listing.favorites.remove(position);
            self.notice = Some("已取消收藏".into());
        } else {
            listing.favorites.push(entry);
            self.notice = Some("已加入收藏".into());
        }
        if self.shelf == Shelf::Favorites && self.listing().favorites.is_empty() {
            self.shelf = self.shelves().first().copied().unwrap_or(Shelf::Favorites);
            self.picked = 0;
        } else {
            let len = self.listed_models(cx).len();
            if len == 0 {
                self.picked = 0;
            } else if self.picked >= len {
                self.picked = len - 1;
            }
        }
        cx.emit(PickerEvent::ToggleFavorite(self.purpose, reference));
        cx.notify();
    }
}

impl EventEmitter<PickerEvent> for ModelPicker {}

impl Render for ModelPicker {
    /// Providers on the left, the models of the open one on the right.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let terms = self.terms(cx);
        let mut column: Vec<AnyElement> = Vec::new();
        let mut group: Option<&str> = None;
        for shelf in self.shelves() {
            let count = self.shelf_models(shelf, &terms).len();
            if count == 0 && !terms.is_empty() {
                continue;
            }
            let label = match shelf {
                Shelf::Favorites => "收藏".to_string(),
                Shelf::Provider(index) => {
                    let provider = &self.listing().providers[index];
                    if group != Some(provider.group.as_str()) && !provider.group.is_empty() {
                        column.push(
                            div()
                                .flex_none()
                                .px(px(9.0))
                                .pt(px(11.0))
                                .pb(px(4.0))
                                .text_size(px(10.5))
                                .text_color(palette.faint)
                                .child(provider.group.clone())
                                .into_any_element(),
                        );
                    }
                    group = Some(provider.group.as_str());
                    provider.name.clone()
                }
            };
            let mark = match shelf {
                Shelf::Favorites => None,
                Shelf::Provider(index) => Some(crate::brand::avatar(
                    self.listing().providers[index].brand(palette),
                    &label,
                    14.0,
                    palette.hairline,
                    palette.muted,
                )),
            };
            let open = shelf == self.shelf;
            if open && self.reveal_shelf.take() {
                self.shelves_scroll.scroll_to_item(column.len());
            }
            column.push(
                sheet_row(("shelf", column.len()), palette, open)
                    .when(!open, |row| row.text_color(palette.muted))
                    .when(shelf == Shelf::Favorites, |row| {
                        row.child(svg().path("icons/star.svg").size(px(13.0)).flex_none().text_color(palette.accent))
                    })
                    .children(mark)
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(palette.faint)
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.shelf = shelf;
                        if this.terms(cx).is_empty() {
                            this.pick_current(cx);
                        } else {
                            this.picked = 0;
                            this.models_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        let listed = self.listed_models(cx);
        let count = listed.len();
        let with_provider = self.shelf == Shelf::Favorites;
        let models = if listed.is_empty() {
            div()
                .flex_1()
                .p(px(14.0))
                .text_size(px(11.5))
                .text_color(palette.faint)
                .child("没有匹配的模型")
                .into_any_element()
        } else {
            let listed_rows = listed.clone();
            let rows = cx.processor(move |this: &mut Self, range: Range<usize>, _, cx| {
                let palette = this.palette;
                let purpose = this.purpose;
                let allow_actions = !this.chat_only;
                let allow_favorite = allow_actions && matches!(purpose, Purpose::Chat | Purpose::Images);
                let default_scope = this.defaults.default_scope_for(purpose);
                let current = this
                    .current
                    .as_ref()
                    .filter(|(p, _)| *p == purpose)
                    .map(|(_, m)| (m.provider.clone(), m.model.clone()));
                range
                    .map(|row| {
                        let (provider, model) = listed_rows[row];
                        let source = &this.listing().providers[provider];
                        let entry = &source.models[model];
                        let in_use = current.as_ref().is_some_and(|(id, name)| *id == source.id && *name == entry.name);
                        let is_default = allow_actions && this.defaults.is_row_default(purpose, &source.id, &entry.name);
                        let is_fav = allow_favorite && this.listing().favorites.contains(&(provider, model));
                        let mark = |icon: &'static str| {
                            svg().path(icon).size(px(12.0)).flex_none().text_color(palette.faint)
                        };
                        // A model nobody recognizes takes its provider's icon.
                        let brand = crate::brand::model(&entry.name, palette.ink)
                            .or_else(|| source.brand(palette));
                        sheet_row(("model", row), palette, row == this.picked)
                            .child(crate::brand::avatar(brand, &source.name, 15.0, palette.hairline, palette.muted))
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(entry.name.clone()))
                            .when(is_default, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .px(px(5.0))
                                        .py(px(1.0))
                                        .rounded(px(4.0))
                                        .bg(palette.accent_wash)
                                        .text_size(px(10.0))
                                        .text_color(palette.accent)
                                        .child("默认"),
                                )
                            })
                            .when(entry.vision, |el| el.child(mark("icons/eye.svg")))
                            .when(entry.reasoning, |el| el.child(mark("icons/brain.svg")))
                            .when(entry.tool_call, |el| el.child(mark("icons/wrench.svg")))
                            .when_some(entry.context.clone(), |el, context| {
                                el.child(div().flex_none().text_size(px(10.5)).text_color(palette.faint).child(context))
                            })
                            .when(with_provider, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.0))
                                        .text_color(palette.faint)
                                        .child(source.name.clone()),
                                )
                            })
                            .when(in_use, |el| {
                                el.child(svg().path("icons/check.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                            })
                            .when(allow_favorite, |el| {
                                el.child(
                                    div()
                                        .id(("model-fav", row))
                                        .size(px(22.0))
                                        .flex_none()
                                        .rounded(px(5.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .hover(move |style| style.bg(palette.accent_wash))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.toggle_favorite(provider, model, cx);
                                        }))
                                        .child(
                                            svg()
                                                .path(if is_fav { "icons/star-fill.svg" } else { "icons/star.svg" })
                                                .size(px(12.0))
                                                .text_color(if is_fav { palette.accent } else { palette.faint }),
                                        ),
                                )
                            })
                            .when(allow_actions, |el| {
                                el.child(
                                    div()
                                        .id(("model-pin", row))
                                        .size(px(22.0))
                                        .flex_none()
                                        .rounded(px(5.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .hover(move |style| style.bg(palette.accent_wash))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.emit_default(provider, model, default_scope, cx);
                                        }))
                                        .child(
                                            svg()
                                                .path("icons/pin.svg")
                                                .size(px(12.0))
                                                .text_color(if is_default { palette.accent } else { palette.faint }),
                                        ),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose(provider, model, cx);
                            }))
                    })
                    .collect::<Vec<_>>()
            });
            uniform_list("models", count, rows)
                .track_scroll(&self.models_scroll)
                .flex_1()
                .h_full()
                .p(px(5.0))
                .into_any_element()
        };
        let footer = (!self.chat_only).then(|| {
            let picked = listed.get(self.picked).copied();
            let target = picked.map(|(provider, model)| {
                let source = &self.listing().providers[provider];
                let name = source.models[model].name.clone();
                (provider, model, source.id.clone(), name)
            });
            let action_btn = |id: &'static str, label: String, active: bool| {
                div()
                    .id(id)
                    .h(px(22.0))
                    .flex_none()
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .when(active, |btn| btn.bg(palette.accent_wash).text_color(palette.accent))
                    .when(!active, |btn| {
                        btn.text_color(palette.muted)
                            .border_1()
                            .border_color(palette.hairline)
                            .hover(move |style| style.bg(palette.selected))
                    })
                    .child(label)
            };
            let left_text = self
                .notice
                .clone()
                .or_else(|| target.as_ref().map(|(_, _, _, name)| name.clone()))
                .unwrap_or_default();
            let left_color = if self.notice.is_some() { palette.accent } else { palette.faint };
            let mut bar = div()
                .h(px(34.0))
                .flex_none()
                .px(px(10.0))
                .border_t_1()
                .border_color(palette.hairline)
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(left_color)
                        .child(left_text),
                );
            if let Some((provider, model, provider_id, model_name)) = target {
                match self.purpose {
                    Purpose::Chat => {
                        let is_global = self
                            .defaults
                            .chat_global
                            .as_ref()
                            .is_some_and(|m| m.provider == provider_id && m.model == model_name);
                        let is_assistant = !self.defaults.assistant_uses_global
                            && self
                                .defaults
                                .chat_assistant
                                .as_ref()
                                .is_some_and(|m| m.provider == provider_id && m.model == model_name);
                        if !self.defaults.assistant_name.is_empty() {
                            bar = bar.child(
                                action_btn(
                                    "picker-default-assistant",
                                    if is_assistant { "本助手默认 ✓".into() } else { "设为本助手默认".into() },
                                    is_assistant,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.emit_default(provider, model, DefaultScope::Assistant, cx);
                                })),
                            );
                        }
                        bar = bar.child(
                            action_btn(
                                "picker-default-global",
                                if is_global { "全局默认 ✓".into() } else { "设为全局默认".into() },
                                is_global,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.emit_default(provider, model, DefaultScope::Global, cx);
                            })),
                        );
                    }
                    Purpose::Images => {
                        let is_default = self
                            .defaults
                            .image
                            .as_ref()
                            .is_some_and(|m| m.provider == provider_id && m.model == model_name);
                        bar = bar.child(
                            action_btn(
                                "picker-default-image",
                                if is_default { "默认生图 ✓".into() } else { "设为默认生图".into() },
                                is_default,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.emit_default(provider, model, DefaultScope::Global, cx);
                            })),
                        );
                    }
                    Purpose::Speech => {
                        let is_default = self
                            .defaults
                            .speech
                            .as_ref()
                            .is_some_and(|m| m.provider == provider_id && m.model == model_name);
                        bar = bar.child(
                            action_btn(
                                "picker-default-speech",
                                if is_default { "默认语音 ✓".into() } else { "设为默认语音".into() },
                                is_default,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.emit_default(provider, model, DefaultScope::Global, cx);
                            })),
                        );
                    }
                }
            }
            bar
        });
        let tab = |purpose: Purpose, label: &'static str| {
            let active = self.purpose == purpose;
            div()
                .id(label)
                .px(px(10.0))
                .py(px(4.0))
                .rounded(px(7.0))
                .cursor_pointer()
                .text_size(px(12.5))
                .when(active, |el| el.bg(palette.selected).font_weight(gpui_kit::FontWeight::MEDIUM))
                .when(!active, |el| {
                    el.text_color(palette.muted).hover(move |style| style.bg(palette.accent_wash))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_purpose(purpose, window, cx)
                }))
        };
        div()
            .flex()
            .flex_col()
            .when(!self.chat_only, |sheet| {
                sheet.child(div()
                    .h(px(38.0))
                    .flex_none()
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .border_b_1()
                    .border_color(palette.hairline)
                    .child(tab(Purpose::Chat, "对话"))
                    .child(tab(Purpose::Images, "生图"))
                    .child(tab(Purpose::Speech, "语音")))
            })
            .child(
                div()
                    .h(px(40.0))
                    .flex_none()
                    .pl(px(12.0))
                    .pr(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .border_b_1()
                    .border_color(palette.hairline)
                    .child(svg().path("icons/search.svg").size(px(14.0)).flex_none().text_color(palette.faint))
                    .child(div().flex_1().min_w(px(0.0)).child(Input::new(&self.search).appearance(false))),
            )
            .child(
                div()
                    .h(px(308.0))
                    .flex()
                    .child(
                        div()
                            .id("shelves")
                            .w(px(178.0))
                            .flex_none()
                            .h_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.shelves_scroll)
                            .border_r_1()
                            .border_color(palette.hairline)
                            .p(px(5.0))
                            .flex()
                            .flex_col()
                            .children(column),
                    )
                    .child(models),
            )
            .children(footer)
    }
}

pub(crate) fn sheet_row(id: impl Into<ElementId>, palette: Palette, picked: bool) -> Stateful<Div> {
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
