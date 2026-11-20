//! Running MCP servers and calling their tools. A server is started the
//! first time an assistant that lists it is used and then kept for the life
//! of the program. Everything here blocks: call it off the UI thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::TerminateProcess;

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
    process: Arc<ProcessControl>,
    next: u64,
    /// The tools as the model sees them, with the names the server uses.
    tools: Vec<(Tool, String)>,
}

/// The handle keeps Windows from reusing the ID while taskkill walks the tree.
struct Program {
    pid: u32,
    handle: OwnedHandle,
}

impl Program {
    fn terminate(self) {
        kill_tree(self.pid);
        // Ensure the owned child also exits if taskkill could not run.
        unsafe {
            let _ = TerminateProcess(HANDLE(self.handle.as_raw_handle()), 1);
        }
    }
}

fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Default)]
struct ProcessState {
    stopped: bool,
    program: Option<Program>,
}

/// Access to the child independent of the lock held during a tool call.
#[derive(Default)]
struct ProcessControl(Mutex<ProcessState>);

impl ProcessControl {
    fn check(&self) -> Result<(), String> {
        if self.0.lock().unwrap().stopped {
            return Err("服务已停止".into());
        }
        Ok(())
    }

    fn attach(&self, child: &Child) -> Result<(), String> {
        let handle = child.as_handle().try_clone_to_owned().map_err(|error| error.to_string())?;
        let mut state = self.0.lock().unwrap();
        if state.stopped {
            return Err("服务已停止".into());
        }
        state.program = Some(Program { pid: child.id(), handle });
        Ok(())
    }

    fn stop(&self) -> Option<Program> {
        let mut state = self.0.lock().unwrap();
        state.stopped = true;
        state.program.take()
    }
}

struct CachedSession {
    server: Server,
    process: Arc<ProcessControl>,
    session: Mutex<Option<Session>>,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<CachedSession>>>> = LazyLock::new(Default::default);
static PROBES: LazyLock<Mutex<HashMap<String, Vec<Weak<CachedSession>>>>> = LazyLock::new(Default::default);

impl Drop for Session {
    fn drop(&mut self) {
        if let Link::Program { child, .. } = &mut self.link {
            let program = self.process.0.lock().unwrap().program.take();
            if let Some(program) = program {
                program.terminate();
            }
            let _ = child.wait();
        }
    }
}

fn read_stdio(mut source: impl BufRead, tx: Sender<String>) {
    let mut bytes = Vec::new();
    while let Ok(read) = source.read_until(b'\n', &mut bytes) {
        if read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&bytes);
        if tx.send(line.trim_end_matches(['\r', '\n']).to_string()).is_err() {
            break;
        }
        bytes.clear();
    }
}

impl Session {
    fn start(server: &Server, process: Arc<ProcessControl>) -> Result<Self, String> {
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
            if let Err(reason) = process.attach(&child) {
                kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(reason);
            }
            let stdin = child.stdin.take().ok_or("拿不到服务的输入")?;
            let stdout = child.stdout.take().ok_or("拿不到服务的输出")?;
            let (tx, lines) = channel();
            std::thread::spawn(move || read_stdio(BufReader::new(stdout), tx));
            Link::Program { child, stdin, lines }
        } else {
            Link::Http { session: None }
        };
        let mut session = Self { server: server.clone(), link, process, next: 1, tools: Vec::new() };
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

impl CachedSession {
    fn new(server: &Server) -> Self {
        Self {
            server: server.clone(),
            process: Arc::default(),
            session: Mutex::new(None),
        }
    }

    fn run<T>(&self, use_session: impl FnOnce(&mut Session) -> Result<T, String>) -> Result<T, String> {
        let mut active = self.session.lock().unwrap();
        self.process.check()?;
        if active.is_none() {
            let started = Instant::now();
            let server = &self.server;
            let session = Session::start(server, self.process.clone())
                .inspect_err(|reason| info!("mcp: {} did not start: {reason}", server.name))?;
            self.process.check()?;
            info!("mcp: {} started in {} ms, {} tools", server.name, started.elapsed().as_millis(), session.tools.len());
            *active = Some(session);
        }
        let result = use_session(active.as_mut().expect("started MCP session"));
        if result.as_ref().is_err_and(|reason| reason.contains("退出")) {
            // A dead child is started again on the next call.
            *active = None;
        }
        result
    }

