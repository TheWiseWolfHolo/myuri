//! Talking to model providers: building the request for each API family and
//! decoding its streamed reply. No networking happens here; the application
//! sends [`HttpRequest`] and feeds the response bytes to a [`Decoder`].

use serde_json::{Value, json};

use crate::catalog::ModelInfo;
use crate::config::{Api, Reasoning};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// For example `image/png`.
    pub mime: String,
    pub base64: String,
}

/// Something the model may ask the program to do for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// Letters, digits, `_` and `-` only: that is all providers accept.
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub schema: Value,
}

/// The model asking for a tool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as the text of a JSON object.
    pub arguments: String,
    /// What the provider wants handed back along with the call: reasoning
    /// items, signed thinking blocks, a thought signature.
    pub echo: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolResult {
    /// The id of the call this answers.
    pub id: String,
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Only meaningful on user turns.
    pub pictures: Vec<Picture>,
    /// Tools the assistant asked for in this turn.
    pub calls: Vec<ToolCall>,
    /// What the tools answered. A user turn carrying these follows the turn
    /// with the calls; its text and pictures are not sent.
    pub results: Vec<ToolResult>,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), pictures: Vec::new(), calls: Vec::new(), results: Vec::new() }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, text: text.into(), pictures: Vec::new(), calls: Vec::new(), results: Vec::new() }
    }
}

/// Arguments as a JSON object, whatever the model wrote.
fn arguments(text: &str) -> Value {
    serde_json::from_str::<Value>(text).ok().filter(Value::is_object).unwrap_or_else(|| json!({}))
}

pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub turns: &'a [Turn],
    pub reasoning: Option<Reasoning>,
    pub temperature: Option<f32>,
    /// What the catalog knows about the model, to pick the right way of
    /// asking for reasoning.
    pub info: Option<&'a ModelInfo>,
    /// Tools the model may call.
    pub tools: &'a [Tool],
}

#[derive(Debug, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    /// Includes the credential; never log these.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

fn effort(reasoning: Reasoning) -> &'static str {
    reasoning.as_str()
}

fn data_url(picture: &Picture) -> String {
    format!("data:{};base64,{}", picture.mime, picture.base64)
}

