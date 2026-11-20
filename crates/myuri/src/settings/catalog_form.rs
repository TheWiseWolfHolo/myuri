use super::*;

fn vendor_name(id: &'static str) -> &'static str {
    match id {
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "google" => "Google",
        "xai" => "xAI",
        "deepseek" => "DeepSeek",
        "mistral" => "Mistral",
        "moonshotai" => "Moonshot",
        "alibaba" => "阿里",
        "zhipuai" => "智谱",
        "minimax" => "MiniMax",
        "cohere" => "Cohere",
        "perplexity" => "Perplexity",
        "meta" | "llama" => "Meta",
        _ => id,
    }
}

impl SettingsView {
    fn official_models(&self, vendor: &str) -> impl Iterator<Item = &String> {
        self.catalog.vendor_models(vendor).iter().filter(move |name| match self.tab {
            Tab::Images => models::is_picture(name, &self.catalog),
            _ => !models::is_non_chat(name, &self.catalog),
        })
    }

    pub(super) fn toggle_official(&mut self, cx: &mut Context<Self>) {
        if self.official_vendor.take().is_none() {
            let id = self.provider_id.read(cx).value();
            let name = self.provider_name.read(cx).value();
            let url = self.base_url.read(cx).value();
            self.official_vendor = models::guess_vendor(id.trim(), name.trim(), url.trim(), &self.catalog)
                .filter(|vendor| self.official_models(vendor).next().is_some())
                .or_else(|| {
                    self.catalog.vendors()
                        .find(|vendor| self.official_models(vendor).next().is_some())
                        .map(str::to_string)
                });
            if self.official_vendor.is_none() {
                self.status = Some((false, "模型目录里暂无原厂模型".into()));
            }
        }
        cx.notify();
    }

    pub(super) fn official_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active_vendor = self.official_vendor.as_deref()?;
        let palette = self.palette;
        let query = self.model_filter.read(cx).value().trim().to_lowercase();
        let current = self.current_models(cx);
        let vendors = self.catalog.vendors()
            .filter(|vendor| self.official_models(vendor).next().is_some())
            .map(|vendor| {
                choice(format!("official-vendor-{vendor}"), vendor_name(vendor), vendor == active_vendor, palette)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.official_vendor = Some(vendor.to_string());
                        cx.notify();
                    }))
            }).collect::<Vec<_>>();
        let rows = self.official_models(active_vendor)
            .filter(|name| {
                let lower = name.to_lowercase();
                query.split_whitespace().all(|term| lower.contains(term))
            })
            .enumerate()
            .map(|(index, name)| self.official_row(index, name, current.contains(name), cx))
            .collect::<Vec<_>>();
        let empty = rows.is_empty();

        Some(div()
            .rounded(px(10.0))
            .border_1()
            .border_color(palette.hairline)
            .bg(palette.raised)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(div()
                .px(px(8.0))
                .py(px(6.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(4.0))
                .children(vendors))
            .child(div()
                .px(px(10.0))
                .border_b_1()
                .border_color(palette.hairline)
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(svg().path("icons/search.svg").size(px(13.0)).flex_none().text_color(palette.faint))
                .child(div().flex_1().min_w(px(0.0)).child(Input::new(&self.model_filter).appearance(false))))
            .child(div()
                .id(format!("official-models-{active_vendor}"))
                .max_h(px(240.0))
                .overflow_y_scroll()
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(rows)
                .when(empty, |list| {
                    list.child(div().p(px(8.0)).text_size(px(12.0)).text_color(palette.faint).child("没有匹配的模型"))
                }))
            .into_any_element())
    }

    fn official_row(&self, index: usize, name: &str, included: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let copied = self.copied_model.as_deref() == Some(name);
        let name_copy = name.to_string();
        let name_toggle = name.to_string();
        let mut details = Vec::new();
        if let Some(info) = self.catalog.lookup(name) {
            details.extend(info.context_label().map(|context| format!("上下文 {context}")));
            details.extend(info.vision.then(|| "视觉".to_string()));
            details.extend(info.reasoning.then(|| "思考".to_string()));
            details.extend(info.tool_call.then(|| "工具".to_string()));
            details.extend(info.pdf.then(|| "PDF".to_string()));
            let price = match (info.cost_in, info.cost_out) {
                (Some(input), Some(output)) => Some(format!("入 ${input} / 出 ${output} 每百万 token")),
                (Some(input), None) => Some(format!("入 ${input} 每百万 token")),
                (None, Some(output)) => Some(format!("出 ${output} 每百万 token")),
                (None, None) => None,
            };
            details.extend(price);
            details.extend(info.release_date.clone());
        }
        div()
            .id(("official-model", index))
            .py(px(4.0))
            .px(px(8.0))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .when(included, |row| row.bg(palette.selected))
            .hover(move |style| style.bg(palette.selected))
            .child(svg()
                .path(if included { "icons/check.svg" } else { "icons/plus.svg" })
                .size(px(13.0))
                .flex_none()
                .text_color(if included { palette.accent } else { palette.faint }))
            .child(div()
                .flex_1()
                .min_w(px(0.0))
                .child(div().truncate().font(theme::mono()).text_size(px(12.0)).child(name.to_string()))
                .when(!details.is_empty(), |row| {
                    row.child(div()
                        .text_size(px(10.5))
                        .text_color(palette.muted)
                        .flex()
                        .flex_wrap()
                        .gap(px(6.0))
                        .children(details.into_iter().map(|detail| div().child(detail))))
                }))
            .child(key_button(
                ("official-copy", index),
                if copied { "icons/check.svg" } else { "icons/copy.svg" },
                true,
                palette,
            ).on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.copy_model_id(name_copy.clone(), cx);
            })))
            .on_click(cx.listener(move |this, _, window, cx| this.toggle_model(name_toggle.clone(), window, cx)))
            .into_any_element()
    }
}
