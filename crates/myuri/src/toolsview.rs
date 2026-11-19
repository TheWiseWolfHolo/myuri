//! The tools page of the settings window: the MCP servers in `mcp.toml`,
//! and the skills found in the skills folder.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px, svg,
};

use myuri_core::edit::{self, Field};
use myuri_core::mcp::{self, MCP_FILE, SKILLS_DIR, Server, Skill};

use crate::settings::{boxed, button, field, heading, switch};
use crate::theme::Palette;

pub struct ToolsView {
    dir: PathBuf,
    palette: Palette,
    servers: Vec<Server>,
    skills: Vec<Skill>,
    /// Index of the open server.
    open: usize,
    name: Entity<InputState>,
    command: Entity<InputState>,
    args: Entity<TextareaState>,
    env: Entity<TextareaState>,
    url: Entity<InputState>,
    headers: Entity<TextareaState>,
    enabled: bool,
    confirm: bool,
    /// The delete button was pressed once.
    deleting: bool,
    busy: bool,
    status: Option<(bool, String)>,
}

/// `KEY=VALUE` lines as a map; lines without `=` are ignored.
fn pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

fn lines_of(map: &BTreeMap<String, String>) -> String {
    map.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("\n")
}

impl ToolsView {
    pub fn new(dir: PathBuf, palette: Palette, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("只能字母、数字、- 和 _")),
            command: cx.new(|cx| InputState::new(window, cx).placeholder("uvx、npx，或程序的完整路径")),
            args: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 8).placeholder("一行一个参数")),
            env: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 8).placeholder("一行一个，写成 名字=值")),
            url: cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com/mcp")),
            headers: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 8).placeholder("一行一个，写成 名字=值")),
            servers: Vec::new(),
            skills: Vec::new(),
            open: 0,
            enabled: true,
            confirm: true,
            deleting: false,
            busy: false,
            status: None,
            dir,
            palette,
        };
        view.refresh(window, cx);
        view
    }

    /// Read the file and the skills folder again.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match mcp::load_servers(&self.dir) {
            Ok(servers) => self.servers = servers,
            Err(problem) => self.status = Some((false, problem)),
        }
        self.skills = mcp::load_skills(&self.dir);
        self.open = self.open.min(self.servers.len().saturating_sub(1));
        self.fill(window, cx);
    }

    fn fill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.deleting = false;
        let Some(server) = self.servers.get(self.open).cloned() else { return };
        self.enabled = server.enabled;
        self.confirm = server.confirm;
        self.name.update(cx, |input, cx| input.set_value(server.name.clone(), window, cx));
        self.command.update(cx, |input, cx| input.set_value(server.command.clone(), window, cx));
        self.url.update(cx, |input, cx| input.set_value(server.url.clone(), window, cx));
        self.args.update(cx, |input, cx| input.set_value(server.args.join("\n"), window, cx));
        self.env.update(cx, |input, cx| input.set_value(lines_of(&server.env), window, cx));
        self.headers.update(cx, |input, cx| input.set_value(lines_of(&server.headers), window, cx));
        cx.notify();
    }

    fn write(&mut self, change: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
        let path = self.dir.join(MCP_FILE);
        let source = fs::read_to_string(&path).unwrap_or_else(|_| mcp::DEFAULT_MCP.to_string());
        let updated = change(&source)?;
        mcp::parse_servers(&updated)?;
        fs::write(path, updated).map_err(|error| error.to_string())
    }

    fn finish(&mut self, outcome: Result<(), String>, open: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.status = Some(match outcome {
            Ok(()) => (true, "已保存".into()),
            Err(reason) => (false, reason),
        });
        if self.status.as_ref().is_some_and(|(ok, _)| *ok) {
            self.servers = mcp::load_servers(&self.dir).unwrap_or_default();
            if let Some(index) = open.and_then(|name| self.servers.iter().position(|server| server.name == name)) {
                self.open = index;
            }
            self.open = self.open.min(self.servers.len().saturating_sub(1));
            self.fill(window, cx);
        }
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = (1..).map(|n| format!("server-{n}")).find(|name| !self.servers.iter().any(|s| &s.name == name)).unwrap_or_default();
        let entry = format!("\n[[server]]\nname = {name:?}\ncommand = \"uvx\"\nenabled = false\n");
        let outcome = self.write(|source| Ok(format!("{}\n{entry}", source.trim_end())));
        self.finish(outcome, Some(name), window, cx);
    }

    /// The form as a server, the way it would be saved.
    fn written(&self, cx: &Context<Self>) -> Server {
        let line = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let area = |input: &Entity<TextareaState>| input.read(cx).value().to_string();
        Server {
            name: line(&self.name),
            command: line(&self.command),
            args: area(&self.args).lines().map(str::trim).filter(|arg| !arg.is_empty()).map(str::to_string).collect(),
            env: pairs(&area(&self.env)).into_iter().collect(),
            url: line(&self.url),
            headers: pairs(&area(&self.headers)).into_iter().collect(),
            enabled: self.enabled,
            confirm: self.confirm,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(old) = self.servers.get(self.open).map(|server| server.name.clone()) else { return };
        let server = self.written(cx);
        let map = |map: &BTreeMap<String, String>| Field::Map(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        let flag = |on: bool| if on { Field::Absent } else { Field::Flag(false) };
        let changes = [
            ("name", Field::Text(server.name.clone())),
            ("command", Field::optional(&server.command)),
            ("args", Field::List(server.args.clone())),
            ("env", map(&server.env)),
            ("url", Field::optional(&server.url)),
            ("headers", map(&server.headers)),
            ("enabled", flag(server.enabled)),
            ("confirm", flag(server.confirm)),
        ];
        let outcome = self.write(|source| edit::set_entry(source, "server", "name", &old, &changes));
        self.finish(outcome, Some(server.name), window, cx);
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.servers.get(self.open).map(|server| server.name.clone()) else { return };
        if !std::mem::replace(&mut self.deleting, true) {
            return cx.notify();
        }
        let outcome = self.write(|source| edit::remove_entry(source, "server", "name", &name));
        self.finish(outcome, None, window, cx);
    }

    /// Start the server as written in the form and list its tools.
    fn test(&mut self, cx: &mut Context<Self>) {
        let server = self.written(cx);
        if server.command.is_empty() == server.url.is_empty() {
            self.status = Some((false, "命令和地址填一个".into()));
            return cx.notify();
        }
        self.busy = true;
        self.status = None;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(crate::mcp::probe(&server));
        });
        cx.spawn(async move |this, cx| {
            let Ok(outcome) = rx.recv().await else { return };
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(match outcome {
                    Ok(tools) if tools.is_empty() => (true, "连上了，它没有提供工具".into()),
                    Ok(tools) => (true, format!("连上了，{} 个工具：{}", tools.len(), tools.join("、"))),
                    Err(reason) => (false, reason),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = self.palette;
        let label = |text: &'static str| {
            div().flex_none().px(px(9.0)).pt(px(10.0)).pb(px(4.0)).text_size(px(10.5)).text_color(palette.faint).child(text)
        };
        let servers = self.servers.iter().enumerate().map(|(index, server)| {
            let open = index == self.open;
            div()
                .id(("server", index))
                .h(px(32.0))
                .flex_none()
                .px(px(9.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .gap(px(9.0))
                .cursor_pointer()
                .when(open, |row| row.bg(palette.selected))
                .when(!server.enabled, |row| row.opacity(0.45))
                .hover(move |style| style.bg(palette.selected))
                .child(svg().path(if server.url.is_empty() { "icons/terminal.svg" } else { "icons/globe.svg" }).size(px(14.0)).flex_none().text_color(palette.muted))
                .child(div().flex_1().min_w(px(0.0)).truncate().child(server.name.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open = index;
                    this.status = None;
                    this.fill(window, cx);
                }))
        });
        let skills = self.skills.iter().map(|skill| {
            div()
                .flex_none()
                .px(px(9.0))
                .py(px(5.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(div().truncate().child(skill.name.clone()))
                .child(div().text_size(px(11.0)).text_color(palette.faint).line_clamp(2).child(skill.description.clone()))
        });
        let folder = self.dir.join(SKILLS_DIR);
        div()
            .w(px(232.0))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_r_1()
            .border_color(palette.hairline)
            .flex()
            .flex_col()
            .child(
                div()
                    .id("tools-list")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .p(px(6.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(label("MCP 服务"))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("add-server")
                                    .size(px(24.0))
                                    .rounded(px(7.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(move |style| style.bg(palette.selected))
                                    .child(svg().path("icons/plus.svg").size(px(14.0)).text_color(palette.muted))
                                    .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                            ),
                    )
                    .children(servers)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(label("技能"))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("open-skills")
                                    .size(px(24.0))
                                    .rounded(px(7.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(move |style| style.bg(palette.selected))
                                    .child(svg().path("icons/folder-open.svg").size(px(14.0)).text_color(palette.muted))
                                    .on_click(move |_, _, _| {
                                        let _ = fs::create_dir_all(&folder);
                                        crate::win::shell_open(&folder);
                                    }),
                            ),
                    )
                    .children(skills)
                    .when(self.skills.is_empty(), |list| {
                        list.child(
                            div()
                                .px(px(9.0))
                                .text_size(px(11.5))
                                .text_color(palette.faint)
                                .child("把带 SKILL.md 的文件夹放进技能文件夹"),
                        )
                    }),
            )
    }

    fn form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let Some(server) = self.servers.get(self.open) else {
            return vec![div().text_color(palette.faint).child("点左边的 + 加一个 MCP 服务").into_any_element()];
        };
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            div().id(id).flex().items_center().gap(px(8.0)).cursor_pointer().child(label).child(switch((id, 1usize), on, palette))
        };
        let head = heading(server.name.clone(), palette)
            .child(div().flex_1())
            .child(
                div()
                    .id("delete-server")
                    .px(px(9.0))
                    .py(px(4.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(if self.deleting { palette.accent } else { palette.muted })
                    .hover(move |style| style.bg(palette.selected))
                    .child(if self.deleting { "再点一次删除" } else { "删除" })
                    .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
            )
            .child(toggle("ask-first", "调用前询问", self.confirm).on_click(cx.listener(|this, _, _, cx| {
                this.confirm = !this.confirm;
                cx.notify();
            })))
            .child(toggle("server-enabled", "启用", self.enabled).on_click(cx.listener(|this, _, _, cx| {
                this.enabled = !this.enabled;
                cx.notify();
            })));
        let status = self.status.clone().map(|(ok, message)| {
            div().flex_1().min_w(px(0.0)).text_size(px(12.5)).text_color(if ok { palette.muted } else { palette.accent }).child(message)
        });
        vec![
            head.into_any_element(),
            field("名称", boxed(Input::new(&self.name).appearance(false), palette), palette),
            field("本地程序：命令", boxed(Input::new(&self.command).appearance(false), palette), palette),
            field("参数", boxed(Textarea::new(&self.args).appearance(false), palette), palette),
            field("环境变量", boxed(Textarea::new(&self.env).appearance(false), palette), palette),
            field("或者远程服务：地址", boxed(Input::new(&self.url).appearance(false), palette), palette),
            field("请求头", boxed(Textarea::new(&self.headers).appearance(false), palette), palette),
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(button("save-server", "保存", true, palette).on_click(cx.listener(|this, _, window, cx| this.save(window, cx))))
                .child(
                    button("test-server", if self.busy { "正在连接" } else { "测试连接" }, false, palette)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy {
                                this.test(cx);
                            }
                        })),
                )
                .children(status)
                .into_any_element(),
        ]
    }
}

impl Render for ToolsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().child(self.list(cx)).child(
            div()
                .id("tools-form")
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .overflow_y_scroll()
                .px(px(26.0))
                .pt(px(6.0))
                .pb(px(26.0))
                .flex()
                .flex_col()
                .gap(px(18.0))
                .children(self.form(cx)),
        )
    }
}
