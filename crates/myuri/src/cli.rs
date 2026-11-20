//! Command line. Without arguments Myuri starts normally; with one it talks
//! to the running instance or checks the configuration, then exits.

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

use std::io::Write;
use std::sync::{Arc, atomic::AtomicBool};

use myuri_core::config::{self, ContentKind};
use myuri_core::image::{ImageRequest, ReferenceImage};
use myuri_core::llm;

use crate::{autostart, chat, config_dir, models, secrets};
use crate::tray::{self, Request};

const USAGE: &str = "\
用法：
  myuri                             启动
  myuri --version                   显示当前版本（或 -V）
  myuri check                       检查配置文件，有问题就列出来
  myuri preview [text|image|files]  让正在运行的 Myuri 用示例内容弹出面板
  myuri quit                        让正在运行的 Myuri 退出
  myuri autostart [on|off]          查看或开关开机自启
  myuri ask <助手> [文字]           不开窗口，直接问一个助手；不写文字就读标准输入
  myuri image <供应商 id> <模型> <描述> [--size 16:9] [--quality low] [--format png]
             [--resolution 1K|2K|4K] [--ref <图片> ...]
                                    生成或编辑图片，存到“图片”文件夹里的 Myuri
  myuri settings                    打开设置窗口
  myuri models refresh              从 models.dev 更新模型能力目录
  myuri models show <模型名>        看目录里怎么记录某个模型
  myuri models fetch <供应商 id>    向供应商要它的模型列表，和现有的比一比
  myuri models fetch <供应商 id> --sync   并把现有列表改成和它一致
  myuri models fetch --all [--sync] 所有启用的供应商
  myuri models fetch <供应商 id> --sync --keep gpt,codex   只要名字里带这些词的
  myuri key [--image|--speech] <供应商 id> [--delete]
                                    从标准输入保存密钥，或删除对应渠道的密钥
  myuri mcp <服务名>                启动 mcp.toml 里的一个服务，列出它的工具
  myuri mcp <服务名> <工具> [JSON]  调用它的一个工具";

/// Returns the exit code, or `None` to start the application.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let command = args.first()?.as_str();
    // A windowed program has no console of its own; borrow the caller's so
    // the output shows up in the terminal it was run from.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    Some(match (command, args.get(1).map(String::as_str)) {
        ("--version" | "-V", None) => {
            println!("Myuri {}", crate::VERSION);
            0
        }
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
        ("autostart", action) => autostart_command(action),
        ("image", Some(provider)) => picture(provider, &args[2..]),
        ("ask", Some(assistant)) => ask(assistant, &args[2..]),
        ("models", Some(action)) => models_command(action, &args[2..]),
        ("key", Some(_)) => key(&args[1..]),
        ("mcp", Some(server)) => mcp_command(server, &args[2..]),
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

