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
    EventEmitter, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle,
    ScrollStrategy, Stateful, StatefulInteractiveElement, Styled, Subscription, Window, div,
    px, svg, uniform_list,
};
use gpui_kit::UniformListScrollHandle;

use myuri_core::catalog::{Catalog, Kind};
use myuri_core::config::{ModelRef, Provider, Purpose};

use crate::theme::Palette;

/// What the picker tells whoever opened it.
pub enum PickerEvent {
    Chosen(ModelRef),
    Dismissed,
}

/// A model the picker could offer.
struct Listed {
    name: String,
    /// For searching.
    lower: String,
    vision: bool,
    reasoning: bool,
}

/// A column entry: the favorites, or one provider.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shelf {
    Favorites,
    /// Index into the providers.
    Provider(usize),
}

pub struct ModelPicker {
    providers: Vec<Provider>,
    catalog: Arc<Catalog>,
    /// Which listing the 对话/生图 tabs are showing.
    purpose: Purpose,
    favorite_refs: Vec<ModelRef>,
    /// For every provider, the models it lists for the purpose.
    models: Vec<Vec<Listed>>,
    /// Favorite models as indices into `providers` and `models`.
    favorites: Vec<(usize, usize)>,
    /// The model the caller is using, marked with a check.
    current: Option<ModelRef>,
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

/// The models each provider lists for the purpose; none when the provider is
/// off or does not serve it.
fn models_for(providers: &[Provider], catalog: &Catalog, purpose: Purpose) -> Vec<Vec<Listed>> {
    let entry = |name: &String| {
        let info = catalog.lookup(name);
        Listed {
            name: name.clone(),
            lower: name.to_lowercase(),
            vision: info.is_some_and(|info| info.vision),
            reasoning: info.is_some_and(|info| info.reasoning),
        }
    };
    providers
        .iter()
        .map(|provider| {
            if !provider.enabled || !provider.serves(purpose) {
                return Vec::new();
            }
            match purpose {
                // Embedding, speech and picture models cannot hold a conversation.
                Purpose::Chat => provider
                    .models
                    .iter()
                    .filter(|name| {
                        !catalog.lookup(name).is_some_and(|info| info.kind != Kind::Chat)
                    })
                    .map(entry)
                    .collect(),
                Purpose::Images => provider.image_models.iter().map(entry).collect(),
                Purpose::Speech => provider
                    .asr_models
                    .iter()
                    .chain(provider.tts_models.iter())
                    .map(entry)
                    .collect(),
            }
        })
        .collect()
}

/// Favorites as indices into `providers` and `models`; chat only.
fn favorites_for(
    providers: &[Provider],
    models: &[Vec<Listed>],
    favorites: &[ModelRef],
    purpose: Purpose,
) -> Vec<(usize, usize)> {
    if purpose != Purpose::Chat {
        return Vec::new();
    }
    favorites
        .iter()
        .filter_map(|favorite| {
            let provider = providers.iter().position(|p| p.id == favorite.provider)?;
            Some((provider, models[provider].iter().position(|m| m.name == favorite.model)?))
        })
        .collect()
}

impl ModelPicker {
    pub fn new(
        providers: Vec<Provider>,
        favorites: Vec<ModelRef>,
        catalog: Arc<Catalog>,
        purpose: Purpose,
        palette: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型"));
        let models = models_for(&providers, &catalog, purpose);
        let picked_favorites = favorites_for(&providers, &models, &favorites, purpose);
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
            providers,
            catalog,
            purpose,
            favorite_refs: favorites,
            models,
            favorites: picked_favorites,
            current: None,
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

    /// Show the shelf of `current`'s provider with it picked, clear and focus
    /// the search. An image model opens on the 生图 tab, a chat model on 对话.
    pub fn open(&mut self, current: Option<&ModelRef>, window: &mut Window, cx: &mut Context<Self>) {
        let purpose = current
            .and_then(|m| self.providers.iter().find(|p| p.id == m.provider))
            .filter(|provider| provider.image_models.contains(&current.unwrap().model))
            .map(|_| Purpose::Images)
            .unwrap_or(Purpose::Chat);
        if purpose != self.purpose {
            self.models = models_for(&self.providers, &self.catalog, purpose);
            self.favorites = favorites_for(&self.providers, &self.models, &self.favorite_refs, purpose);
            self.purpose = purpose;
        }
        self.active = true;
        self.handle = Some(Window::window_handle(window));
        self.current = current.cloned();
        let provider = self
            .current
            .as_ref()
            .and_then(|m| self.providers.iter().position(|provider| provider.id == m.provider));
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

    /// One of the 对话/生图 tabs was clicked: list the other purpose's models.
    fn set_purpose(&mut self, purpose: Purpose, window: &mut Window, cx: &mut Context<Self>) {
        if purpose == self.purpose {
            return;
        }
        self.purpose = purpose;
        self.models = models_for(&self.providers, &self.catalog, purpose);
        self.favorites = favorites_for(&self.providers, &self.models, &self.favorite_refs, purpose);
        self.current = None;
        let current = self.current.clone();
        self.open(current.as_ref(), window, cx);
    }

    /// The words typed into the model search box, lowercase.
    fn terms(&self, cx: &App) -> Vec<String> {
        self.search.read(cx).value().to_lowercase().split_whitespace().map(String::from).collect()
    }

    /// What the left column offers: the favorites when there are any, then
    /// every provider with something to list, in file order.
    fn shelves(&self) -> Vec<Shelf> {
        let usable = |&index: &usize| !self.models[index].is_empty();
        let providers = (0..self.providers.len()).filter(usable).map(Shelf::Provider);
        (!self.favorites.is_empty()).then_some(Shelf::Favorites).into_iter().chain(providers).collect()
    }

    /// The models of one shelf whose names contain every search term, as
    /// provider index and model index.
    fn shelf_models(&self, shelf: Shelf, terms: &[String]) -> Vec<(usize, usize)> {
        let fits = |&(provider, model): &(usize, usize)| {
            let name = &self.models[provider][model].lower;
            terms.iter().all(|term| name.contains(term.as_str()))
        };
        match shelf {
            Shelf::Favorites => self.favorites.iter().copied().filter(fits).collect(),
            Shelf::Provider(index) => {
                (0..self.models[index].len()).map(|model| (index, model)).filter(fits).collect()
            }
        }
    }

    fn listed_models(&self, cx: &App) -> Vec<(usize, usize)> {
        self.shelf_models(self.shelf, &self.terms(cx))
    }

    /// Put the keyboard on the model in use, if the open shelf lists it.
    fn pick_current(&mut self, cx: &App) {
        let listed = self.listed_models(cx);
        let position = self.current.as_ref().and_then(|current| {
            listed.iter().position(|&(provider, model)| {
                self.providers[provider].id == current.provider
                    && self.models[provider][model].name == current.model
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
        let reference = ModelRef {
            provider: self.providers[provider].id.clone(),
            model: self.models[provider][model].name.clone(),
        };
        self.active = false;
        cx.emit(PickerEvent::Chosen(reference));
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
                    let provider = &self.providers[index];
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
                    crate::brand::provider(&self.providers[index], palette.ink),
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
            let rows = cx.processor(move |this: &mut Self, range: Range<usize>, _, cx| {
                let palette = this.palette;
                let current = this.current.clone().map(|m| (m.provider, m.model));
                range
                    .map(|row| {
                        let (provider, model) = listed[row];
                        let source = &this.providers[provider];
                        let entry = &this.models[provider][model];
                        let in_use = current.as_ref().is_some_and(|(id, name)| *id == source.id && *name == entry.name);
                        let mark = |icon: &'static str| {
                            svg().path(icon).size(px(12.0)).flex_none().text_color(palette.faint)
                        };
                        // A model nobody recognizes takes its provider's icon.
                        let brand = crate::brand::model(&entry.name, palette.ink)
                            .or_else(|| crate::brand::provider(source, palette.ink));
                        sheet_row(("model", row), palette, row == this.picked)
                            .child(crate::brand::avatar(brand, &source.name, 15.0, palette.hairline, palette.muted))
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(entry.name.clone()))
                            .when(entry.vision, |el| el.child(mark("icons/eye.svg")))
                            .when(entry.reasoning, |el| el.child(mark("icons/brain.svg")))
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
            .child(
                div()
                    .h(px(38.0))
                    .flex_none()
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .border_b_1()
                    .border_color(palette.hairline)
                    .child(tab(Purpose::Chat, "对话"))
                    .child(tab(Purpose::Images, "生图")),
            )
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
