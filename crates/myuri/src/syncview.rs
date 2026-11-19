//! The sync page of the settings window: export the configuration to a
//! file, import one, or keep a copy on a WebDAV server.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, Render, StatefulInteractiveElement, Styled, Window, div, px,
};

use myuri_core::backup::{self, Bundle};
use myuri_core::config;
use myuri_core::edit::{self, Field};

use crate::historyview::now;
use crate::settings::{boxed, button, field, heading, switch};
use crate::theme::Palette;
use crate::{net, secrets};

/// Where the WebDAV password is kept in the credential store. Not a valid
/// provider id, so it cannot collide with one.
const WEBDAV_SECRET: &str = "@webdav";

/// The files on disk were replaced by those of a backup.
pub struct Restored;

pub struct SyncView {
    dir: PathBuf,
    palette: Palette,
    url: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// Locks the keys inside a backup.
    lock: Entity<InputState>,
    with_keys: bool,
    with_history: bool,
    busy: bool,
    status: Option<(bool, String)>,
}

impl EventEmitter<Restored> for SyncView {}

/// What a backup is made from, gathered on the UI thread.
struct Recipe {
    dir: PathBuf,
    with_history: bool,
    /// The password for the keys, when they are to be included.
    lock: Option<String>,
}

impl Recipe {
    fn pack(&self) -> Result<Vec<u8>, String> {
        let files = backup::collect(&self.dir, self.with_history);
        let keys: BTreeMap<String, String> = match &self.lock {
            None => BTreeMap::new(),
            Some(_) => config::load_providers(&self.dir)
                .map_err(|problem| problem.to_string())?
                .iter()
                .filter_map(|provider| secrets::read(&provider.id).map(|key| (provider.id.clone(), key)))
                .collect(),
        };
        backup::pack(&files, self.lock.as_deref().map(|lock| (&keys, lock)), now())
    }
}

/// Put a backup's files and keys in place. Returns what to tell the user.
fn restore(dir: &PathBuf, bytes: &[u8], lock: &str) -> Result<String, String> {
    let bundle = Bundle::open(bytes)?;
    // A wrong password stops everything before any file changes.
    let keys = match bundle.has_keys() {
        true if lock.is_empty() => return Err("这份备份带着密钥，先在上面填它的密码".into()),
        true => bundle.keys(lock)?,
        false => BTreeMap::new(),
    };
    let files = bundle.restore(dir, now())?;
    for (provider, key) in &keys {
        secrets::write(provider, key)?;
    }
    Ok(match keys.len() {
        0 => format!("恢复了 {files} 个文件"),
        count => format!("恢复了 {files} 个文件和 {count} 个渠道的密钥"),
    })
}

fn basic(user: &str, password: &str) -> Vec<(String, String)> {
    if user.is_empty() && password.is_empty() {
        return Vec::new();
    }
    let token = crate::win::base64(format!("{user}:{password}").as_bytes());
    vec![("authorization".into(), format!("Basic {token}"))]
}