fn autostart_command(action: Option<&str>) -> i32 {
    match action {
        None => {
            match autostart::status() {
                autostart::Status::Enabled => println!("开机自启：已开启"),
                autostart::Status::Disabled => println!("开机自启：未开启"),
                autostart::Status::BlockedByWindows => {
                    println!("开机自启：已登记，但被 Windows 启动设置禁用了（执行 myuri autostart on 可重新启用）");
                }
                autostart::Status::OtherPath(path) => {
                    println!("开机自启：当前指向另一路径 {path}（执行 myuri autostart on 可切到当前程序）");
                }
            }
            0
        }
        Some("on" | "enable" | "true") => match autostart::set(true) {
            Ok(()) => {
                println!("开机自启：已开启");
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        },
        Some("off" | "disable" | "false") => match autostart::set(false) {
            Ok(()) => {
                println!("开机自启：已关闭");
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        },
        Some(_) => usage_error(),
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
                upstream.only(&provider.models, |name| {
                    !models::is_non_chat(name, &catalog) && (keep.is_empty() || {
                        let name = name.to_lowercase();
                        keep.iter().any(|word| name.contains(word))
                    })
                });
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
    let mut references = Vec::new();
    let mut request = ImageRequest {
        model,
        prompt,
        size: None,
        quality: None,
        format: None,
        resolution: None,
        references: &[],
    };
    let mut flags = flags.iter().map(String::as_str);
    while let Some(flag) = flags.next() {
        let Some(value) = flags.next() else { return usage_error() };
        match flag {
            "--size" => request.size = Some(value),
            "--quality" => request.quality = Some(value),
            "--format" => request.format = Some(value),
            "--resolution" if matches!(value, "1K" | "2K" | "4K") => request.resolution = Some(value),
            "--ref" => {
                let reference = std::fs::read(value).map_err(|error| error.to_string())
                    .and_then(|bytes| ReferenceImage::from_bytes(bytes).ok_or_else(|| "无法识别参考图格式".to_string()));
                match reference {
                    Ok(image) => references.push(image),
                    Err(error) => {
                        eprintln!("参考图：{error}");
                        return 1;
                    }
                }
            }
            _ => return usage_error(),
        }
    }
    request.references = &references;
    let dir = config_dir();
    let outcome = crate::app::migrate_providers(&dir, &[])
        .and_then(|_| config::load_image_providers(&dir).map_err(|problem| problem.to_string()))
        .and_then(|providers| {
            let provider = providers
                .iter()
                .find(|candidate| candidate.id == provider && candidate.enabled)
                .ok_or_else(|| format!("没有已启用的生图供应商 \"{provider}\""))?;
            crate::images::generate(provider, &request, Arc::new(AtomicBool::new(false)))
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

/// Start an MCP server from `mcp.toml` and list its tools, or call one.
fn mcp_command(name: &str, rest: &[String]) -> i32 {
    let servers = match myuri_core::mcp::load_servers(&crate::config_dir()) {
        Ok(servers) => servers,
        Err(problem) => {
            eprintln!("{problem}");
            return 1;
        }
    };
    let Some(server) = servers.iter().find(|server| server.name == name) else {
        eprintln!("mcp.toml 里没有叫 {name} 的服务");
        return 1;
    };
    let outcome = match rest {
        [] => crate::mcp::probe(server).map(|tools| tools.join("
")),
        [tool, arguments @ ..] => {
            // An assistant that lists just this server gets exactly its tools.
            let written = format!("[[assistant]]
name = \"mcp\"
tools = [{name:?}]
");
            config::parse_assistants(&written).and_then(|assistants| {
                let kit = crate::mcp::Kit::gather(&crate::config_dir(), &assistants[0]);
                match kit.problems.first() {
                    Some(problem) => Err(problem.clone()),
                    None => kit.call(
                        &myuri_core::mcp::tool_name(name, tool),
                        arguments.first().map(String::as_str).unwrap_or("{}"),
                    ),
                }
            })
        }
    };
    crate::mcp::stop_all();
    match outcome {
        Ok(text) => {
            println!("{text}");
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
fn key(args: &[String]) -> i32 {
    let (namespace, flag, args) = match args.first().map(String::as_str) {
        Some("--image") => (secrets::NS_IMAGE, " --image", &args[1..]),
        Some("--speech") => (secrets::NS_SPEECH, " --speech", &args[1..]),
        _ => (secrets::NS_CHAT, "", args),
    };
    let [provider, options @ ..] = args else { return usage_error() };
    if options.len() > 1 || !config::valid_provider_id(provider) {
        return usage_error();
    }
    let option = options.first().map(String::as_str);
    let outcome = match option {
        Some("--delete") => secrets::delete_in(namespace, provider).map(|existed| {
            if existed { "已删除" } else { "本来就没有存" }
        }),
        Some(_) => return usage_error(),
        None => {
            let Ok(text) = std::io::read_to_string(std::io::stdin()) else {
                eprintln!("没有读到密钥。用法示例：Get-Clipboard | myuri key{flag} {provider}");
                return 1;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                eprintln!("没有读到密钥。用法示例：Get-Clipboard | myuri key{flag} {provider}");
                return 1;
            }
            secrets::write_in(namespace, provider, trimmed).map(|()| "已保存")
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
    let images = config::load_image_providers(&dir);
    let speech = config::load_speech_providers(&dir);
    let unmigrated = providers.is_ok()
        && (!dir.join(config::IMAGE_PROVIDERS_FILE).exists()
            || !dir.join(config::SPEECH_PROVIDERS_FILE).exists());
    let mut problems: Vec<String> = [
        settings.as_ref().err(),
        assistants.as_ref().err(),
        providers.as_ref().err(),
        images.as_ref().err(),
        speech.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .map(ToString::to_string)
    .collect();
    if unmigrated {
        problems.push("提示：启动一次 Myuri（或打开设置）会自动从旧版 providers.toml 迁移生图与语音渠道。".into());
    }
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
    if let (Ok(settings), Ok(speech)) = (&settings, &speech) {
        for (kind, route) in [("asr", &settings.speech.asr), ("tts", &settings.speech.tts)] {
            if let Some(reference) = route.as_deref().and_then(config::ModelRef::parse) {
                match speech.iter().find(|p| p.id == reference.provider) {
                    None => problems.push(format!("[speech].{kind} 用的渠道 \"{}\" 不在 speech_providers.toml 里", reference.provider)),
                    Some(p) if !p.enabled => problems.push(format!("[speech].{kind} 用的渠道 \"{}\" 被停用了", p.id)),
                    Some(p) if secrets::read_in(secrets::NS_SPEECH, &p.id).is_none_or(|key| p.keys_to_try(&key, 0).is_empty()) => {
                        problems.push(format!("语音渠道 \"{}\" 没有可用的密钥", p.id));
                    }
                    Some(_) => {}
                }
            }
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
