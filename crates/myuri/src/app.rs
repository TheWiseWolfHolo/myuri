//! Starts the worker threads and runs the UI loop that ties them together.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui_kit::App;

use myuri_core::config::{self, Assistant, Problem, Purpose, Settings};
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

pub fn migrate_providers(dir: &Path, replace_defaults: &[&str]) -> Result<(), String> {
    config::migrate_split_providers_with(dir, replace_defaults, |namespace, id| {
        use crate::secrets;
        if secrets::read_in(namespace, id).is_none() {
            if let Some(stored) = secrets::read_in(secrets::NS_CHAT, id) {
                secrets::write_in(namespace, id, &stored)?;
            }
        }
        Ok(())
    }).map(|_| ())
}

pub fn run() {
    let dir = config_dir();
    if let Err(error) = config::ensure_defaults(&dir) {
        info!("config: cannot write the defaults to {}: {error}", dir.display());
    }
    let mut problems = Vec::new();
    if let Err(message) = migrate_providers(&dir, &[]) {
        problems.push(Problem { file: "渠道迁移", message });
    }
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
    crate::events::install(ui.clone());
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
            Running { dir, settings, assistants, reported: BTreeMap::new(), tray, capture, hook, panel, settings_window: None };
        for problem in problems {
            running.report(problem);
        }
        running.prefetch_provider_icons();
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
    reported: BTreeMap<&'static str, String>,
    tray: TrayHandle,
    capture: CaptureHandle,
    hook: HookHandle,
    panel: Panel,
    /// The settings window, if it has been opened.
    settings_window: Option<(gpui_kit::AnyWindowHandle, gpui_kit::WeakEntity<crate::settings::SettingsView>)>,
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
                self.open_assistant(&assistant, content, anchor, None, cx);
            }
            UiMsg::Resume(id) => {
                if let Some(conversation) = myuri_core::history::load(&self.dir, &id) {
                    let name = conversation.assistant.clone();
                    let anchor = crate::win::cursor_pos();
                    self.open_assistant(&name, None, anchor, Some(conversation), cx);
                }
            }
            UiMsg::Page(page) => self.show_settings(Some(page), cx),
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
            UiMsg::Settings => self.show_settings(None, cx),
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

    /// Open the settings window, or bring it to the front, on `page` if given.
    fn show_settings(&mut self, page: Option<&'static str>, cx: &mut App) {
        if let Err(message) = migrate_providers(&self.dir, &[]) {
            self.report(Problem { file: "渠道迁移", message });
        } else {
            self.reported.remove("渠道迁移");
        }
        // An open window comes to the front; a closed one no longer answers.
        let shown = self.settings_window.as_ref().is_some_and(|(handle, view)| {
            handle
                .update(cx, |_, window, cx| {
                    if let (Some(page), Some(view)) = (page, view.upgrade()) {
                        view.update(cx, |view, cx| view.show_page(page, window, cx));
                    }
                    window.activate_window();
                })
                .is_ok()
        });
        if !shown {
            let dark = theme::is_dark(self.settings.panel.theme, cx.window_appearance());
            theme::apply_to_components(dark, cx);
            self.settings_window = crate::settings::open(self.dir.clone(), dark, page, cx);
        }
    }

    fn open_assistant(
        &mut self,
        name: &str,
        content: Option<Content>,
        anchor: Pos,
        resume: Option<myuri_core::history::Conversation>,
        cx: &mut App,
    ) {
        // A conversation whose assistant is gone continues with the first one.
        let found = self.assistants.iter().find(|a| a.name == name);
        let fallback = resume.as_ref().and_then(|_| self.assistants.first());
        let Some(assistant) = found.or(fallback).cloned() else { return };
        // Read from disk each time, so edits apply to the next window.
        let providers = config::load_providers(&self.dir);
        let image_providers = config::load_image_providers(&self.dir).unwrap_or_default();
        let speech_providers = config::load_speech_providers(&self.dir).unwrap_or_default();
        let last_model = resume.as_ref().and_then(|conversation| {
            conversation.resume_model(providers.as_deref().unwrap_or_default(), &image_providers, &speech_providers)
        });
        let route = match &providers {
            Ok(providers) => last_model.as_ref()
                .filter(|(purpose, _)| *purpose == Purpose::Chat)
                .and_then(|(_, model)| Route::to(providers, model).ok())
                .map(Ok)
                .unwrap_or_else(|| Route::find(&self.settings, providers, &assistant)),
            Err(problem) => Err(problem.to_string()),
        };
        let dark = theme::is_dark(self.settings.panel.theme, cx.window_appearance());
        theme::apply_to_components(dark, cx);
        let opening = Opening {
            system: config::load_prompt(&self.dir, &assistant),
            dark,
            anchor,
            providers: providers.unwrap_or_default(),
            image_providers,
            speech_providers,
            favorites: config::load_favorites(&self.dir),
            image_favorites: config::load_image_favorites(&self.dir),
            catalog: Arc::new(models::load(&self.dir)),
            default_chat: self.settings.models.get("default").and_then(|v| myuri_core::config::ModelRef::parse(v)),
            default_image: self.settings.image.default.as_deref().and_then(myuri_core::config::ModelRef::parse),
            speech: self.settings.speech.clone(),
            assistant,
            route,
            content,
            resume,
            resume_mode: last_model.filter(|(purpose, _)| *purpose != Purpose::Chat),
        };
        window::open(opening, cx);
    }

    fn shutdown(&self, cx: &mut App) {
        crate::mcp::stop_all();
        self.hook.quit();
        self.capture.quit();
        self.tray.quit();
        cx.quit();
    }

    fn reload(&mut self, cx: &mut App) {
        match config::load_settings(&self.dir) {
            Ok(settings) => {
                self.reported.remove(config::SETTINGS_FILE);
                if settings != self.settings {
                    applog::set_debug(settings.log.debug);
                    self.hook.apply(hook_settings(&settings));
                    self.capture.apply((&settings.capture).into());
                    let theme = settings.panel.theme;
                    let dark = theme::is_dark(theme, cx.window_appearance());
                    theme::apply_to_components(dark, cx);
                    if theme != self.settings.panel.theme {
                        if let Some((handle, view)) = &self.settings_window {
                            let _ = handle.update(cx, |_, window, cx| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |view, cx| view.apply_theme(theme, window, cx));
                                }
                            });
                        }
                    }
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
                self.reported.remove(config::ASSISTANTS_FILE);
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
        self.prefetch_provider_icons();
    }

    fn prefetch_provider_icons(&mut self) {
        let chat = config::load_providers(&self.dir);
        let images = config::load_image_providers(&self.dir);
        let speech = config::load_speech_providers(&self.dir);
        for (file, problem) in [
            (config::PROVIDERS_FILE, chat.as_ref().err()),
            (config::IMAGE_PROVIDERS_FILE, images.as_ref().err()),
            (config::SPEECH_PROVIDERS_FILE, speech.as_ref().err()),
        ] {
            if let Some(problem) = problem {
                self.report_problem(problem);
            } else {
                self.reported.remove(file);
            }
        }
        crate::brand::prefetch(chat.unwrap_or_default(), images.unwrap_or_default(), speech.unwrap_or_default());
    }

    fn report(&mut self, problem: Problem) {
        self.report_problem(&problem);
    }

    fn report_problem(&mut self, problem: &Problem) {
        if self.reported.get(problem.file) != Some(&problem.message) {
            info!("config: {problem}");
            self.tray.notify(problem.to_string());
            self.reported.insert(problem.file, problem.message.clone());
        }
    }
}
