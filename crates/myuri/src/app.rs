//! Starts the worker threads and runs the UI loop that ties them together.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui_kit::App;

use myuri_core::config::{self, Assistant, Problem, Settings};
use myuri_core::gesture::Pos;
use myuri_core::hotkey;

use crate::applog::{self, info};
use crate::capture::{self, CaptureHandle, Content};
use crate::chat::Route;
use crate::config_dir;
use crate::events::{UiMsg, UiSender};
use crate::hook::{self, HookHandle, HookSettings};
use crate::panel::Panel;
use crate::tray::{self, TrayHandle};
use crate::models;
use crate::watch;
use crate::window::{self, Opening};
use crate::theme;

pub fn run() {
    let dir = config_dir();
    if let Err(error) = config::ensure_defaults(&dir) {
        info!("config: cannot write the defaults to {}: {error}", dir.display());
    }
    let mut problems = Vec::new();
    let settings = config::load_settings(&dir).unwrap_or_else(|problem| {
        problems.push(problem);
        Settings::default()
    });
    let assistants = config::load_assistants(&dir).unwrap_or_else(|problem| {
        problems.push(problem);
        config::parse_assistants(config::DEFAULT_ASSISTANTS).unwrap_or_default()
    });
    applog::set_debug(settings.log.debug);

    let (tx, rx) = async_channel::unbounded();
    let ui = UiSender(tx);
    let paused = Arc::new(AtomicBool::new(false));
    let tray = tray::spawn(paused.clone(), ui.clone());
    let capture = capture::spawn((&settings.capture).into(), ui.clone());
    let hook = hook::spawn(hook_settings(&settings), paused, ui.clone(), capture.clone());
    watch::spawn(dir.clone(), ui.clone());
    info!(
        "started {}: hold {} ms, hotkey {:?}, {} assistants",
        env!("CARGO_PKG_VERSION"),
        settings.gesture.hold_ms,
        settings.hotkey.panel,
        assistants.iter().filter(|assistant| assistant.enabled).count()
    );

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        theme::apply_to_components(theme::is_dark(settings.panel.theme, cx.window_appearance()), cx);
        let Some(panel) = Panel::open(cx, settings.panel.theme, &assistants, ui) else {
            tray.notify("面板窗口创建不出来，Myuri 先退出了，详情见日志");
            hook.quit();
            capture.quit();
            tray.quit();
            cx.quit();
            return;
        };
        let mut running =
            Running { dir, settings, assistants, reported: [None, None], tray, capture, hook, panel, settings_window: None };
        for problem in problems {
            running.report(problem);
        }
        cx.spawn(async move |cx| {
            while let Ok(message) = rx.recv().await {
                if cx.update(|cx| running.handle(message, cx)) {
                    break;
                }
            }
        })
        .detach();
    });
}

fn hook_settings(settings: &Settings) -> HookSettings {
    HookSettings {
        enabled: settings.gesture.enabled,
        hold_ms: settings.gesture.hold_ms,
        tolerance: settings.gesture.move_tolerance,
        skip_fullscreen: settings.gesture.skip_fullscreen,
        exclude: settings.gesture.exclude.clone(),
        hotkey_text: settings.hotkey.panel.clone(),
        // Already validated when the settings were parsed.
        hotkey: hotkey::parse(&settings.hotkey.panel).ok().flatten(),
    }
}

struct Running {
    dir: PathBuf,
    settings: Settings,
    assistants: Vec<Assistant>,
    /// The last problem reported for each file, so one mistake is announced
    /// once however many times the file is saved.
    reported: [Option<String>; 2],
    tray: TrayHandle,
    capture: CaptureHandle,
    hook: HookHandle,
    panel: Panel,
    /// The settings window, if it has been opened.
    settings_window: Option<gpui_kit::AnyWindowHandle>,
}

