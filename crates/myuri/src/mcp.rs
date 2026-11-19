//! Running MCP servers and calling their tools. A server is started the
//! first time an assistant that lists it is used and then kept for the life
//! of the program. Everything here blocks: call it off the UI thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use myuri_core::config::Assistant;
use myuri_core::llm::Tool;
use myuri_core::mcp::{self, SKILL_TOOL, Server, Skill};

use crate::applog::info;
use crate::net;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Starting can include a download (`uvx`, `npx`).
const START: Duration = Duration::from_secs(90);
const LIST: Duration = Duration::from_secs(30);
const CALL: Duration = Duration::from_secs(180);

enum Link {
    Program { child: Child, stdin: ChildStdin, lines: Receiver<String> },
    Http { session: Option<String> },
}

struct Session {
    server: Server,
    link: Link,
    next: u64,
    /// The tools as the model sees them, with the names the server uses.
    tools: Vec<(Tool, String)>,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Session>>>>> = LazyLock::new(Default::default);

impl Drop for Session {
    fn drop(&mut self) {
        if let Link::Program { child, .. } = &mut self.link {
            // The program may sit behind a shell; take the whole tree down.
            let _ = Command::new("taskkill")
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.wait();
        }
    }
}

impl Session {
    fn start(server: &Server) -> Result<Self, String> {
        let link = if server.url.is_empty() {
            // Through the shell, so that `npx` and other script shims resolve.
            let mut child = Command::new("cmd")
                .arg("/C")
                .arg(&server.command)
                .args(&server.args)
                .envs(&server.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|error| format!("启动不了 {}：{error}", server.command))?;
            let stdin = child.stdin.take().ok_or("拿不到服务的输入")?;
            let stdout = child.stdout.take().ok_or("拿不到服务的输出")?;
            let (tx, lines) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            Link::Program { child, stdin, lines }
        } else {
            Link::Http { session: None }
        };
        let mut session = Self { server: server.clone(), link, next: 1, tools: Vec::new() };
        session.ask(|id| mcp::initialize(id, env!("CARGO_PKG_VERSION")), START)?;
        session.tell(mcp::initialized());
        let mut cursor: Option<String> = None;
        loop {
            let page = session.ask(|id| mcp::list_tools(id, cursor.as_deref()), LIST)?;
            session.tools.extend(mcp::tools_of(&server.name, &page));
            cursor = page["nextCursor"].as_str().filter(|next| !next.is_empty()).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(session)
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json, text/event-stream".to_string()),
        ];
        headers.extend(self.server.headers.iter().map(|(name, value)| (name.clone(), value.clone())));
        if let Link::Http { session: Some(session) } = &self.link {
            headers.push(("mcp-session-id".into(), session.clone()));
        }
        headers
    }

    /// Send a message that expects no answer.
    fn tell(&mut self, message: Value) {
        let headers = self.headers();
        match &mut self.link {
            Link::Program { stdin, .. } => {
                let _ = writeln!(stdin, "{message}").and_then(|()| stdin.flush());
            }
            Link::Http { .. } => {
                let _ = net::post(&self.server.url, &headers, message.to_string().as_bytes());
            }
        }
    }

    /// Send a request and wait for its result.
    fn ask(&mut self, build: impl Fn(u64) -> Value, wait: Duration) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let message = build(id).to_string();
        let headers = self.headers();
        match &mut self.link {
            Link::Http { session } => {
                let (status, body, issued) =
                    net::post_for(&self.server.url, &headers, message.as_bytes(), "Mcp-Session-Id")?;
                if issued.is_some() {
                    *session = issued;
                }
                let body = String::from_utf8_lossy(&body);
                match mcp::reply(&body, id) {
                    Some(result) => result,
                    None if !(200..300).contains(&status) => Err(format!("服务返回 HTTP {status}")),
                    None => Err("服务没有回答".into()),
                }
            }
            Link::Program { stdin, lines, .. } => {
                writeln!(stdin, "{message}").and_then(|()| stdin.flush()).map_err(|_| "服务已经退出".to_string())?;
                let deadline = Instant::now() + wait;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let line = match lines.recv_timeout(left) {
                        Ok(line) => line,
                        Err(RecvTimeoutError::Timeout) => return Err("服务太久没有回答".into()),
                        Err(RecvTimeoutError::Disconnected) => return Err("服务已经退出".into()),
                    };
                    if let Some(result) = mcp::reply(&line, id) {
                        return result;
                    }
                    // The server may ask things of its own; decline politely.
                    let Ok(asked) = serde_json::from_str::<Value>(&line) else { continue };
                    if asked["method"].is_string() && !asked["id"].is_null() {
                        let answer = match asked["method"].as_str() {
                            Some("ping") => json!({ "jsonrpc": "2.0", "id": asked["id"], "result": {} }),
                            _ => json!({
                                "jsonrpc": "2.0",
                                "id": asked["id"],
                                "error": { "code": -32601, "message": "Method not found" },
                            }),
                        };
                        let _ = writeln!(stdin, "{answer}").and_then(|()| stdin.flush());
                    }
                }
            }
        }
    }
}