/// The streaming request for `chat` against a provider speaking `api`.
pub fn build(api: Api, base_url: &str, key: &str, chat: &ChatRequest) -> HttpRequest {
    let base = base_url.trim_end_matches('/');
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    let (path, mut body): (String, Value) = match api {
        Api::ChatCompletions => {
            headers.push(("authorization".into(), format!("Bearer {key}")));
            let mut messages = Vec::new();
            if !chat.system.is_empty() {
                messages.push(json!({ "role": "system", "content": chat.system }));
            }
            for turn in chat.turns {
                if !turn.results.is_empty() {
                    for result in &turn.results {
                        messages.push(json!({ "role": "tool", "tool_call_id": result.id, "content": result.text }));
                    }
                    continue;
                }
                let content = if turn.pictures.is_empty() {
                    json!(turn.text)
                } else {
                    let mut parts = Vec::new();
                    if !turn.text.is_empty() {
                        parts.push(json!({ "type": "text", "text": turn.text }));
                    }
                    for picture in &turn.pictures {
                        parts.push(json!({ "type": "image_url", "image_url": { "url": data_url(picture) } }));
                    }
                    json!(parts)
                };
                let role = if turn.role == Role::User { "user" } else { "assistant" };
                let mut message = json!({ "role": role, "content": content });
                if !turn.calls.is_empty() {
                    let calls: Vec<Value> = turn
                        .calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": call.arguments },
                            })
                        })
                        .collect();
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
            let mut body = json!({ "model": chat.model, "stream": true, "messages": messages });
            if !chat.tools.is_empty() {
                let tools: Vec<Value> = chat
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": { "name": tool.name, "description": tool.description, "parameters": tool.schema },
                        })
                    })
                    .collect();
                body["tools"] = json!(tools);
            }
            if let Some(reasoning) = chat.reasoning {
                body["reasoning_effort"] = json!(effort(reasoning));
            }
            ("/chat/completions".into(), body)
        }
        Api::Responses => {
            headers.push(("authorization".into(), format!("Bearer {key}")));
            let mut input: Vec<Value> = Vec::new();
            for turn in chat.turns {
                if !turn.results.is_empty() {
                    for result in &turn.results {
                        input.push(json!({ "type": "function_call_output", "call_id": result.id, "output": result.text }));
                    }
                    continue;
                }
                if turn.role == Role::Assistant {
                    if !turn.text.is_empty() || turn.calls.is_empty() {
                        input.push(json!({ "role": "assistant", "content": [{ "type": "output_text", "text": turn.text }] }));
                    }
                    for call in &turn.calls {
                        // What the provider sent goes back as it came, with
                        // the reasoning that led to the call.
                        let sent = call.echo.as_deref().and_then(|echo| serde_json::from_str::<Vec<Value>>(echo).ok());
                        match sent {
                            Some(items) => input.extend(items),
                            None => input.push(json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.name,
                                "arguments": call.arguments,
                            })),
                        }
                    }
                    continue;
                }
                let mut parts = Vec::new();
                if !turn.text.is_empty() {
                    parts.push(json!({ "type": "input_text", "text": turn.text }));
                }
                for picture in &turn.pictures {
                    parts.push(json!({ "type": "input_image", "image_url": data_url(picture) }));
                }
                input.push(json!({ "role": "user", "content": parts }));
            }
            let mut body = json!({ "model": chat.model, "stream": true, "input": input });
            if !chat.tools.is_empty() {
                let tools: Vec<Value> = chat
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.schema,
                        })
                    })
                    .collect();
                body["tools"] = json!(tools);
            }
            if !chat.system.is_empty() {
                body["instructions"] = json!(chat.system);
            }
            if let Some(reasoning) = chat.reasoning {
                body["reasoning"] = json!({ "effort": effort(reasoning) });
                // Without this the thinking is not shared at all.
                if reasoning != Reasoning::None {
                    body["reasoning"]["summary"] = json!("auto");
                }
            }
            ("/responses".into(), body)
        }
        Api::Anthropic => {
            headers.push(("x-api-key".into(), key.to_string()));
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            let messages: Vec<Value> = chat
                .turns
                .iter()
                .map(|turn| {
                    if !turn.results.is_empty() {
                        let results: Vec<Value> = turn
                            .results
                            .iter()
                            .map(|result| json!({ "type": "tool_result", "tool_use_id": result.id, "content": result.text }))
                            .collect();
                        return json!({ "role": "user", "content": results });
                    }
                    let mut parts = Vec::new();
                    // Signed thinking comes first, exactly as it was received.
                    for call in &turn.calls {
                        let sent = call.echo.as_deref().and_then(|echo| serde_json::from_str::<Vec<Value>>(echo).ok());
                        parts.extend(sent.into_iter().flatten());
                    }
                    for picture in &turn.pictures {
                        parts.push(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": picture.mime, "data": picture.base64 },
                        }));
                    }
                    if !turn.text.is_empty() || (parts.is_empty() && turn.calls.is_empty()) {
                        parts.push(json!({ "type": "text", "text": turn.text }));
                    }
                    for call in &turn.calls {
                        parts.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": arguments(&call.arguments),
                        }));
                    }
                    let role = if turn.role == Role::User { "user" } else { "assistant" };
                    json!({ "role": role, "content": parts })
                })
                .collect();
            let output = chat.info.and_then(|info| info.output).filter(|&limit| limit > 0);
            // The output cap includes thinking; reserve room for the visible reply.
            let thinking = match chat.reasoning {
                Some(Reasoning::None) | None => 0,
                Some(level) => chat.info.map_or_else(
                    || ModelInfo::default().budget_for(level),
                    |info| info.budget_for(level),
                ),
            };
            let thinking = output.map_or(thinking, |limit| {
                let reserve = (limit / 4).clamp(1, 4_096);
                thinking.min(limit.saturating_sub(reserve))
            });
            // Anthropic requires a thinking budget of at least 1024 tokens.
            let thinking = if thinking >= 1_024 { thinking } else { 0 };
            let max_tokens = 8_192u32.saturating_add(thinking).min(output.unwrap_or(u32::MAX));
            let mut body = json!({
                "model": chat.model,
                "stream": true,
                "max_tokens": max_tokens,
                "messages": messages,
            });
            if !chat.system.is_empty() {
                body["system"] = json!(chat.system);
            }
            if thinking > 0 {
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": thinking });
            }
            if !chat.tools.is_empty() {
                let tools: Vec<Value> = chat
                    .tools
                    .iter()
                    .map(|tool| json!({ "name": tool.name, "description": tool.description, "input_schema": tool.schema }))
                    .collect();
                body["tools"] = json!(tools);
            }
            ("/messages".into(), body)
        }
        Api::Gemini => {
            headers.push(("x-goog-api-key".into(), key.to_string()));
            let contents: Vec<Value> = chat
                .turns
                .iter()
                .map(|turn| {
                    if !turn.results.is_empty() {
                        let results: Vec<Value> = turn
                            .results
                            .iter()
                            .map(|result| {
                                json!({ "functionResponse": { "name": result.name, "response": { "result": result.text } } })
                            })
                            .collect();
                        return json!({ "role": "user", "parts": results });
                    }
                    let mut parts = Vec::new();
                    for picture in &turn.pictures {
                        parts.push(json!({
                            "inlineData": { "mimeType": picture.mime, "data": picture.base64 },
                        }));
                    }
                    if !turn.text.is_empty() || (parts.is_empty() && turn.calls.is_empty()) {
                        parts.push(json!({ "text": turn.text }));
                    }
                    for call in &turn.calls {
                        let mut part = json!({ "functionCall": { "name": call.name, "args": arguments(&call.arguments) } });
                        if let Some(signature) = &call.echo {
                            part["thoughtSignature"] = json!(signature);
                        }
                        parts.push(part);
                    }
                    let role = if turn.role == Role::User { "user" } else { "model" };
                    json!({ "role": role, "parts": parts })
                })
                .collect();
            let mut body = json!({ "contents": contents });
            if !chat.tools.is_empty() {
                let tools: Vec<Value> = chat
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({ "name": tool.name, "description": tool.description, "parametersJsonSchema": tool.schema })
                    })
                    .collect();
                body["tools"] = json!([{ "functionDeclarations": tools }]);
            }
            if !chat.system.is_empty() {
                body["systemInstruction"] = json!({ "parts": [{ "text": chat.system }] });
            }
            if let Some(reasoning) = chat.reasoning {
                let off = reasoning == Reasoning::None;
                // Older Gemini models take a token budget, newer ones a level.
                let budgeted = chat.info.filter(|info| info.efforts.is_empty() && info.budget.is_some());
                body["generationConfig"]["thinkingConfig"] = match budgeted {
                    Some(info) => json!({
                        "thinkingBudget": info.budget_for(reasoning),
                        "includeThoughts": !off,
                    }),
                    None => json!({
                        "thinkingLevel": if off { "minimal" } else { effort(reasoning) },
                        "includeThoughts": !off,
                    }),
                };
            }
            (format!("/models/{}:streamGenerateContent?alt=sse", chat.model), body)
        }
    };
    let accepts_temperature = !chat.info.is_some_and(|info| info.temperature == Some(false))
        && ["thinking", "reasoning_effort", "reasoning"].iter().all(|&field| body.get(field).is_none());
    if let Some(temperature) = chat.temperature.filter(|_| accepts_temperature) {
        // Widening 0.2f32 as it stands would send 0.20000000298023224.
        let rounded = json!((f64::from(temperature) * 1000.0).round() / 1000.0);
        if api == Api::Gemini {
            body["generationConfig"]["temperature"] = rounded;
        } else {
            body["temperature"] = rounded;
        }
    }
    HttpRequest { url: format!("{base}{path}"), headers, body }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Text(String),
    /// The model's thinking, where the provider shares it.
    Reasoning(String),
    /// The model asks for a tool. Complete when it is reported.
    Call(ToolCall),
    /// The reply is complete.
    Done,
    Error(String),
}