impl Running {
    /// Returns true when the application is quitting.
    fn handle(&mut self, message: UiMsg, cx: &mut App) -> bool {
        match message {
            UiMsg::Trigger { press, pos } => {
                self.panel.update(cx, |panel, _, _| panel.trigger(press, pos));
            }
            UiMsg::Captured { press, capture } => {
                self.panel.update(cx, |panel, window, cx| panel.captured(press, capture, window, cx));
            }
            UiMsg::Released { press } => {
                self.panel.update(cx, |panel, _, cx| panel.released(press, cx));
            }
            UiMsg::Chosen { assistant, content, anchor } => {
                self.open_assistant(&assistant, content, anchor, cx);
            }
            UiMsg::Dismiss => self.panel.update(cx, |panel, _, cx| panel.dismiss(cx)),
            UiMsg::Preview(kind) => {
                self.panel.update(cx, |panel, window, cx| panel.preview(kind, window, cx));
            }
            UiMsg::HotkeyUnavailable(binding) => {
                self.tray.notify(format!("快捷键 {binding} 被别的程序占着，右键长按不受影响"));
            }
            UiMsg::TrayMenu(at) => {
                let dark = theme::is_dark(self.settings.panel.theme, cx.window_appearance());
                crate::traymenu::open(at, dark, self.tray.clone(), cx);
            }
            UiMsg::Settings => {
                // An open window comes to the front; a closed one no longer answers.
                let shown = self.settings_window.is_some_and(|window| {
                    window.update(cx, |_, window, _| window.activate_window()).is_ok()
                });
                if !shown {
                    let dark = theme::is_dark(self.settings.panel.theme, cx.window_appearance());
                    theme::apply_to_components(dark, cx);
                    self.settings_window = crate::settings::open(self.dir.clone(), dark, cx);
                }
            }
            UiMsg::ConfigChanged => self.reload(cx),
            UiMsg::Quit => {
                info!("quit");
                self.shutdown(cx);
                return true;
            }
            UiMsg::Fatal(reason) => {
                info!("fatal: {reason}");
                self.tray.notify(format!("{reason}，Myuri 先退出了"));
                self.shutdown(cx);
                return true;
            }
        }
        false
    }

    fn open_assistant(&mut self, name: &str, content: Option<Content>, anchor: Pos, cx: &mut App) {
        let Some(assistant) = self.assistants.iter().find(|a| a.name == name).cloned() else { return };
        // Read from disk each time, so edits apply to the next window.
        let providers = config::load_providers(&self.dir);
        let route = match &providers {
            Ok(providers) => Route::find(&self.settings, providers, &assistant),
            Err(problem) => Err(problem.to_string()),
        };
        let dark = theme::is_dark(self.settings.panel.theme, cx.window_appearance());
        theme::apply_to_components(dark, cx);
        let opening = Opening {
            system: config::load_prompt(&self.dir, &assistant),
            dark,
            anchor,
            providers: providers.unwrap_or_default(),
            favorites: config::load_favorites(&self.dir),
            catalog: Arc::new(models::load(&self.dir)),
            speech: self.settings.speech.clone(),
            assistant,
            route,
            content,
        };
        window::open(opening, cx);
    }

    fn shutdown(&self, cx: &mut App) {
        self.hook.quit();
        self.capture.quit();
        self.tray.quit();
        cx.quit();
    }

    fn reload(&mut self, cx: &mut App) {
        match config::load_settings(&self.dir) {
            Ok(settings) => {
                self.reported[0] = None;
                if settings != self.settings {
                    applog::set_debug(settings.log.debug);
                    self.hook.apply(hook_settings(&settings));
                    self.capture.apply((&settings.capture).into());
                    let theme = settings.panel.theme;
                    self.panel.update(cx, |panel, window, cx| {
                        panel.apply_theme(theme, window);
                        cx.notify();
                    });
                    self.settings = settings;
                    info!("config: settings reloaded");
                }
            }
            Err(problem) => self.report(problem),
        }
        match config::load_assistants(&self.dir) {
            Ok(assistants) => {
                self.reported[1] = None;
                if assistants != self.assistants {
                    self.assistants = assistants.clone();
                    self.panel.update(cx, |panel, _, cx| {
                        panel.set_assistants(assistants);
                        cx.notify();
                    });
                    info!("config: assistants reloaded");
                }
            }
            Err(problem) => self.report(problem),
        }
    }

    fn report(&mut self, problem: Problem) {
        let slot = &mut self.reported[usize::from(problem.file == config::ASSISTANTS_FILE)];
        if slot.as_deref() != Some(problem.message.as_str()) {
            info!("config: {problem}");
            self.tray.notify(format!("{problem}。改好之前先沿用原来的设置。"));
            *slot = Some(problem.message);
        }
    }
}
