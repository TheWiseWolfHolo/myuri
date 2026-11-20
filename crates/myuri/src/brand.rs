//! Brand icons for models and providers, from Lobe Icons. Which icon goes
//! with which name is built in (`assets/lobehub.json`); the pictures are
//! downloaded the first time they are needed and kept in the configuration
//! folder, so they also show offline afterwards.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use gpui_kit::{AnyElement, Image, ImageFormat, IntoElement, ParentElement, Rgba, Styled, div, img, px};

use myuri_core::brand::Brands;
use myuri_core::config::{ImageProvider, Provider, SpeechProvider};

use crate::net;

static BRANDS: LazyLock<Brands> = LazyLock::new(|| {
    Brands::parse(include_str!("../assets/lobehub.json")).expect("the built-in icon rules parse")
});
/// Pictures already read, by file and ink color.
static LOADED: LazyLock<Mutex<HashMap<String, Arc<Image>>>> = LazyLock::new(Default::default);
/// Downloads started in this run; a failed one is not retried until the next.
static ASKED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
/// Which icon each model name got, so lists do not match again every frame.
static MODELS: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Default::default);

/// Where a picture comes from.
#[derive(Clone)]
enum Source {
    /// A file of the icon set, without extension.
    Lobe(String),
    Web(String),
    File(PathBuf),
}

impl Source {
    fn path(&self) -> PathBuf {
        let icons = crate::config_dir().join("icons");
        match self {
            Self::Lobe(name) => icons.join("lobehub").join(format!("{name}.svg")),
            Self::Web(url) => {
                let mut hasher = DefaultHasher::new();
                url.hash(&mut hasher);
                icons.join("web").join(format!("{:016x}", hasher.finish()))
            }
            Self::File(path) => path.clone(),
        }
    }

    fn url(&self) -> Option<String> {
        match self {
            Self::Lobe(name) => Some(format!(
                "https://unpkg.com/@lobehub/icons-static-svg@{}/icons/{name}.svg",
                BRANDS.version()
            )),
            Self::Web(url) => Some(url.clone()),
            Self::File(_) => None,
        }
    }

    /// Fetch the picture into its file. Returns whether it is there now.
    fn download(&self) -> bool {
        let path = self.path();
        if path.is_file() {
            return true;
        }
        let Some(url) = self.url() else { return false };
        let Ok(bytes) = net::get(&url, &[]) else { return false };
        if format(&bytes).is_none() {
            return false;
        }
        if let Some(folder) = path.parent() {
            let _ = fs::create_dir_all(folder);
        }
        fs::write(path, bytes).is_ok()
    }
}

fn format(bytes: &[u8]) -> Option<ImageFormat> {
    let head = &bytes[..bytes.len().min(256)];
    if head.starts_with(b"\x89PNG") {
        Some(ImageFormat::Png)
    } else if head.starts_with(b"\xff\xd8") {
        Some(ImageFormat::Jpeg)
    } else if head.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::Webp)
    } else if head.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if String::from_utf8_lossy(head).contains("<svg") {
        Some(ImageFormat::Svg)
    } else {
        None
    }
}

/// The picture, if its file is here. A missing one is downloaded in the
/// background and shows the next time it is asked for.
fn picture(source: &Source, ink: Rgba) -> Option<Arc<Image>> {
    let path = source.path();
    let ink = format!("#{:02x}{:02x}{:02x}", (ink.r * 255.0) as u8, (ink.g * 255.0) as u8, (ink.b * 255.0) as u8);
    let key = format!("{}|{ink}", path.display());
    if let Some(image) = LOADED.lock().unwrap().get(&key) {
        return Some(image.clone());
    }
    let Ok(mut bytes) = fs::read(&path) else {
        if source.url().is_some() && ASKED.lock().unwrap().insert(path) {
            let source = source.clone();
            std::thread::spawn(move || source.download());
        }
        return None;
    };
    let format = format(&bytes)?;
    if format == ImageFormat::Svg {
        // One-color icons take the text color. The files are sized in em,
        // which would come out tiny; draw them large and let layout scale.
        bytes = String::from_utf8_lossy(&bytes)
            .replace("currentColor", &ink)
            .replace("height=\"1em\"", "height=\"96\"")
            .replace("width=\"1em\"", "width=\"96\"")
            .into_bytes();
    }
    let image = Arc::new(Image::from_bytes(format, bytes));
    LOADED.lock().unwrap().insert(key, image.clone());
    Some(image)
}

