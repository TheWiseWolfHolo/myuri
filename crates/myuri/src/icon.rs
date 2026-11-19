//! Assistant icons. The `icon` setting may name a line icon from the bundled
//! Lucide set ("languages"), point at an image in the configuration folder
//! ("icons/a.png"), name a brand icon from Lobe Icons ("openai"), or simply
//! be a character or an emoji.

use std::path::PathBuf;

use gpui_kit::assets::AllAssets;
use gpui_kit::{
    AnyElement, AssetSource, IntoElement, ParentElement, Rgba, SharedString, Styled, div, img, px,
    svg,
};

use myuri_core::config::Assistant;

use crate::theme;

#[derive(Clone)]
pub enum Glyph {
    Picture(PathBuf),
    /// Asset path of a line icon.
    Line(SharedString),
    /// File name of a brand icon, such as "openai" or "gemini-color".
    Brand(String),
    Text(SharedString),
}

impl Glyph {
    pub fn of(assistant: &Assistant) -> Self {
        let icon = assistant.icon.trim();
        let lower = icon.to_lowercase();
        if [".png", ".jpg", ".jpeg", ".webp", ".gif"].iter().any(|ext| lower.ends_with(ext)) {
            let path = crate::config_dir().join(icon);
            if path.is_file() {
                return Self::Picture(path);
            }
        } else if let Some(path) = line_icon(icon) {
            return Self::Line(path);
        } else if let Some(name) = crate::brand::named(icon) {
            return Self::Brand(name);
        } else if !icon.is_empty() && !icon.contains(['/', '\\']) {
            return Self::Text(icon.to_string().into());
        }
        // Nothing usable: the first character of the name stands in.
        Self::Text(assistant.name.chars().next().map(String::from).unwrap_or_default().into())
    }

    /// The icon in a square of `size`, with `color` for line icons and text.
    pub fn render(&self, size: f32, color: Rgba) -> AnyElement {
        let frame =
            div().size(px(size)).flex_none().flex().items_center().justify_center().text_color(color);
        match self {
            Self::Picture(path) => frame
                .child(img(path.clone()).size(px(size * 0.92)).rounded(px(size * 0.22)))
                .into_any_element(),
            Self::Line(path) => frame
                .child(svg().path(path.clone()).size(px(size * 0.8)).text_color(color))
                .into_any_element(),
            // Until the picture is downloaded the square stays empty.
            Self::Brand(name) => frame
                .children(crate::brand::by_name(name, color).map(|image| crate::brand::badge(image, size * 0.8)))
                .into_any_element(),
            Self::Text(text) => frame
                .font(theme::serif())
                .text_size(px(size * 0.7))
                .child(text.clone())
                .into_any_element(),
        }
    }
}

/// The asset path for a Lucide icon name, if there is such an icon.
pub fn line_icon(name: &str) -> Option<SharedString> {
    let plain = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let path = format!("icons/{name}.svg");
    (plain && AllAssets.load(&path).is_ok_and(|found| found.is_some())).then(|| path.into())
}
