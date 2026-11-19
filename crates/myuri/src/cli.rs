//! Command line. Without arguments Myuri starts normally; with one it talks
//! to the running instance or checks the configuration, then exits.

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

use std::io::Write;

use myuri_core::config::{self, ContentKind};
use myuri_core::llm;

use crate::{chat, config_dir, models, secrets};
use crate::tray::{self, Request};

const USAGE: &str = "\
用法：
  myuri                             启动
  myuri check                       检查配置文件，有问题就列出来
  myuri preview [text|image|files]  让正在运行的 Myuri 用示例内容弹出面板
  myuri quit                        让正在运行的 Myuri 退出
  myuri ask <助手> [文字]           不开窗口，直接问一个助手；不写文字就读标准输入
  myuri image <供应商 id> <模型> <描述> [--size 16:9] [--quality low] [--format png]
                                    生成图片，存到“图片”文件夹里的 Myuri
  myuri settings                    打开设置窗口
  myuri models refresh              从 models.dev 更新模型能力目录
  myuri models show <模型名>        看目录里怎么记录某个模型
  myuri models fetch <供应商 id>    向供应商要它的模型列表，和现有的比一比
  myuri models fetch <供应商 id> --sync   并把现有列表改成和它一致
  myuri models fetch --all [--sync] 所有启用的供应商
  myuri models fetch <供应商 id> --sync --keep gpt,codex   只要名字里带这些词的
  myuri key <供应商 id>             从标准输入读一行密钥，存进 Windows 凭据管理器
  myuri key <供应商 id> --delete    删掉存着的密钥";

/// Returns the exit code, or `None` to start the application.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let command = args.first()?.as_str();
    // A windowed program has no console of its own; borrow the caller's so
    // the output shows up in the terminal it was run from.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    Some(match (command, args.get(1).map(String::as_str)) {
        ("check", None) => check(),
        ("preview", kind) => match kind {
            None => request(Request::Preview(None)),
            Some("text") => request(Request::Preview(Some(ContentKind::Text))),
            Some("image") => request(Request::Preview(Some(ContentKind::Image))),
            Some("files") => request(Request::Preview(Some(ContentKind::Files))),
            Some("menu") => request(Request::Menu),
            Some(_) => usage_error(),
        },
        ("quit", None) => request(Request::Quit),
        ("settings", None) => request(Request::Settings),
        ("image", Some(provider)) => picture(provider, &args[2..]),
        ("ask", Some(assistant)) => ask(assistant, &args[2..]),
        ("models", Some(action)) => models_command(action, &args[2..]),
        ("key", Some(provider)) => key(provider, args.get(2).map(String::as_str)),
        ("help" | "--help" | "-h", _) => {
            println!("{USAGE}");
            0
        }
        _ => usage_error(),
    })
}

fn usage_error() -> i32 {
    eprintln!("{USAGE}");
    2
}

fn request(request: Request) -> i32 {
    if tray::send_to_running_instance(request) {
        0
    } else {
        eprintln!("Myuri 没有在运行");
        1
    }
}

/// Ask one assistant and print the reply as it streams.
fn ask(name: &str, words: &[String]) -> i32 {
    let fail = |reason: String| {
        eprintln!("{reason}");
        1
    };
    let dir = config_dir();
    let loaded = config::load_settings(&dir)
        .and_then(|settings| Ok((settings, config::load_assistants(&dir)?, config::load_providers(&dir)?)));
    let (settings, assistants, providers) = match loaded {
        Ok(loaded) => loaded,
        Err(problem) => return fail(problem.to_string()),
    };
    let Some(assistant) = assistants.iter().find(|assistant| assistant.name == name) else {
        return fail(format!("没有叫 \"{name}\" 的助手"));
    };
    let route = match chat::Route::find(&settings, &providers, assistant) {
        Ok(route) => route,
        Err(problem) => return fail(problem),
    };
    let text = if words.is_empty() {
        std::io::read_to_string(std::io::stdin()).unwrap_or_default()
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        return fail("没有要问的内容".into());
    }

    let system = config::load_prompt(&dir, assistant);
    let turns = [llm::Turn::user(text.trim())];
    let catalog = models::load(&dir);
    let info = catalog.lookup(&route.model);
    let (tx, rx) = std::sync::mpsc::channel();
    let _abort = route.send(&system, &turns, assistant.reasoning, assistant.temperature, info, &[], move |event| {
        let _ = tx.send(event);
    });
    let mut out = std::io::stdout();
    for event in rx {
        match event {
            llm::Event::Text(text) => {
                let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
            }
            llm::Event::Reasoning(_) | llm::Event::Call(_) => {}
            llm::Event::Done => {
                println!();
                return 0;
            }
            llm::Event::Error(reason) => return fail(reason),
        }
    }
    fail("连接意外断开".into())
}

