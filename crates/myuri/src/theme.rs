//! Colors and typefaces. Warm paper surfaces, ink text, and one terracotta
//! accent that marks the current choice and nothing else.

use gpui_kit::{Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, Rgba, WindowAppearance, rgb, rgba};

use myuri_core::config::ThemeChoice;

#[derive(Clone, Copy)]
pub struct Palette {
    pub surface: Rgba,
    /// Surfaces that sit on top of `surface`, such as the input box.
    pub raised: Rgba,
    pub ink: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub hairline: Rgba,
    /// Background of the row under the pointer or keyboard selection.
    pub selected: Rgba,
    pub accent: Rgba,
    /// The accent thinned out, for the moment a row is chosen.
    pub accent_wash: Rgba,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            surface: rgb(0x262624),
            raised: rgb(0x30302E),
            ink: rgb(0xF0EEE6),
            muted: rgb(0xA6A39A),
            faint: rgb(0x6F6D66),
            hairline: rgb(0x3D3C38),
            selected: rgb(0x343330),
            accent: rgb(0xD97757),
            accent_wash: rgba(0xD9775740),
        }
    } else {
        Palette {
            surface: rgb(0xFAF9F5),
            raised: rgb(0xFFFFFF),
            ink: rgb(0x1F1E1D),
            muted: rgb(0x73726C),
            faint: rgb(0xA3A19A),
            hairline: rgb(0xE3E1D7),
            selected: rgb(0xEFEDE3),
            accent: rgb(0xC96442),
            accent_wash: rgba(0xC9644233),
        }
    }
}

pub fn is_dark(choice: ThemeChoice, system: WindowAppearance) -> bool {
    match choice {
        ThemeChoice::Light => false,
        ThemeChoice::Dark => true,
        ThemeChoice::System => {
            matches!(system, WindowAppearance::Dark | WindowAppearance::VibrantDark)
        }
    }
}

/// Make the component library's own widgets (the input box, Markdown) match.
pub fn apply_to_components(dark: bool, cx: &mut gpui_kit::App) {
    use gpui_kit::component::theme::{Theme, ThemeMode};
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    let palette = palette(dark);
    let theme = Theme::global_mut(cx);
    theme.font_family = "Anthropic Sans Text".into();
    theme.mono_font_family = "Anthropic Mono Web".into();
    theme.colors.background = palette.surface.into();
    theme.colors.foreground = palette.ink.into();
    theme.colors.muted_foreground = palette.faint.into();
    theme.colors.border = palette.hairline.into();
    theme.colors.caret = palette.accent.into();
    theme.colors.link = palette.accent.into();
    theme.colors.link_hover = palette.accent.into();
    theme.colors.link_active = palette.accent.into();
    theme.colors.selection = palette.accent_wash.into();
}

fn face(family: &'static str, chinese: &'static str) -> Font {
    Font {
        family: family.into(),
        features: FontFeatures::default(),
        fallbacks: Some(FontFallbacks::from_fonts(vec![chinese.into()])),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

/// Controls and dense interface text.
pub fn sans() -> Font {
    face("Anthropic Sans Text", "Noto Sans SC")
}

/// The user's own words and anything meant to be read.
pub fn serif() -> Font {
    face("Anthropic Serif Text", "Noto Serif SC")
}

/// Typed input and key hints.
pub fn mono() -> Font {
    face("Anthropic Mono Web", "Noto Sans SC")
}