/// Turns a streamed response body into [`Event`]s. Bytes may arrive split
/// anywhere, including in the middle of a character.
pub struct Decoder {
    api: Api,
    buffer: Vec<u8>,
    data: String,
    finished: bool,
    /// Tool calls that arrive in pieces, by their position in the reply.
    calls: Vec<ToolCall>,
    /// What the provider wants back with the next call.
    echo: Vec<Value>,
    /// The Anthropic content block being received, if it is one to keep.
    block: Option<Value>,
}

impl Decoder {
    pub fn new(api: Api) -> Self {
        Self {
            api,
            buffer: Vec::new(),
            data: String::new(),
            finished: false,
            calls: Vec::new(),
            echo: Vec::new(),
            block: None,
        }
    }

    /// Report the calls assembled so far.
    fn flush_calls(&mut self, events: &mut Vec<Event>) {
        for call in self.calls.drain(..).filter(|call| !call.name.is_empty()) {
            events.push(Event::Call(call));
        }
    }

    fn take_echo(&mut self) -> Option<String> {
        (!self.echo.is_empty()).then(|| Value::Array(std::mem::take(&mut self.echo)).to_string())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            self.line(line.trim_end_matches(['\r', '\n']), &mut events);
        }
        events
    }

    /// The connection closed. Reports a reply that ended without saying so.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            self.line(rest.trim_end_matches(['\r', '\n']), &mut events);
        }
        self.dispatch(&mut events);
        if !self.finished {
            self.finished = true;
            self.flush_calls(&mut events);
            events.push(Event::Done);
        }
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<Event>) {
        if line.is_empty() {
            self.dispatch(events);
        } else if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
        } else if line.starts_with('{') && self.data.is_empty() {
            // Not an event stream at all: a plain JSON error body. No event
            // stream field starts with a brace.
            self.data.push_str(line);
        }
    }

    fn dispatch(&mut self, events: &mut Vec<Event>) {
        let data = std::mem::take(&mut self.data);
        if data.is_empty() || self.finished {
            return;
        }
        if data.trim() == "[DONE]" {
            self.finished = true;
            self.flush_calls(events);
            events.push(Event::Done);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else { return };
        if let Some(message) = error_message(&value) {
            self.finished = true;
            events.push(Event::Error(message));
            return;
        }
        let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_string);
        match self.api {
            Api::ChatCompletions => {
                let delta = &value["choices"][0]["delta"];
                if let Some(thought) = text(&delta["reasoning_content"]).or_else(|| text(&delta["reasoning"])) {
                    events.push(Event::Reasoning(thought));
                }
                if let Some(content) = text(&delta["content"]) {
                    events.push(Event::Text(content));
                }
                for piece in delta["tool_calls"].as_array().into_iter().flatten() {
                    let index = piece["index"].as_u64().unwrap_or(0) as usize;
                    if self.calls.len() <= index {
                        self.calls.resize_with(index + 1, ToolCall::default);
                    }
                    let call = &mut self.calls[index];
                    if let Some(id) = text(&piece["id"]) {
                        call.id = id;
                    }
                    if let Some(name) = text(&piece["function"]["name"]) {
                        call.name.push_str(&name);
                    }
                    if let Some(more) = piece["function"]["arguments"].as_str() {
                        call.arguments.push_str(more);
                    }
                }
                if !value["choices"][0]["finish_reason"].is_null() {
                    self.flush_calls(events);
                }
            }
            Api::Responses => match value["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => events.extend(text(&value["delta"]).map(Event::Text)),
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    events.extend(text(&value["delta"]).map(Event::Reasoning));
                }
                "response.output_item.done" => {
                    let item = &value["item"];
                    match item["type"].as_str().unwrap_or_default() {
                        "reasoning" => self.echo.push(item.clone()),
                        "function_call" => {
                            self.echo.push(item.clone());
                            events.push(Event::Call(ToolCall {
                                id: item["call_id"].as_str().unwrap_or_default().to_string(),
                                name: item["name"].as_str().unwrap_or_default().to_string(),
                                arguments: item["arguments"].as_str().unwrap_or("{}").to_string(),
                                echo: self.take_echo(),
                            }));
                        }
                        _ => {}
                    }
                }
                "response.completed" | "response.incomplete" => {
                    self.finished = true;
                    events.push(Event::Done);
                }
                _ => {}
            },
            Api::Anthropic => match value["type"].as_str().unwrap_or_default() {
                "content_block_start" => {
                    let block = &value["content_block"];
                    self.block = match block["type"].as_str().unwrap_or_default() {
                        "tool_use" => Some(json!({ "type": "tool_use", "id": block["id"], "name": block["name"], "json": "" })),
                        "thinking" => Some(json!({ "type": "thinking", "thinking": "", "signature": "" })),
                        // Thinking the provider hides still has to be handed back.
                        "redacted_thinking" => {
                            self.echo.push(block.clone());
                            None
                        }
                        _ => None,
                    };
                }
                "content_block_delta" => {
                    let delta = &value["delta"];
                    events.extend(text(&delta["text"]).map(Event::Text));
                    events.extend(text(&delta["thinking"]).map(Event::Reasoning));
                    if let Some(block) = &mut self.block {
                        for (from, to) in [("partial_json", "json"), ("thinking", "thinking"), ("signature", "signature")] {
                            if let (Some(more), Some(so_far)) = (delta[from].as_str(), block[to].as_str()) {
                                block[to] = json!(format!("{so_far}{more}"));
                            }
                        }
                    }
                }
                "content_block_stop" => match self.block.take() {
                    Some(block) if block["type"] == "tool_use" => {
                        let written = block["json"].as_str().unwrap_or_default();
                        events.push(Event::Call(ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            arguments: if written.is_empty() { "{}".into() } else { written.to_string() },
                            echo: self.take_echo(),
                        }));
                    }
                    Some(block) => self.echo.push(block),
                    None => {}
                },
                "message_stop" => {
                    self.finished = true;
                    events.push(Event::Done);
                }
                _ => {}
            },
            // Gemini has no closing event; the stream simply ends.
            Api::Gemini => {
                for part in value["candidates"][0]["content"]["parts"].as_array().into_iter().flatten() {
                    if let Some(piece) = text(&part["text"]) {
                        let thought = part["thought"] == true;
                        events.push(if thought { Event::Reasoning(piece) } else { Event::Text(piece) });
                    }
                    if part["functionCall"].is_object() {
                        // Gemini gives calls no id; number them.
                        self.calls.push(ToolCall::default());
                        events.push(Event::Call(ToolCall {
                            id: format!("call_{}", self.calls.len()),
                            name: part["functionCall"]["name"].as_str().unwrap_or_default().to_string(),
                            arguments: part["functionCall"]["args"].to_string(),
                            echo: part["thoughtSignature"].as_str().map(str::to_string),
                        }));
                    }
                }
            }
        }
    }
}