fn models_command(action: &str, rest: &[String]) -> i32 {
    let dir = config_dir();
    let outcome = match (action, rest) {
        ("refresh", []) => models::refresh(&dir).map(|count| format!("目录已更新，共 {count} 个模型")),
        // Makes the copy that is built into the program.
        ("build", [from, to]) => models::build(from, to).map(|count| format!("{count} models")),
        ("fetch", [target, flags @ ..]) => {
            let sync = flags.iter().any(|flag| flag == "--sync");
            let keep = flags.iter().position(|flag| flag == "--keep").and_then(|at| flags.get(at + 1));
            let words = keep.map(|words| words.to_lowercase()).unwrap_or_default();
            fetch(&dir, target, sync, &words.split(',').filter(|w| !w.is_empty()).collect::<Vec<_>>())
        }
        ("show", [name]) => Ok(match models::load(&dir).lookup(name) {
            Some(info) => format!("{name}：{info:?}\n可选的思考强度：{:?}", info.levels()),
            None => format!("目录里没有 {name}"),
        }),
        _ => return usage_error(),
    };
    match outcome {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// Compare providers' own model lists with the ones on file, and with `sync`
/// bring the file in line.
fn fetch(dir: &std::path::Path, target: &str, sync: bool, keep: &[&str]) -> Result<String, String> {
    let providers = config::load_providers(dir).map_err(|problem| problem.to_string())?;
    let chosen: Vec<_> = providers
        .iter()
        .filter(|provider| if target == "--all" { provider.enabled } else { provider.id == target })
        .collect();
    if chosen.is_empty() {
        return Err(format!("没有 id 是 \"{target}\" 的供应商"));
    }
    // All at once: most of the time goes into waiting for answers.
    let answers: Vec<_> = std::thread::scope(|scope| {
        let asked: Vec<_> =
            chosen.iter().map(|provider| scope.spawn(|| models::upstream(provider))).collect();
        asked
            .into_iter()
            .map(|thread| thread.join().unwrap_or_else(|_| Err("中断了".into())))
            .collect()
    });
    let catalog = models::load(dir);
    let mut lines = Vec::new();
    for (provider, answer) in chosen.iter().zip(answers) {
        let id = &provider.id;
        lines.push(match answer {
            Err(reason) => {
                // An error page would otherwise fill the screen.
                let brief: String = reason.lines().next().unwrap_or("").chars().take(120).collect();
                format!("{id}：失败，{brief}")
            }
            Ok(mut upstream) => {
                if !keep.is_empty() {
                    upstream.only(provider, |name| {
                        let name = name.to_lowercase();
                        keep.iter().any(|word| name.contains(word))
                    });
                }
                let mut line = format!(
                    "{id}：上游 {} 个，新增 {} 个，下架 {} 个",
                    upstream.names.len(),
                    upstream.added.len(),
                    upstream.gone.len()
                );
                if chosen.len() == 1 {
                    for (label, names) in [("新增", &upstream.added), ("下架", &upstream.gone)] {
                        if !names.is_empty() {
                            let shown: Vec<_> = names.iter().take(30).cloned().collect();
                            line += &format!("\n  {label}：{}", shown.join("、"));
                        }
                    }
                }
                let changed = !upstream.added.is_empty() || !upstream.gone.is_empty();
                // An empty answer is more likely a hiccup than an empty shop.
                if sync && changed && !upstream.names.is_empty() {
                    match models::sync(dir, provider, &upstream, &catalog) {
                        Ok(()) => line += "，已同步",
                        Err(reason) => line += &format!("，没写进去：{reason}"),
                    }
                }
                line
            }
        });
    }
    Ok(lines.join("\n"))
}

/// Generate pictures and print where they were saved.
fn picture(provider: &str, rest: &[String]) -> i32 {
    let [model, prompt, flags @ ..] = rest else { return usage_error() };
    let flag = |name: &str| {
        flags.iter().position(|flag| flag == name).and_then(|at| flags.get(at + 1)).map(String::as_str)
    };
    let request = myuri_core::image::ImageRequest {
        model,
        prompt,
        size: flag("--size"),
        quality: flag("--quality"),
        format: flag("--format"),
    };
    let outcome = config::load_providers(&config_dir())
        .map_err(|problem| problem.to_string())
        .and_then(|providers| {
            let provider = providers
                .iter()
                .find(|candidate| candidate.id == provider)
                .ok_or_else(|| format!("没有 id 是 \"{provider}\" 的供应商"))?;
            crate::images::generate(provider, &request)
        })
        .and_then(|pictures| {
            let folder = crate::images::folder();
            pictures.iter().enumerate().map(|(index, bytes)| crate::images::save(&folder, bytes, index)).collect::<Result<Vec<_>, _>>()
        });
    match outcome {
        Ok(paths) => {
            paths.iter().for_each(|path| println!("{}", path.display()));
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// Store or delete a provider key. The key comes from standard input so it
/// never appears in a command line, a shell history or a process list.
fn key(provider: &str, option: Option<&str>) -> i32 {
    let outcome = match option {
        Some("--delete") => secrets::delete(provider).map(|existed| {
            if existed { "已删除" } else { "本来就没有存" }
        }),
        Some(_) => return usage_error(),
        None => {
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                eprintln!("没有读到密钥。用法示例：Get-Clipboard | myuri key {provider}");
                return 1;
            }
            secrets::write(provider, line.trim()).map(|()| "已保存")
        }
    };
    match outcome {
        Ok(message) => {
            println!("{provider}：{message}");
            0
        }
        Err(error) => {
            eprintln!("{provider}：{error}");
            1
        }
    }
}

fn check() -> i32 {
    let dir = config_dir();
    if !dir.exists() {
        println!("{} 还不存在，启动一次 Myuri 会生成默认配置", dir.display());
        return 0;
    }
    let settings = config::load_settings(&dir);
    let assistants = config::load_assistants(&dir);
    let providers = config::load_providers(&dir);
    let mut problems: Vec<String> = [
        settings.as_ref().err(),
        assistants.as_ref().err(),
        providers.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .map(ToString::to_string)
    .collect();
    // With all three files readable, check that they fit together.
    if let (Ok(settings), Ok(assistants), Ok(providers)) = (&settings, &assistants, &providers) {
        let mut keyless = Vec::new();
        for assistant in assistants.iter().filter(|assistant| assistant.enabled) {
            let name = &assistant.name;
            match settings.model_for(assistant) {
                Err(problem) => problems.push(problem),
                Ok(model) => match providers.iter().find(|p| p.id == model.provider) {
                    None => problems
                        .push(format!("助手 \"{name}\" 用的供应商 \"{}\" 不在 providers.toml 里", model.provider)),
                    Some(provider) if !provider.enabled => {
                        problems.push(format!("助手 \"{name}\" 用的供应商 \"{}\" 被停用了", provider.id));
                    }
                    Some(provider) => {
                        if secrets::read(&provider.id).is_none() && !keyless.contains(&provider.id) {
                            keyless.push(provider.id.clone());
                        }
                    }
                },
            }
            if !dir.join(assistant.prompt_path()).is_file() {
                problems.push(format!("助手 \"{name}\" 的提示词文件 {} 不存在", assistant.prompt_path()));
            }
        }
        for id in keyless {
            problems.push(format!("供应商 \"{id}\" 还没有密钥，用 myuri key {id} 设置"));
        }
    }
    for problem in &problems {
        println!("{problem}");
    }
    if problems.is_empty() {
        println!("{} 里的配置没有问题", dir.display());
        0
    } else {
        1
    }
}