fn model_source(name: &str) -> Option<Source> {
    let mut memo = MODELS.lock().unwrap();
    let file = match memo.get(name) {
        Some(file) => file.clone(),
        None => {
            let file = BRANDS.model(name).map(|brand| brand.file());
            memo.insert(name.to_string(), file.clone());
            file
        }
    };
    file.map(Source::Lobe)
}

/// A provider's `icon` may be left out (matched from its id, name and
/// address), name an icon of the set, point at a picture in the
/// configuration folder, or be a web address.
fn provider_source(id: &str, name: &str, base_url: &str, icon: &str) -> Option<Source> {
    let written = icon.trim();
    if written.is_empty() {
        return BRANDS.provider(id, name, base_url).map(|brand| Source::Lobe(brand.file()));
    }
    if written.starts_with("http://") || written.starts_with("https://") {
        return Some(Source::Web(written.to_string()));
    }
    if let Some(name) = named(written) {
        return Some(Source::Lobe(name));
    }
    let path = crate::config_dir().join(written);
    path.is_file().then_some(Source::File(path))
}

/// The file name of an icon written by hand, if the set has it.
pub fn named(written: &str) -> Option<String> {
    BRANDS.named(written)
}

/// An icon of the set by file name, tinted with `ink` when it has one color.
pub fn by_name(name: &str, ink: Rgba) -> Option<Arc<Image>> {
    picture(&Source::Lobe(name.to_string()), ink)
}

pub fn model(name: &str, ink: Rgba) -> Option<Arc<Image>> {
    picture(&model_source(name)?, ink)
}

pub fn provider_details(id: &str, name: &str, base_url: &str, icon: &str, ink: Rgba) -> Option<Arc<Image>> {
    picture(&provider_source(id, name, base_url, icon)?, ink)
}

/// The picture in a square of `size`.
pub fn badge(image: Arc<Image>, size: f32) -> AnyElement {
    img(image).size(px(size)).flex_none().into_any_element()
}

/// The picture, or the first letter of `name` on a tile when there is none,
/// so rows with and without an icon line up.
pub fn avatar(image: Option<Arc<Image>>, name: &str, size: f32, tile: Rgba, ink: Rgba) -> AnyElement {
    match image {
        Some(image) => badge(image, size),
        None => div()
            .size(px(size))
            .flex_none()
            .rounded(px(size * 0.28))
            .bg(tile)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(size * 0.6))
            .text_color(ink)
            .child(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default())
            .into_any_element(),
    }
}

/// Download every icon the providers and their models use, off the UI
/// thread, so lists show them from the first time they open.
pub fn prefetch(chat: Vec<Provider>, images: Vec<ImageProvider>, speech: Vec<SpeechProvider>) {
    std::thread::spawn(move || {
        let mut seen = HashSet::new();
        let mut get = |source: Source| {
            if seen.insert(source.path()) {
                source.download();
            }
        };
        for p in chat {
            provider_source(&p.id, &p.name, &p.base_url, &p.icon).into_iter()
                .chain(p.models.iter().filter_map(|name| model_source(name))).for_each(&mut get);
        }
        for p in images {
            provider_source(&p.id, &p.name, &p.base_url, &p.icon).into_iter()
                .chain(p.models.iter().filter_map(|name| model_source(name))).for_each(&mut get);
        }
        for p in speech {
            provider_source(&p.id, &p.name, &p.base_url, &p.icon).into_iter()
                .chain(p.asr_models.iter().chain(&p.tts_models).filter_map(|name| model_source(name)))
                .for_each(&mut get);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_rules_parse() {
        assert_eq!(BRANDS.model("gpt-6.1-terra").map(|brand| brand.file()).as_deref(), Some("openai"));
        assert_eq!(BRANDS.model("claude-opus-5-5").map(|brand| brand.file()).as_deref(), Some("claude-color"));
        assert!(BRANDS.provider("groq", "Groq", "https://api.groq.com/openai/v1").is_some());
        assert_eq!(named("gemini-color").as_deref(), Some("gemini-color"));
    }

    #[test]
    fn formats_are_told_apart() {
        assert!(format(b"<svg xmlns=\"x\"></svg>") == Some(ImageFormat::Svg));
        assert!(format(b"\x89PNG\r\n") == Some(ImageFormat::Png));
        assert!(format(b"<html>not found</html>").is_none());
    }
}