    fn stop(&self) {
        if let Some(program) = self.process.stop() {
            program.terminate();
        }
    }
}

/// Reserve the slot before starting the child so stop can reach a server
/// that is still initializing. Closed settings also invalidate open chats.
fn session(server: &Server, dir: &Path) -> Result<Arc<CachedSession>, String> {
    let mut sessions = SESSIONS.lock().unwrap();
    let configured = mcp::load_servers(dir)?;
    let Some(current) = configured
        .into_iter()
        .find(|current| current.enabled && current.name == server.name)
    else {
        return Err("MCP 服务已删除或停用".into());
    };
    if let Some(known) = sessions.get(&current.name).filter(|known| known.server == current) {
        return Ok(known.clone());
    }
    let slot = Arc::new(CachedSession::new(&current));
    let previous = sessions.insert(current.name.clone(), slot.clone());
    drop(sessions);
    if let Some(previous) = previous {
        previous.stop();
    }
    Ok(slot)
}

/// Shut down this service without waiting on its current call on the UI thread.
pub fn stop_server(name: &str) {
    let live = SESSIONS.lock().unwrap().remove(name);
    let probes = PROBES.lock().unwrap().remove(name).unwrap_or_default();
    let stopped: Vec<_> = live.into_iter()
        .chain(probes.iter().filter_map(Weak::upgrade))
        .map(|slot| {
            let program = slot.process.stop();
            (slot, program)
        })
        .collect();
    if !stopped.is_empty() {
        std::thread::spawn(move || {
            for (slot, program) in stopped {
                if let Some(program) = program {
                    program.terminate();
                }
                drop(slot);
            }
        });
    }
}

/// Stop every server. They are also gone when the program exits.
pub fn stop_all() {
    let live = std::mem::take(&mut *SESSIONS.lock().unwrap());
    let probes = std::mem::take(&mut *PROBES.lock().unwrap());
    for slot in live.into_values().chain(probes.into_values().flatten().filter_map(|slot| slot.upgrade())) {
        slot.stop();
    }
}

/// Start a server just to see what it offers: the names of its tools.
pub fn probe(server: &Server) -> Result<Vec<String>, String> {
    CachedSession::new(server).run(|session| Ok(session.tools.iter().map(|(_, name)| name.clone()).collect()))
}

/// Track the test by the saved service name even if the draft was renamed.
/// The test owns its session and drops it after listing tools.
pub fn prepare_probe(
    saved_name: &str,
    server: &Server,
) -> impl FnOnce() -> Result<Vec<String>, String> + Send + 'static + use<> {
    let slot = Arc::new(CachedSession::new(server));
    let saved_name = saved_name.to_string();
    let weak = Arc::downgrade(&slot);
    PROBES.lock().unwrap().entry(saved_name.clone()).or_default().push(weak.clone());
    move || {
        let result = slot.run(|session| Ok(session.tools.iter().map(|(_, name)| name.clone()).collect()));
        drop(slot);
        let mut probes = PROBES.lock().unwrap();
        if let std::collections::hash_map::Entry::Occupied(mut named) = probes.entry(saved_name) {
            named.get_mut().retain(|entry| !entry.ptr_eq(&weak) && entry.strong_count() > 0);
            if named.get().is_empty() {
                named.remove();
            }
        }
        result
    }
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
    dir: PathBuf,
    /// Model-facing tool name to its server and the server's own name for it.
    routes: HashMap<String, (Server, String)>,
}

impl Kit {
    /// Gather the tools of the servers and the skills `assistant` lists.
    pub fn gather(dir: &Path, assistant: &Assistant) -> Self {
        let mut kit = Self { dir: dir.to_path_buf(), ..Self::default() };
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
                        let tools = session(server, dir).and_then(|slot| slot.run(|session| Ok(session.tools.clone())));
                        match tools {
                            Err(reason) => kit.problems.push(format!("{}：{reason}", server.name)),
                            Ok(tools) => {
                                for (tool, original) in tools {
                                    kit.routes.insert(tool.name.clone(), (server.clone(), original));
                                    kit.tools.push(tool);
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
        let result = session(server, &self.dir)?.run(|session| {
            session.ask(|id| mcp::call_tool(id, original, arguments), CALL)
        })?;
        mcp::result_text(&result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_reader_keeps_later_lines_after_non_utf8_output() {
        let (tx, rx) = channel();
        let output = b"banner \xd6\xd0\xce\xc4\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        read_stdio(&output[..], tx);
        assert!(rx.recv().unwrap().contains('\u{fffd}'));
        let reply = rx.recv().unwrap();
        assert!(mcp::reply(&reply, 1).unwrap().is_ok());
        assert!(rx.recv().is_err());
    }
}