impl SyncView {
    pub fn new(dir: PathBuf, palette: Palette, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = config::load_settings(&dir).map(|settings| settings.sync).unwrap_or_default();
        let mut line = |hint: &'static str, value: &str, masked: bool| {
            let value = value.to_string();
            cx.new(|cx| {
                let mut input = InputState::new(window, cx).masked(masked).placeholder(hint);
                input.set_value(value, window, cx);
                input
            })
        };
        let stored = secrets::read(WEBDAV_SECRET).is_some();
        Self {
            url: line("https://dav.example.com/myuri", &settings.webdav_url, false),
            user: line("用户名", &settings.webdav_user, false),
            password: line(if stored { "已保存，要换就重新填" } else { "密码" }, "", true),
            lock: line("带上密钥时用它加密，恢复时要用同一个", "", true),
            with_keys: false,
            with_history: settings.history,
            busy: false,
            status: None,
            dir,
            palette,
        }
    }

    fn read(&self, input: &Entity<InputState>, cx: &Context<Self>) -> String {
        input.read(cx).value().trim().to_string()
    }

    fn recipe(&self, cx: &Context<Self>) -> Result<Recipe, String> {
        let lock = self.read(&self.lock, cx);
        if self.with_keys && lock.is_empty() {
            return Err("带上密钥的话要设一个密码".into());
        }
        Ok(Recipe { dir: self.dir.clone(), with_history: self.with_history, lock: self.with_keys.then_some(lock) })
    }

    fn report(&mut self, outcome: Result<String, String>, cx: &mut Context<Self>) {
        self.busy = false;
        self.status = Some(match outcome {
            Ok(message) => (true, message),
            Err(reason) => (false, reason),
        });
        cx.notify();
    }

    /// Where the backup lives on the server, and how to sign in. Saves the
    /// address, the user and a newly typed password on the way.
    fn remote(&mut self, cx: &mut Context<Self>) -> Result<(String, Vec<(String, String)>), String> {
        let (url, user, typed) = (self.read(&self.url, cx), self.read(&self.user, cx), self.read(&self.password, cx));
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("先填 WebDAV 地址，以 https:// 开头".into());
        }
        let source = fs::read_to_string(self.dir.join(config::SETTINGS_FILE)).map_err(|error| error.to_string())?;
        let updated = edit::set_setting(&source, "sync", "webdav_url", Field::Text(url.clone()))
            .and_then(|source| edit::set_setting(&source, "sync", "webdav_user", Field::optional(&user)))
            .and_then(|source| edit::set_setting(&source, "sync", "history", Field::Flag(self.with_history)))?;
        config::parse_settings(&updated)?;
        if updated != source {
            fs::write(self.dir.join(config::SETTINGS_FILE), updated).map_err(|error| error.to_string())?;
        }
        if !typed.is_empty() {
            secrets::write(WEBDAV_SECRET, &typed)?;
        }
        let password = if typed.is_empty() { secrets::read(WEBDAV_SECRET).unwrap_or_default() } else { typed };
        let address = format!("{}/{}", url.trim_end_matches('/'), backup::REMOTE_NAME);
        Ok((address, basic(&user, &password)))
    }

    /// Run `work` off the UI thread and show what it says.
    fn run(
        &mut self,
        restores: bool,
        work: impl FnOnce() -> Result<String, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.status = None;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(work());
        });
        cx.spawn(async move |this, cx| {
            let Ok(outcome) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                if restores && outcome.is_ok() {
                    cx.emit(Restored);
                }
                this.report(outcome, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let recipe = match self.recipe(cx) {
            Ok(recipe) => recipe,
            Err(reason) => return self.report(Err(reason), cx),
        };
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        let chosen = cx.prompt_for_new_path(&home.join("Documents"), Some(backup::REMOTE_NAME));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.run(
                    false,
                    move || {
                        let bytes = recipe.pack()?;
                        fs::write(&path, bytes).map_err(|error| error.to_string())?;
                        Ok(format!("已导出到 {}", path.display()))
                    },
                    cx,
                );
            });
        })
        .detach();
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let lock = self.read(&self.lock, cx);
        let dir = self.dir.clone();
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("导入".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                this.run(
                    true,
                    move || {
                        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
                        restore(&dir, &bytes, &lock)
                    },
                    cx,
                );
            });
        })
        .detach();
    }

    fn upload(&mut self, cx: &mut Context<Self>) {
        let prepared = self.recipe(cx).and_then(|recipe| self.remote(cx).map(|remote| (recipe, remote)));
        let (recipe, (address, headers)) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => return self.report(Err(reason), cx),
        };
        self.run(
            false,
            move || {
                let bytes = recipe.pack()?;
                net::put(&address, &headers, &bytes)?;
                Ok(format!("已上传，{} KB", bytes.len().div_ceil(1024)))
            },
            cx,
        );
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let (address, headers) = match self.remote(cx) {
            Ok(remote) => remote,
            Err(reason) => return self.report(Err(reason), cx),
        };
        let lock = self.read(&self.lock, cx);
        let dir = self.dir.clone();
        self.run(true, move || restore(&dir, &net::get(&address, &headers)?, &lock), cx);
    }
}

impl Render for SyncView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            div().flex().items_center().gap(px(10.0)).child(switch(id, on, palette)).child(label)
        };
        let contents = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                toggle("with-history", "带上对话历史", self.with_history).id("row-history").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.with_history = !this.with_history;
                        cx.notify();
                    },
                )),
            )
            .child(toggle("with-keys", "带上密钥（用下面的密码加密）", self.with_keys).id("row-keys").on_click(
                cx.listener(|this, _, _, cx| {
                    this.with_keys = !this.with_keys;
                    cx.notify();
                }),
            ));
        let busy = self.busy;
        let act = |id: &'static str, label: &'static str, primary: bool| {
            button(id, label, primary, palette).when_busy(busy)
        };
        let status = self.status.clone().map(|(ok, message)| {
            div().text_size(px(12.5)).text_color(if ok { palette.muted } else { palette.accent }).child(message)
        });
        div()
            .id("sync")
            .size_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(palette.hairline)
            .px(px(26.0))
            .pt(px(6.0))
            .pb(px(26.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(heading("备份与同步".into(), palette))
            .child(field("备份里放什么", contents, palette))
            .child(field("密钥的密码", boxed(Input::new(&self.lock).appearance(false), palette), palette))
            .child(
                field(
                    "备份文件",
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(act("export", "导出到文件", false).on_click(cx.listener(|this, _, _, cx| this.export(cx))))
                        .child(act("import", "从文件导入", false).on_click(cx.listener(|this, _, _, cx| this.import(cx)))),
                    palette,
                ),
            )
            .child(field("WebDAV 地址", boxed(Input::new(&self.url).appearance(false), palette), palette))
            .child(field("用户名", boxed(Input::new(&self.user).appearance(false), palette), palette))
            .child(field("密码", boxed(Input::new(&self.password).appearance(false), palette), palette))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(act("upload", "上传到 WebDAV", true).on_click(cx.listener(|this, _, _, cx| this.upload(cx))))
                    .child(
                        act("download", "从 WebDAV 恢复", false).on_click(cx.listener(|this, _, _, cx| this.download(cx))),
                    )
                    .children(busy.then(|| div().text_size(px(12.5)).text_color(palette.faint).child("正在处理")))
                    .children(status),
            )
    }
}

/// Dims a button and stops its clicks while work is in flight.
trait WhenBusy {
    fn when_busy(self, busy: bool) -> Self;
}

impl WhenBusy for gpui_kit::Stateful<gpui_kit::Div> {
    fn when_busy(self, busy: bool) -> Self {
        if busy { self.opacity(0.5).occlude() } else { self }
    }
}
