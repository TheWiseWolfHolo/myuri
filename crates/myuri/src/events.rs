//! Messages from the worker threads to the UI thread.

use myuri_core::config::ContentKind;
use myuri_core::gesture::Pos;

use crate::capture::{Captured, Content};

pub enum UiMsg {
    /// The panel was asked for at `pos`. The matching `Captured` follows.
    Trigger { press: u64, pos: Pos },
    Captured { press: u64, capture: Captured },
    /// The right button came up while the panel for `press` was open.
    Released { press: u64 },
    /// An assistant was picked in the panel for the captured content.
    Chosen { assistant: String, content: Option<Content>, anchor: Pos },
    /// Close the panel if it is open.
    Dismiss,
    HotkeyUnavailable(String),
    /// A configuration file changed on disk.
    ConfigChanged,
    /// Show the panel with stand-in content, without reading anything
    /// (`myuri preview`).
    Preview(Option<ContentKind>),
    /// The tray icon was clicked with the pointer at this position.
    TrayMenu(Pos),
    /// Open the settings window, or bring it to the front.
    Settings,
    /// Open the settings window on one of its pages, by name.
    Page(&'static str),
    /// Continue the saved conversation with this id in a new window.
    Resume(String),
    Quit,
    Fatal(String),
}

static UI: std::sync::OnceLock<UiSender> = std::sync::OnceLock::new();

/// Let windows reach the main loop with [`post`].
pub fn install(ui: UiSender) {
    let _ = UI.set(ui);
}

/// Send a message to the main loop from anywhere in the UI.
pub fn post(message: UiMsg) {
    if let Some(ui) = UI.get() {
        ui.send(message);
    }
}

#[derive(Clone)]
pub struct UiSender(pub async_channel::Sender<UiMsg>);

impl UiSender {
    /// Never blocks: the channel is unbounded.
    pub fn send(&self, message: UiMsg) {
        let _ = self.0.try_send(message);
    }
}