/// The request that lists a provider's models: its URL and headers.
pub fn models_request(api: Api, base_url: &str, key: &str) -> (String, Vec<(String, String)>) {
    let base = base_url.trim_end_matches('/');
    match api {
        Api::ChatCompletions | Api::Responses => {
            (format!("{base}/models"), vec![("authorization".into(), format!("Bearer {key}"))])
        }
        Api::Anthropic => (
            format!("{base}/models?limit=1000"),
            vec![("x-api-key".into(), key.into()), ("anthropic-version".into(), "2023-06-01".into())],
        ),
        Api::Gemini => {
            (format!("{base}/models?pageSize=1000"), vec![("x-goog-api-key".into(), key.into())])
        }
    }
}

/// The model names in the reply to [`models_request`], in the provider's
/// order. Relays do not always answer in the format of the API they imitate,
/// so every known shape is accepted.
pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let unreadable = || "供应商返回的不是模型列表".to_string();
    let value: Value = serde_json::from_str(body).map_err(|_| unreadable())?;
    if let Some(message) = error_message(&value) {
        return Err(message);
    }
    let list = [&value["data"], &value["models"], &value["publisherModels"], &value]
        .into_iter()
        .find_map(Value::as_array)
        .ok_or_else(unreadable)?;
    let mut names: Vec<String> = Vec::new();
    for entry in list {
        // Gemini lists embedding models too, marked by what they can be asked.
        // Keep chat (`generateContent`) and image (`predict` / `generateImages`) models.
        let methods = entry["supportedGenerationMethods"].as_array();
        if methods.is_some_and(|methods| {
            !methods.iter().any(|method| {
                matches!(method.as_str(), Some("generateContent" | "predict" | "generateImages"))
            })
        }) {
            continue;
        }
        let name = entry["id"].as_str().or(entry["name"].as_str()).or(entry.as_str()).unwrap_or("");
        let name = name.rsplit_once("/models/").map_or_else(|| name.trim_start_matches("models/"), |(_, tail)| tail);
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// The URL of the next page of models, if the reply indicates more exist.
pub fn next_models_page(api: Api, base_url: &str, body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let base = base_url.trim_end_matches('/');
    match api {
        Api::Anthropic => {
            if value["has_more"] != true {
                return None;
            }
            let after = value["last_id"].as_str().filter(|s| !s.is_empty())?;
            Some(format!("{base}/models?limit=1000&after_id={}", url_encode(after)))
        }
        Api::Gemini => {
            let token = value["nextPageToken"].as_str().filter(|s| !s.is_empty())?;
            Some(format!("{base}/models?pageSize=1000&pageToken={}", url_encode(token)))
        }
        Api::ChatCompletions | Api::Responses => None,
    }
}

pub(crate) fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Every key in a stored field: several are separated by commas.
pub fn split_keys(stored: &str) -> Vec<&str> {
    stored.split([',', '，']).map(str::trim).filter(|key| !key.is_empty()).collect()
}

/// Whether a refusal is about the key or the provider's state, so that
/// another key is worth trying.
pub fn another_key_may_work(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 429) || status >= 500
}

