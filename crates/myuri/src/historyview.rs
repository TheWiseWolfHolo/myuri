//! The history page of the settings window: saved conversations on the
//! left, the chosen one on the right, to read, continue or delete.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px, relative, svg,
};

use myuri_core::history::{self, Conversation, Summary};

use crate::events::{self, UiMsg};
use crate::theme::{self, Palette};

pub struct HistoryView {
    dir: PathBuf,
    palette: Palette,
    summaries: Vec<Summary>,
    open: Option<Conversation>,
    /// Replies of the open conversation, rendered as Markdown.
    replies: Vec<Option<Entity<TextViewState>>>,
    search: Entity<InputState>,
    /// The delete button was pressed once.
    confirm: bool,
    _subscriptions: Vec<Subscription>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or_default()
}

impl HistoryView {
    pub fn set_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        self.palette = palette;
        cx.notify();
    }

    pub fn new(dir: PathBuf, palette: Palette, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索"));
        let subscriptions = vec![cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        let mut view = Self {
            dir,
            palette,
            summaries: Vec::new(),
            open: None,
            replies: Vec::new(),
            search,
            confirm: false,
            _subscriptions: subscriptions,
        };
        view.refresh(cx);
        view
    }

    /// Read the list again, keeping the open conversation if it still exists.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.summaries = history::list(&self.dir);
        let current = self.open.as_ref().map(|conversation| conversation.id.clone());
        let next = current
            .filter(|id| self.summaries.iter().any(|summary| &summary.id == id))
            .or_else(|| self.summaries.first().map(|summary| summary.id.clone()));
        match next {
            Some(id) => self.select(&id, cx),
            None => {
                self.open = None;
                self.replies.clear();
            }
        }
        cx.notify();
    }

    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.confirm = false;
        self.open = history::load(&self.dir, id);
        self.replies = self
            .open
            .iter()
            .flat_map(|conversation| &conversation.messages)
            .map(|entry| {
                (entry.role == "assistant" && !entry.text.is_empty())
                    .then(|| cx.new(|cx| TextViewState::markdown(&entry.text, cx)))
            })
            .collect();
        cx.notify();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open.as_ref().map(|conversation| conversation.id.clone()) else { return };
        if !std::mem::replace(&mut self.confirm, true) {
            return cx.notify();
        }
        let _ = history::delete(&self.dir, &id);
        self.open = None;
        self.refresh(cx);
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let query = self.search.read(cx).value().trim().to_lowercase();
        let now = now();
        let chosen = self.open.as_ref().map(|conversation| conversation.id.as_str());
        let rows = self
            .summaries
            .iter()
            .filter(|summary| {
                query.is_empty()
                    || summary.title.to_lowercase().contains(&query)
                    || summary.assistant.to_lowercase().contains(&query)
            })
            .enumerate()
            .map(|(index, summary)| {
                let id = summary.id.clone();
                let open = chosen == Some(summary.id.as_str());
                div()
                    .id(("conversation", index))
                    .flex_none()
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .cursor_pointer()
                    .when(open, |row| row.bg(palette.selected))
                    .hover(move |style| style.bg(palette.selected))
                    .child(div().truncate().child(summary.title.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(palette.faint)
                            .child(format!("{} · {}", summary.assistant, history::ago(now, summary.updated))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select(&id, cx)))
            })
            .collect::<Vec<_>>();
        let bar = div().flex_none().p(px(6.0)).pb(px(2.0)).child(
            div()
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
        );
        div()
            .w(px(250.0))
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
                    .id("conversations")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .p(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .children(rows),
            )
    }

    fn transcript(&self, conversation: &Conversation, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(30.0))
                .flex_none()
                .px(px(13.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .when(primary, |button| button.bg(palette.accent).text_color(gpui_kit::rgb(0xFFFFFF)))
                .when(!primary, |button| button.text_color(palette.muted).border_1().border_color(palette.hairline))
                .hover(|style| style.opacity(0.88))
                .child(label)
        };
        let id = conversation.id.clone();
        let head = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pb(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().truncate().font(theme::serif()).text_size(px(19.0)).child(conversation.title()))
                    .child(div().truncate().text_size(px(11.5)).text_color(palette.faint).child(format!(
                        "{} · {} · {} 条",
                        conversation.assistant,
                        conversation.model,
                        conversation.messages.len()
                    ))),
            )
            .child(
                button("delete-conversation", if self.confirm { "再点一次删除" } else { "删除" }, false)
                    .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
            )
            .child(button("resume", "继续对话", true).on_click(move |_, _, _| {
                events::post(UiMsg::Resume(id.clone()));
            }));
        let messages = conversation.messages.iter().zip(&self.replies).map(|(entry, reply)| {
            if entry.role == "user" {
                let bubble = div()
                    .max_w(relative(0.86))
                    .px(px(13.0))
                    .py(px(9.0))
                    .rounded(px(14.0))
                    .bg(palette.selected)
                    .line_height(relative(1.55))
                    .when(!entry.pictures.is_empty(), |bubble| {
                        bubble.child(div().text_color(palette.muted).child(format!("{} 张图片", entry.pictures.len())))
                    })
                    .when(!entry.text.is_empty(), |bubble| bubble.child(entry.text.clone()));
                div().w_full().flex().justify_end().child(bubble)
            } else {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .font(theme::serif())
                    .text_size(px(14.5))
                    .line_height(relative(1.7))
                    .children(reply.as_ref().map(|view| TextView::new(view).selectable(true)))
                    .children(entry.shots.iter().map(|path| {
                        div().font(theme::sans()).text_size(px(12.0)).text_color(palette.muted).child(path.clone())
                    }))
                    .children(entry.audio.as_ref().map(|audio| {
                        let secs = audio.duration_ms / 1000;
                        let open = PathBuf::from(&audio.path);
                        let select = open.clone();
                        div()
                            .max_w(px(420.0))
                            .p(px(9.0))
                            .rounded(px(9.0))
                            .border_1()
                            .border_color(palette.hairline)
                            .bg(palette.raised)
                            .font(theme::sans())
                            .text_size(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(svg().path("icons/audio-lines.svg").size(px(14.0)).flex_none().text_color(palette.accent))
                            .child(div().flex_none().text_color(palette.muted).child(format!("{}:{:02}", secs / 60, secs % 60)))
                            .child(div().flex_1().min_w(px(0.0)).truncate().text_color(palette.muted).child(audio.summary.clone()))
                            .child(
                                div()
                                    .id(format!("hist-open-{}", audio.path))
                                    .px(px(7.0))
                                    .py(px(2.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_color(palette.muted)
                                    .hover(move |style| style.bg(palette.selected))
                                    .child("打开")
                                    .on_click(move |_, _, _| crate::win::shell_open(&open)),
                            )
                            .child(
                                div()
                                    .id(format!("hist-dir-{}", audio.path))
                                    .px(px(7.0))
                                    .py(px(2.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_color(palette.muted)
                                    .hover(move |style| style.bg(palette.selected))
                                    .child("文件夹")
                                    .on_click(move |_, _, _| crate::win::shell_select(&select)),
                            )
                    }))
            }
        });
        div()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .px(px(26.0))
            .pt(px(6.0))
            .flex()
            .flex_col()
            .child(head)
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .pb(px(26.0))
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .children(messages),
            )
    }
}

impl Render for HistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let body = match self.open.clone() {
            Some(conversation) => self.transcript(&conversation, cx).into_any_element(),
            None => div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(palette.faint)
                .child("聊过的对话会出现在这里")
                .into_any_element(),
        };
        div().size_full().flex().child(self.list(cx)).child(body)
    }
}
