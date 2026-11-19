//! Tools from outside the program: MCP servers listed in `mcp.toml`, and
//! skills, which are folders with a `SKILL.md` the model reads on demand.
//! This module holds the configuration and the wording of the protocol;
//! starting servers and talking to them is the application's part.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::Tool;

pub const MCP_FILE: &str = "mcp.toml";
pub const SKILLS_DIR: &str = "skills";
/// The tool through which the model reads a skill.
pub const SKILL_TOOL: &str = "load_skill";
const PROTOCOL: &str = "2025-06-18";

pub const DEFAULT_MCP: &str = r#"# MCP 服务。助手在 assistants.toml 里用 tools = ["名字"] 挂上它们。
# 保存后立即生效。
#
# 本地程序（标准输入输出）：
# [[server]]
# name = "fetch"
# command = "uvx"
# args = ["mcp-server-fetch"]
# env = { HTTP_PROXY = "http://127.0.0.1:7890" }   # 可选
#
# 远程服务（HTTP）：
# [[server]]
# name = "docs"
# url = "https://example.com/mcp"
# headers = { Authorization = "Bearer ..." }       # 可选
#
# enabled = false   暂时停用
# confirm = false   调用它的工具前不再询问（默认每次都问）
"#;

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub name: String,
    /// A program speaking MCP over its standard input and output.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Or the address of a server speaking MCP over HTTP.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Ask before each call of one of its tools.
    #[serde(default = "yes")]
    pub confirm: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    server: Vec<Server>,
}

fn plain(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn parse_servers(source: &str) -> Result<Vec<Server>, String> {
    let file: File = toml::from_str(source).map_err(|error| error.message().to_string())?;
    for (index, server) in file.server.iter().enumerate() {
        if !plain(&server.name) {
            return Err(format!("服务的 name 只能用字母、数字、- 和 _：\"{}\"", server.name));
        }
        if file.server[..index].iter().any(|other| other.name == server.name) {
            return Err(format!("有两个服务都叫 \"{}\"", server.name));
        }
        if server.command.is_empty() == server.url.is_empty() {
            return Err(format!("服务 \"{}\" 要么写 command，要么写 url，二选一", server.name));
        }
    }
    Ok(file.server)
}

/// The servers in `dir`. A missing file means none.
pub fn load_servers(dir: &Path) -> Result<Vec<Server>, String> {
    match fs::read_to_string(dir.join(MCP_FILE)) {
        Ok(source) => parse_servers(source.trim_start_matches('\u{feff}')).map_err(|problem| format!("{MCP_FILE}：{problem}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("{MCP_FILE} 读不出来：{error}")),
    }
}

/// The name a server's tool goes by towards the model: providers accept only
/// letters, digits, `_` and `-`, at most 64 of them.
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean: String = tool.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{server}__{clean}").chars().take(64).collect()
}

/// The server a tool name belongs to.
pub fn server_of(name: &str) -> Option<&str> {
    name.split_once("__").map(|(server, _)| server)
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn initialize(id: u64, version: &str) -> Value {
    request(
        id,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": { "name": "Myuri", "version": version },
        }),
    )
}

pub fn initialized() -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

pub fn list_tools(id: u64, cursor: Option<&str>) -> Value {
    request(id, "tools/list", cursor.map(|cursor| json!({ "cursor": cursor })).unwrap_or_else(|| json!({})))
}

pub fn call_tool(id: u64, name: &str, arguments: &str) -> Value {
    let arguments = serde_json::from_str::<Value>(arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
    request(id, "tools/call", json!({ "name": name, "arguments": arguments }))
}

/// The answer to request `id` in what a server sent: one JSON message, or an
/// event stream of them. `None` when it is not in there.
pub fn reply(body: &str, id: u64) -> Option<Result<Value, String>> {
    let messages = body.lines().filter_map(|line| {
        let line = line.trim();
        let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        serde_json::from_str::<Value>(data).ok()
    });
    // A pretty-printed body is one message over several lines.
    let whole = serde_json::from_str::<Value>(body.trim()).ok();
    whole.into_iter().chain(messages).find(|message| message["id"].as_u64() == Some(id)).map(|message| {
        match message.get("error") {
            Some(error) => Err(error["message"].as_str().unwrap_or("服务报了错").to_string()),
            None => Ok(message["result"].clone()),
        }
    })
}

/// The tools in a `tools/list` result, as the model sees them, each with the
/// name the server knows it by.
pub fn tools_of(server: &str, result: &Value) -> Vec<(Tool, String)> {
    result["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool["name"].as_str()?;
            let schema = match &tool["inputSchema"] {
                schema if schema.is_object() => schema.clone(),
                _ => json!({ "type": "object", "properties": {} }),
            };
            let described = Tool {
                name: tool_name(server, name),
                description: tool["description"].as_str().unwrap_or_default().to_string(),
                schema,
            };
            Some((described, name.to_string()))
        })
        .collect()
}

/// What a `tools/call` result says, as text for the model.
pub fn result_text(result: &Value) -> Result<String, String> {
    let mut parts = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or_default() {
            "text" => parts.push(item["text"].as_str().unwrap_or_default().to_string()),
            "resource" => parts.push(item["resource"]["text"].as_str().unwrap_or_default().to_string()),
            kind => parts.push(format!("[{kind}]")),
        }
    }
    if parts.is_empty() && !result["structuredContent"].is_null() {
        parts.push(result["structuredContent"].to_string());
    }
    let text = parts.join("\n");
    if result["isError"] == true { Err(if text.is_empty() { "工具报了错".into() } else { text }) } else { Ok(text) }
}