pub fn first_key(stored: &str) -> &str {
    stored.split([',', '，']).map(str::trim).find(|key| !key.is_empty()).unwrap_or("")
}

/// The message of an error object, in any of the shapes providers use.
pub fn error_message(value: &Value) -> Option<String> {
    let error = match value.get("error") {
        Some(error) if !error.is_null() => error,
        _ if value["type"] == "response.failed" => &value["response"]["error"],
        _ => return None,
    };
    let message = error["message"].as_str().or_else(|| error.as_str()).unwrap_or("请求失败");
    Some(message.to_string())
}

/// Whether a provider turned the request down because of the reasoning
/// setting, so that sending it again without one is worth a try.
pub fn rejects_reasoning(status: u16, body: &str) -> bool {
    let body = body.to_lowercase();
    status == 400 && (body.contains("thinking") || body.contains("reasoning"))
}

/// A readable reason for a failed HTTP response.
pub fn describe_failure(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| error_message(&value))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let hint = match status {
        401 | 403 => "密钥不对或没有权限",
        404 => "地址或模型名不对",
        429 => "请求太频繁或额度用完了",
        500..=599 => "供应商那边出错了",
        _ => "请求没有成功",
    };
    if detail.is_empty() { format!("{hint}（HTTP {status}）") } else { format!("{hint}（HTTP {status}）：{detail}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat<'a>(turns: &'a [Turn]) -> ChatRequest<'a> {
        ChatRequest { model: "m", system: "be brief", turns, reasoning: None, temperature: None, info: None, tools: &[] }
    }

    fn picture() -> Picture {
        Picture { mime: "image/png".into(), base64: "QUJD".into() }
    }

    #[test]
    fn chat_completions_request() {
        let turns = [Turn::user("hi"), Turn::assistant("hello"), Turn { pictures: vec![picture()], ..Turn::user("what is this") }];
        let mut request = chat(&turns);
        request.reasoning = Some(Reasoning::Low);
        request.temperature = Some(0.5);
        let http = build(Api::ChatCompletions, "https://x.test/v1/", "sk-1", &request);
        assert_eq!(http.url, "https://x.test/v1/chat/completions");
        assert!(http.headers.contains(&("authorization".into(), "Bearer sk-1".into())));
        assert_eq!(http.body["messages"][0], json!({ "role": "system", "content": "be brief" }));
        assert_eq!(http.body["messages"][1]["content"], "hi");
        assert_eq!(http.body["messages"][2]["role"], "assistant");
        assert_eq!(http.body["messages"][3]["content"][1]["image_url"]["url"], "data:image/png;base64,QUJD");
        assert_eq!(http.body["reasoning_effort"], "low");
        assert!(http.body.get("temperature").is_none());
        assert_eq!(http.body["stream"], true);
    }

    #[test]
    fn responses_request() {
        let turns = [Turn { pictures: vec![picture()], ..Turn::user("see") }, Turn::assistant("ok")];
        let mut request = chat(&turns);
        request.reasoning = Some(Reasoning::High);
        request.temperature = Some(0.6);
        let http = build(Api::Responses, "https://x.test/v1", "k", &request);
        assert_eq!(http.url, "https://x.test/v1/responses");
        assert_eq!(http.body["instructions"], "be brief");
        assert_eq!(http.body["input"][0]["content"][0], json!({ "type": "input_text", "text": "see" }));
        assert_eq!(http.body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(http.body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(http.body["reasoning"]["effort"], "high");
        assert!(http.body.get("temperature").is_none());
    }

    #[test]
    fn temperature_follows_catalog_support() {
        let turns = [Turn::user("hi")];
        let mut request = chat(&turns);
        request.temperature = Some(0.6);
        let http = build(Api::ChatCompletions, "https://x.test/v1", "k", &request);
        assert_eq!(http.body["temperature"], 0.6);
        let info = ModelInfo { temperature: Some(false), ..ModelInfo::default() };
        request.info = Some(&info);
        for api in [Api::ChatCompletions, Api::Responses, Api::Anthropic, Api::Gemini] {
            let http = build(api, "https://x.test/v1", "k", &request);
            assert!(http.body.get("temperature").is_none());
            assert!(http.body["generationConfig"].get("temperature").is_none());
        }
    }

    #[test]
    fn anthropic_request() {
        let turns = [Turn { pictures: vec![picture()], ..Turn::user("") }];
        let mut request = chat(&turns);
        request.temperature = Some(0.2);
        let http = build(Api::Anthropic, "https://api.anthropic.com/v1", "k", &request);
        assert_eq!(http.url, "https://api.anthropic.com/v1/messages");
        assert!(http.headers.contains(&("x-api-key".into(), "k".into())));
        assert_eq!(http.body["system"], "be brief");
        assert_eq!(http.body["max_tokens"], 8192);
        assert_eq!(http.body["messages"][0]["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(http.body["temperature"], 0.2);
        assert!(http.body.get("thinking").is_none());

        request.reasoning = Some(Reasoning::Medium);
        let http = build(Api::Anthropic, "https://api.anthropic.com/v1", "k", &request);
        assert_eq!(http.body["thinking"]["budget_tokens"], 8000);
        assert_eq!(http.body["max_tokens"], 16192);
        assert!(http.body.get("temperature").is_none());
    }

    #[test]
    fn anthropic_output_cap_includes_thinking() {
        let turns = [Turn::user("hi")];
        let mut request = chat(&turns);
        request.reasoning = Some(Reasoning::Medium);
        let info = ModelInfo { output: Some(4_096), ..ModelInfo::default() };
        request.info = Some(&info);
        let body = build(Api::Anthropic, "https://x.test/v1", "k", &request).body;
        assert_eq!(body["max_tokens"], 4_096);
        assert_eq!(body["thinking"]["budget_tokens"], 3_072);
    }

    #[test]
    fn gemini_request() {
        let turns = [Turn { pictures: vec![picture()], ..Turn::user("see") }, Turn::assistant("ok")];
        let mut request = chat(&turns);
        request.reasoning = Some(Reasoning::None);
        request.temperature = Some(0.4);
        let http = build(Api::Gemini, "https://g.test/v1beta", "k", &request);
        assert_eq!(http.url, "https://g.test/v1beta/models/m:streamGenerateContent?alt=sse");
        assert!(http.headers.contains(&("x-goog-api-key".into(), "k".into())));
        assert_eq!(http.body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(http.body["contents"][0]["parts"][0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(http.body["contents"][0]["parts"][1]["text"], "see");
        assert_eq!(http.body["contents"][1]["role"], "model");
        assert_eq!(http.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "minimal");
        assert_eq!(http.body["generationConfig"]["temperature"], 0.4);
        assert!(http.body.get("temperature").is_none());
    }

    #[test]
    fn gemini_budget_models_get_a_budget() {
        let turns = [Turn::user("hi")];
        let info = ModelInfo { reasoning: true, toggle: true, budget: Some((0, 24576)), ..ModelInfo::default() };
        let mut request = chat(&turns);
        request.info = Some(&info);
        request.reasoning = Some(Reasoning::High);
        let config = build(Api::Gemini, "u", "k", &request).body["generationConfig"]["thinkingConfig"].clone();
        assert_eq!(config, json!({ "thinkingBudget": 24000, "includeThoughts": true }));
        request.reasoning = Some(Reasoning::None);
        let config = build(Api::Gemini, "u", "k", &request).body["generationConfig"]["thinkingConfig"].clone();
        assert_eq!(config, json!({ "thinkingBudget": 0, "includeThoughts": false }));
    }

    #[test]
    fn gemini_stream() {
        let mut decoder = Decoder::new(Api::Gemini);
        let mut events = decoder.feed(
            b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hm\",\"thought\":true},{\"text\":\"Hi\"}]}}]}\n\n\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]},\"finishReason\":\"STOP\"}]}\n\n",
        );
        events.extend(decoder.finish());
        assert_eq!(
            events,
            [Event::Reasoning("hm".into()), Event::Text("Hi".into()), Event::Text(" there".into()), Event::Done]
        );
    }

    #[test]
    fn an_empty_system_prompt_is_left_out() {
        let turns = [Turn::user("hi")];
        let mut request = chat(&turns);
        request.system = "";
        assert_eq!(build(Api::ChatCompletions, "u", "k", &request).body["messages"][0]["role"], "user");
        assert!(build(Api::Responses, "u", "k", &request).body.get("instructions").is_none());
        assert!(build(Api::Anthropic, "u", "k", &request).body.get("system").is_none());
    }

    #[test]
    fn chat_completions_stream_split_anywhere() {
        let stream = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\r\n\r\n\
: keep-alive\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"，世界\"}}]}\n\n\
data: {\"choices\":[],\"usage\":{\"total_tokens\":9}}\n\n\
data: [DONE]\n\n";
        let expected = [
            Event::Reasoning("想".into()),
            Event::Text("你好".into()),
            Event::Text("，世界".into()),
            Event::Done,
        ];
        // Whole, byte by byte, and in uneven pieces: the result is the same.
        for size in [stream.len(), 1, 7] {
            let mut decoder = Decoder::new(Api::ChatCompletions);
            let mut events: Vec<Event> = stream.as_bytes().chunks(size).flat_map(|c| decoder.feed(c)).collect();
            events.extend(decoder.finish());
            assert_eq!(events, expected, "chunk size {size}");
        }
    }

    #[test]
    fn responses_stream() {
        let mut decoder = Decoder::new(Api::Responses);
        let events = decoder.feed(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"hm\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        assert_eq!(events, [Event::Text("Hi".into()), Event::Reasoning("hm".into()), Event::Done]);
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn anthropic_stream() {
        let mut decoder = Decoder::new(Api::Anthropic);
        let events = decoder.feed(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"t\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(events, [Event::Reasoning("t".into()), Event::Text("A".into()), Event::Done]);
    }

    #[test]
    fn errors_in_and_out_of_the_stream() {
        let mut decoder = Decoder::new(Api::ChatCompletions);
        let events = decoder.feed(b"data: {\"error\":{\"message\":\"quota\"}}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
        assert_eq!(events, [Event::Error("quota".into())]);

        // A provider that answers with bare JSON instead of an event stream.
        let mut decoder = Decoder::new(Api::ChatCompletions);
        let mut events = decoder.feed(b"{\"error\":\"bad model\"}");
        events.extend(decoder.finish());
        assert_eq!(events, [Event::Error("bad model".into())]);

        let mut decoder = Decoder::new(Api::Responses);
        let events = decoder.feed(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n");
        assert_eq!(events, [Event::Error("boom".into())]);
    }

    #[test]
    fn a_stream_that_just_ends_is_still_done() {
        let mut decoder = Decoder::new(Api::ChatCompletions);
        let mut events = decoder.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}");
        events.extend(decoder.finish());
        assert_eq!(events, [Event::Text("x".into()), Event::Done]);
    }

    #[test]
    fn reasoning_rejections_are_recognized() {
        assert!(rejects_reasoning(400, "{\"error\":{\"message\":\"Thinking level is unsupported\"}}"));
        assert!(rejects_reasoning(400, "unknown parameter: reasoning_effort"));
        assert!(!rejects_reasoning(400, "model not found"));
        assert!(!rejects_reasoning(500, "thinking failed"));
    }

    #[test]
    fn model_lists_are_read_in_every_shape() {
        let (url, headers) = models_request(Api::ChatCompletions, "https://x.test/v1/", "k");
        assert_eq!(url, "https://x.test/v1/models");
        assert_eq!(headers, [("authorization".to_string(), "Bearer k".to_string())]);
        let (url, _) = models_request(Api::Gemini, "https://g.test/v1beta", "k");
        assert_eq!(url, "https://g.test/v1beta/models?pageSize=1000");

        let names = |body: &str| parse_models(body).unwrap();
        assert_eq!(names(r#"{"data":[{"id":"a"},{"id":"b"},{"id":"a"},{"id":""}]}"#), ["a", "b"]);
        assert_eq!(
            names(
                r#"{"models":[{"name":"models/g-1","supportedGenerationMethods":["generateContent"]},
                {"name":"models/imagen-4","supportedGenerationMethods":["predict"]},
                {"name":"models/embed","supportedGenerationMethods":["embedContent"]}]}"#
            ),
            ["g-1", "imagen-4"]
        );
        assert_eq!(
            names(r#"{"publisherModels":[{"name":"publishers/google/models/gemini-4-neon"}]}"#),
            ["gemini-4-neon"]
        );
        assert_eq!(
            next_models_page(
                Api::Anthropic,
                "https://api.anthropic.com/v1/",
                r#"{"data":[{"id":"claude-1"}],"has_more":true,"last_id":"claude/1"}"#
            ),
            Some("https://api.anthropic.com/v1/models?limit=1000&after_id=claude%2F1".into())
        );
        assert_eq!(
            next_models_page(
                Api::Anthropic,
                "https://api.anthropic.com/v1",
                r#"{"data":[],"has_more":false,"last_id":"claude-1"}"#
            ),
            None
        );
        assert_eq!(
            next_models_page(
                Api::Gemini,
                "https://g.test/v1beta/",
                r#"{"models":[],"nextPageToken":"abc+1="}"#
            ),
            Some("https://g.test/v1beta/models?pageSize=1000&pageToken=abc%2B1%3D".into())
        );
        assert_eq!(names(r#"[{"id":"x"},"y"]"#), ["x", "y"]);
        assert_eq!(parse_models(r#"{"error":{"message":"bad key"}}"#), Err("bad key".into()));
        assert!(parse_models("<html>").is_err());
        assert!(parse_models(r#"{"ok":true}"#).is_err());
    }

    #[test]
    fn the_first_of_several_keys_is_used() {
        assert_eq!(first_key("sk-a"), "sk-a");
        assert_eq!(first_key(" sk-a , sk-b"), "sk-a");
        assert_eq!(first_key("，sk-b"), "sk-b");
        assert_eq!(first_key(""), "");
    }

    #[test]
    fn failures_are_described_for_people() {
        assert_eq!(
            describe_failure(401, "{\"error\":{\"message\":\"Invalid API key\"}}"),
            "密钥不对或没有权限（HTTP 401）：Invalid API key"
        );
        assert_eq!(describe_failure(502, ""), "供应商那边出错了（HTTP 502）");
        assert!(describe_failure(404, "<html>not found</html>").starts_with("地址或模型名不对（HTTP 404）："));
    }

    fn tool_turns() -> Vec<Turn> {
        let call = ToolCall { id: "c1".into(), name: "web__fetch".into(), arguments: "{\"url\":\"https://x.test\"}".into(), echo: None };
        let result = ToolResult { id: "c1".into(), name: "web__fetch".into(), text: "page".into() };
        vec![
            Turn::user("read it"),
            Turn { calls: vec![call], ..Turn::assistant("") },
            Turn { results: vec![result], ..Turn::user("") },
        ]
    }

    fn tool() -> Tool {
        Tool { name: "web__fetch".into(), description: "Fetch a page".into(), schema: json!({ "type": "object" }) }
    }

    #[test]
    fn tools_and_their_results_go_out_in_each_dialect() {
        let (turns, tools) = (tool_turns(), [tool()]);
        let request = ChatRequest { tools: &tools, system: "", ..chat(&turns) };

        let http = build(Api::ChatCompletions, "https://x.test", "k", &request);
        assert_eq!(http.body["tools"][0]["function"]["name"], "web__fetch");
        assert_eq!(http.body["messages"][1]["tool_calls"][0]["function"]["arguments"], "{\"url\":\"https://x.test\"}");
        assert_eq!(http.body["messages"][2], json!({ "role": "tool", "tool_call_id": "c1", "content": "page" }));

        let http = build(Api::Responses, "https://x.test", "k", &request);
        assert_eq!(http.body["tools"][0], json!({ "type": "function", "name": "web__fetch", "description": "Fetch a page", "parameters": { "type": "object" } }));
        assert_eq!(http.body["input"][1]["type"], "function_call");
        assert_eq!(http.body["input"][2], json!({ "type": "function_call_output", "call_id": "c1", "output": "page" }));

        let http = build(Api::Anthropic, "https://x.test", "k", &request);
        assert_eq!(http.body["tools"][0]["input_schema"], json!({ "type": "object" }));
        assert_eq!(http.body["messages"][1]["content"], json!([{ "type": "tool_use", "id": "c1", "name": "web__fetch", "input": { "url": "https://x.test" } }]));
        assert_eq!(http.body["messages"][2]["content"][0], json!({ "type": "tool_result", "tool_use_id": "c1", "content": "page" }));

        let http = build(Api::Gemini, "https://x.test", "k", &request);
        assert_eq!(http.body["tools"][0]["functionDeclarations"][0]["name"], "web__fetch");
        assert_eq!(http.body["contents"][1]["parts"][0]["functionCall"]["args"]["url"], "https://x.test");
        assert_eq!(http.body["contents"][2]["parts"][0]["functionResponse"]["response"]["result"], "page");
    }

    #[test]
    fn what_the_provider_wants_back_is_sent_back() {
        let mut turns = tool_turns();
        turns[1].calls[0].echo = Some("[{\"type\":\"reasoning\",\"id\":\"rs_1\"},{\"type\":\"function_call\",\"call_id\":\"c1\"}]".into());
        let http = build(Api::Responses, "https://x.test", "k", &chat(&turns));
        assert_eq!(http.body["input"][1], json!({ "type": "reasoning", "id": "rs_1" }));
        assert_eq!(http.body["input"][2]["call_id"], "c1");

        turns[1].calls[0].echo = Some("[{\"type\":\"thinking\",\"thinking\":\"hm\",\"signature\":\"sig\"}]".into());
        let http = build(Api::Anthropic, "https://x.test", "k", &chat(&turns));
        assert_eq!(http.body["messages"][1]["content"][0]["signature"], "sig");
        assert_eq!(http.body["messages"][1]["content"][1]["type"], "tool_use");

        turns[1].calls[0].echo = Some("sig".into());
        let http = build(Api::Gemini, "https://x.test", "k", &chat(&turns));
        assert_eq!(http.body["contents"][1]["parts"][0]["thoughtSignature"], "sig");
    }

    fn calls(api: Api, stream: &[u8]) -> Vec<Event> {
        let mut decoder = Decoder::new(api);
        let mut events = decoder.feed(stream);
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn tool_calls_are_assembled_from_the_stream() {
        let events = calls(
            Api::ChatCompletions,
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"web__fetch\",\"arguments\":\"{\\\"url\\\":\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        );
        let expected = ToolCall { id: "c1".into(), name: "web__fetch".into(), arguments: "{\"url\":\"a\"}".into(), echo: None };
        assert_eq!(events, [Event::Call(expected), Event::Done]);

        let events = calls(
            Api::Responses,
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n\
data: {\"type\":\"response.completed\"}\n\n",
        );
        let Event::Call(call) = &events[0] else { panic!("{events:?}") };
        assert_eq!((call.id.as_str(), call.name.as_str(), call.arguments.as_str()), ("c1", "f", "{}"));
        // Key order depends on how serde_json is built; compare as values.
        let echo: Value = serde_json::from_str(call.echo.as_deref().unwrap()).unwrap();
        assert_eq!(echo[0], json!({ "type": "reasoning", "id": "rs_1" }));
        assert_eq!(echo[1]["type"], "function_call");

        let events = calls(
            Api::Anthropic,
            b"data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"thinking\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n\
data: {\"type\":\"content_block_stop\"}\n\n\
data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n\
data: {\"type\":\"content_block_stop\"}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(events[0], Event::Reasoning("hm".into()));
        let Event::Call(call) = &events[1] else { panic!("{events:?}") };
        assert_eq!((call.id.as_str(), call.arguments.as_str()), ("t1", "{\"a\":1}"));
        let echo: Value = serde_json::from_str(call.echo.as_deref().unwrap()).unwrap();
        assert_eq!(echo, json!([{ "type": "thinking", "thinking": "hm", "signature": "sig" }]));

        let events = calls(
            Api::Gemini,
            b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"f\",\"args\":{\"a\":1}},\"thoughtSignature\":\"sig\"}]}}]}\n\n",
        );
        let expected = ToolCall { id: "call_1".into(), name: "f".into(), arguments: "{\"a\":1}".into(), echo: Some("sig".into()) };
        assert_eq!(events, [Event::Call(expected), Event::Done]);
    }
}
