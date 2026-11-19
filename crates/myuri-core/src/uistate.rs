//! Preferences of the interface itself rather than the user: which provider
//! groups are folded, how big the settings window was, which page it was on.
//! Kept in `ui-state.toml` so hand edits to `config.toml` stay clean. A
//! missing or unreadable file means defaults; it never blocks anything.

use std::fs;
use std::path::Path;

use serde::Deserialize;

pub const FILE: &str = "ui-state.toml";

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Provider groups folded in the settings list.
    pub folded: Vec<String>,
    /// The settings window's size in logical pixels.
    pub window: UiWindow,
    /// The settings page that was open, such as "assistants" or "chat".
    pub page: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct UiWindow {
    pub width: f32,
    pub height: f32,
}

/// The remembered state, or defaults when there is none.
pub fn load(dir: &Path) -> UiState {
    fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|text| toml::from_str(text.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

/// Write the state back. Best effort: a failed write is ignored.
pub fn save(dir: &Path, state: &UiState) {
    let mut text = String::new();
    if !state.page.is_empty() {
        text.push_str(&format!("page = {}\n", quoted(&state.page)));
    }
    if !state.folded.is_empty() {
        let folded = state.folded.iter().map(|group| quoted(group)).collect::<Vec<_>>().join(", ");
        text.push_str(&format!("folded = [{folded}]\n"));
    }
    if state.window.width > 0.0 && state.window.height > 0.0 {
        text.push_str(&format!(
            "[window]\nwidth = {}\nheight = {}\n",
            state.window.width.round() as i32,
            state.window.height.round() as i32
        ));
    }
    let _ = fs::write(dir.join(FILE), text);
}

fn quoted(text: &str) -> String {
    toml_edit::Value::from(text).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let dir = std::env::temp_dir().join(format!("myuri-uistate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = UiState {
            folded: vec!["主力".into(), "国际".into()],
            window: UiWindow { width: 1120.4, height: 720.0 },
            page: "chat".into(),
        };
        save(&dir, &state);
        let back = load(&dir);
        assert_eq!(back.folded, ["主力", "国际"]);
        assert_eq!(back.page, "chat");
        assert_eq!(back.window.width, 1120.0);
        assert_eq!(back.window.height, 720.0);
        fs::write(dir.join(FILE), "page = [broken").unwrap();
        assert_eq!(load(&dir), UiState::default());
        fs::remove_dir_all(&dir).ok();
    }
}