/// The running session for `server`, started if need be. A server whose
/// settings changed is started afresh.
fn session(server: &Server) -> Result<Arc<Mutex<Session>>, String> {
    let known = SESSIONS.lock().unwrap().get(&server.name).cloned();
    if let Some(session) = known {
        if session.lock().unwrap().server == *server {
            return Ok(session);
        }
    }
    let started = Instant::now();
    let session = Session::start(server).inspect_err(|reason| info!("mcp: {} did not start: {reason}", server.name))?;
    info!("mcp: {} started in {} ms, {} tools", server.name, started.elapsed().as_millis(), session.tools.len());
    let session = Arc::new(Mutex::new(session));
    SESSIONS.lock().unwrap().insert(server.name.clone(), session.clone());
    Ok(session)
}

/// Stop every server. They are also gone when the program exits.
pub fn stop_all() {
    SESSIONS.lock().unwrap().clear();
}

/// Start a server just to see what it offers: the names of its tools.
pub fn probe(server: &Server) -> Result<Vec<String>, String> {
    SESSIONS.lock().unwrap().remove(&server.name);
    let session = session(server)?;
    let names = session.lock().unwrap().tools.iter().map(|(_, name)| name.clone()).collect();
    Ok(names)
}

/// Everything an assistant may call, and how to carry the calls out.
#[derive(Default)]
pub struct Kit {
    pub tools: Vec<Tool>,
    /// Added to the system prompt: the list of skills.
    pub prompt: String,
    /// Servers that could not be used, to tell the user.
    pub problems: Vec<String>,
    skills: Vec<Skill>,
    /// Model-facing tool name to its server and the server's own name for it.
    routes: HashMap<String, (Server, String)>,
}

impl Kit {
    /// Gather the tools of the servers and the skills `assistant` lists.
    pub fn gather(dir: &Path, assistant: &Assistant) -> Self {
        let mut kit = Self::default();
        if !assistant.tools.is_empty() {
            let every = assistant.tools.iter().any(|name| name == "*");
            match mcp::load_servers(dir) {
                Err(problem) => kit.problems.push(problem),
                Ok(servers) => {
                    for name in assistant.tools.iter().filter(|name| *name != "*") {
                        if !servers.iter().any(|server| &server.name == name) {
                            kit.problems.push(format!("没有叫 {name} 的 MCP 服务"));
                        }
                    }
                    let wanted = servers.iter().filter(|server| server.enabled && (every || assistant.tools.contains(&server.name)));
                    for server in wanted {
                        match session(server) {
                            Err(reason) => kit.problems.push(format!("{}：{reason}", server.name)),
                            Ok(session) => {
                                for (tool, original) in &session.lock().unwrap().tools {
                                    kit.routes.insert(tool.name.clone(), (server.clone(), original.clone()));
                                    kit.tools.push(tool.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        kit.skills = mcp::chosen_skills(&mcp::load_skills(dir), &assistant.skills);
        if !kit.skills.is_empty() {
            kit.prompt = mcp::skills_prompt(&kit.skills);
            kit.tools.push(mcp::skill_tool(&kit.skills));
        }
        kit
    }

    /// The user is asked before this tool runs.
    pub fn asks_first(&self, tool: &str) -> bool {
        self.routes.get(tool).is_some_and(|(server, _)| server.confirm)
    }

    /// The server a tool belongs to; skills have none.
    pub fn server(&self, tool: &str) -> Option<&str> {
        self.routes.get(tool).map(|(server, _)| server.name.as_str())
    }

    /// How a tool is shown: its server and its own name.
    pub fn label(&self, tool: &str) -> String {
        match self.routes.get(tool) {
            Some((server, original)) => format!("{} · {original}", server.name),
            None if tool == SKILL_TOOL => "技能".into(),
            None => tool.to_string(),
        }
    }

    /// Carry out a call and return what it says.
    pub fn call(&self, tool: &str, arguments: &str) -> Result<String, String> {
        if tool == SKILL_TOOL {
            return mcp::read_skill(&self.skills, arguments);
        }
        let (server, original) = self.routes.get(tool).ok_or_else(|| format!("没有 {tool} 这个工具"))?;
        let session = session(server)?;
        let result = session.lock().unwrap().ask(|id| mcp::call_tool(id, original, arguments), CALL);
        match result {
            Ok(result) => mcp::result_text(&result),
            Err(reason) => {
                // A dead server is started again on the next call.
                if reason.contains("退出") {
                    SESSIONS.lock().unwrap().remove(&server.name);
                }
                Err(reason)
            }
        }
    }
}