/// A skill: instructions the model loads when a task calls for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `SKILL.md` without its front matter.
    pub body: String,
    pub folder: PathBuf,
}

/// Split `---` front matter off a Markdown file into its fields.
fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---") else { return (BTreeMap::new(), text) };
    let Some(end) = rest.find("\n---") else { return (BTreeMap::new(), text) };
    let fields = rest[..end]
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().trim_matches(['"', '\'']).to_string()))
        .collect();
    let body = rest[end + 4..].trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    (fields, body)
}

/// Every skill in `dir`'s skills folder, by name.
pub fn load_skills(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(dir.join(SKILLS_DIR)) else { return Vec::new() };
    let mut skills: Vec<Skill> = entries
        .flatten()
        .filter_map(|entry| {
            let folder = entry.path();
            let text = fs::read_to_string(folder.join("SKILL.md")).ok()?;
            let (fields, body) = front_matter(&text);
            let fallback = folder.file_name()?.to_string_lossy().into_owned();
            Some(Skill {
                name: fields.get("name").filter(|name| !name.is_empty()).cloned().unwrap_or(fallback),
                description: fields.get("description").cloned().unwrap_or_default(),
                body: body.trim().to_string(),
                folder,
            })
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// The skills an assistant lists; `*` stands for all of them.
pub fn chosen_skills(all: &[Skill], wanted: &[String]) -> Vec<Skill> {
    let every = wanted.iter().any(|name| name == "*");
    all.iter().filter(|skill| every || wanted.contains(&skill.name)).cloned().collect()
}

/// What is added to the system prompt so the model knows its skills.
pub fn skills_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let list: Vec<String> = skills.iter().map(|skill| format!("- {}: {}", skill.name, skill.description)).collect();
    format!(
        "\n\n# Skills\nYou have skills: instructions for particular kinds of task. When a task matches one, \
call `{SKILL_TOOL}` with its name first and follow what it says.\n{}",
        list.join("\n")
    )
}

pub fn skill_tool(skills: &[Skill]) -> Tool {
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    Tool {
        name: SKILL_TOOL.into(),
        description: "Load the full instructions of a skill by name.".into(),
        schema: json!({
            "type": "object",
            "properties": { "name": { "type": "string", "enum": names } },
            "required": ["name"],
        }),
    }
}

/// The answer to a `load_skill` call.
pub fn read_skill(skills: &[Skill], arguments: &str) -> Result<String, String> {
    let wanted = serde_json::from_str::<Value>(arguments).ok().and_then(|value| value["name"].as_str().map(str::to_string));
    let skill = wanted.as_ref().and_then(|name| skills.iter().find(|skill| &skill.name == name));
    match skill {
        Some(skill) => Ok(format!("{}\n\n(Files of this skill are in {})", skill.body, skill.folder.display())),
        None => Err(format!("没有叫 {} 的技能", wanted.unwrap_or_default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_file_is_valid_and_empty() {
        assert_eq!(parse_servers(DEFAULT_MCP).unwrap(), []);
    }

    #[test]
    fn servers_need_one_way_to_be_reached() {
        let both = "[[server]]\nname = \"a\"\ncommand = \"x\"\nurl = \"https://y\"\n";
        assert!(parse_servers(both).unwrap_err().contains("二选一"));
        assert!(parse_servers("[[server]]\nname = \"a\"\n").is_err());
        assert!(parse_servers("[[server]]\nname = \"a b\"\ncommand = \"x\"\n").is_err());
        let twice = "[[server]]\nname = \"a\"\ncommand = \"x\"\n[[server]]\nname = \"a\"\nurl = \"https://y\"\n";
        assert!(parse_servers(twice).unwrap_err().contains("两个"));
        let good = parse_servers("[[server]]\nname = \"fetch\"\ncommand = \"uvx\"\nargs = [\"mcp-server-fetch\"]\nconfirm = false\n").unwrap();
        assert!(good[0].enabled && !good[0].confirm && good[0].args == ["mcp-server-fetch"]);
    }

    #[test]
    fn replies_are_found_in_plain_and_streamed_bodies() {
        let plain = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}";
        assert_eq!(reply(plain, 2), Some(Ok(json!({ "tools": [] }))));
        assert_eq!(reply(plain, 3), None);
        let stream = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":5,\"error\":{\"code\":-1,\"message\":\"nope\"}}\n\n";
        assert_eq!(reply(stream, 5), Some(Err("nope".into())));
        let pretty = "{\n  \"jsonrpc\": \"2.0\",\n  \"id\": 1,\n  \"result\": {}\n}";
        assert_eq!(reply(pretty, 1), Some(Ok(json!({}))));
    }

    #[test]
    fn tools_get_names_the_model_can_use() {
        let listed = json!({ "tools": [
            { "name": "get.page", "description": "Fetch", "inputSchema": { "type": "object" } },
            { "name": "bare" },
        ] });
        let tools = tools_of("web", &listed);
        assert_eq!(tools[0].0.name, "web__get_page");
        assert_eq!(tools[0].1, "get.page");
        assert_eq!(tools[1].0.schema["type"], "object");
        assert_eq!(server_of("web__get_page"), Some("web"));
        assert_eq!(tool_name("s", &"x".repeat(80)).len(), 64);
        assert_eq!(call_tool(1, "get.page", "not json")["params"]["arguments"], json!({}));
    }

    #[test]
    fn results_become_text() {
        let good = json!({ "content": [{ "type": "text", "text": "a" }, { "type": "image" }, { "type": "text", "text": "b" }] });
        assert_eq!(result_text(&good), Ok("a\n[image]\nb".into()));
        let bad = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
        assert_eq!(result_text(&bad), Err("boom".into()));
    }

    #[test]
    fn skills_are_read_from_their_folders() {
        let dir = std::env::temp_dir().join(format!("myuri-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("skills/pdf")).unwrap();
        fs::create_dir_all(dir.join("skills/bare")).unwrap();
        fs::write(dir.join("skills/pdf/SKILL.md"), "---\nname: pdf-tools\ndescription: \"Work with PDFs\"\n---\n\n# How\nDo it.\n").unwrap();
        fs::write(dir.join("skills/bare/SKILL.md"), "Just text").unwrap();
        let skills = load_skills(&dir);
        assert_eq!(skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["bare", "pdf-tools"]);
        assert_eq!(skills[1].description, "Work with PDFs");
        assert_eq!(skills[1].body, "# How\nDo it.");
        assert_eq!(chosen_skills(&skills, &["pdf-tools".into()]).len(), 1);
        assert_eq!(chosen_skills(&skills, &["*".into()]).len(), 2);
        assert!(skills_prompt(&skills).contains("- pdf-tools: Work with PDFs"));
        assert!(skills_prompt(&[]).is_empty());
        assert!(read_skill(&skills, "{\"name\":\"pdf-tools\"}").unwrap().starts_with("# How"));
        assert!(read_skill(&skills, "{\"name\":\"x\"}").is_err());
        assert_eq!(skill_tool(&skills).schema["properties"]["name"]["enum"], json!(["bare", "pdf-tools"]));
        let _ = fs::remove_dir_all(&dir);
    }
}
