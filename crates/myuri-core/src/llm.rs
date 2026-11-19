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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Only meaningful on user turns.
    pub pictures: Vec<Picture>,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), pictures: Vec::new() }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, text: text.into(), pictures: Vec::new() }
    }
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
                messages.push(json!({ "role": role, "content": content }));
            }
            let mut body = json!({ "model": chat.model, "stream": true, "messages": messages });
            if let Some(reasoning) = chat.reasoning {
                body["reasoning_effort"] = json!(effort(reasoning));
            }
            ("/chat/completions".into(), body)
        }
        Api::Responses => {
            headers.push(("authorization".into(), format!("Bearer {key}")));
            let input: Vec<Value> = chat
                .turns
                .iter()
                .map(|turn| {
                    if turn.role == Role::Assistant {
                        return json!({ "role": "assistant", "content": [{ "type": "output_text", "text": turn.text }] });
                    }
                    let mut parts = Vec::new();
                    if !turn.text.is_empty() {
                        parts.push(json!({ "type": "input_text", "text": turn.text }));
                    }
                    for picture in &turn.pictures {
                        parts.push(json!({ "type": "input_image", "image_url": data_url(picture) }));
                    }
                    json!({ "role": "user", "content": parts })
                })
                .collect();
            let mut body = json!({ "model": chat.model, "stream": true, "input": input });
            if !chat.system.is_empty() {
                body["instructions"] = json!(chat.system);
            }
            if let Some(reasoning) = chat.reasoning {
                body["reasoning"] = json!({ "effort": effort(reasoning) });
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
                    let mut parts = Vec::new();
                    for picture in &turn.pictures {
                        parts.push(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": picture.mime, "data": picture.base64 },
                        }));
                    }
                    if !turn.text.is_empty() || parts.is_empty() {
                        parts.push(json!({ "type": "text", "text": turn.text }));
                    }
                    let role = if turn.role == Role::User { "user" } else { "assistant" };
                    json!({ "role": role, "content": parts })
                })
                .collect();
            // The reply budget comes on top of whatever is spent thinking.
            let thinking = match chat.reasoning {
                Some(Reasoning::None) | None => 0,
                Some(level) => chat.info.cloned().unwrap_or_default().budget_for(level),
            };
            let mut body = json!({
                "model": chat.model,
                "stream": true,
                "max_tokens": 8_192 + thinking,
                "messages": messages,
            });
            if !chat.system.is_empty() {
                body["system"] = json!(chat.system);
            }
            if thinking > 0 {
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": thinking });
            }
            ("/messages".into(), body)
        }
        Api::Gemini => {
            headers.push(("x-goog-api-key".into(), key.to_string()));
            let contents: Vec<Value> = chat
                .turns
                .iter()
                .map(|turn| {
                    let mut parts = Vec::new();
                    for picture in &turn.pictures {
                        parts.push(json!({
                            "inlineData": { "mimeType": picture.mime, "data": picture.base64 },
                        }));
                    }
                    if !turn.text.is_empty() || parts.is_empty() {
                        parts.push(json!({ "text": turn.text }));
                    }
                    let role = if turn.role == Role::User { "user" } else { "model" };
                    json!({ "role": role, "parts": parts })
                })
                .collect();
            let mut body = json!({ "contents": contents });
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
    // Anthropic rejects a temperature while thinking is on.
    if let Some(temperature) = chat.temperature {
        if body.get("thinking").is_none() {
            // Widening 0.2f32 as it stands would send 0.20000000298023224.
            let rounded = json!((f64::from(temperature) * 1000.0).round() / 1000.0);
            if api == Api::Gemini {
                body["generationConfig"]["temperature"] = rounded;
            } else {
                body["temperature"] = rounded;
            }
        }
    }
    HttpRequest { url: format!("{base}{path}"), headers, body }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Text(String),
    /// The model's thinking, where the provider shares it.
    Reasoning(String),
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
}

impl Decoder {
    pub fn new(api: Api) -> Self {
        Self { api, buffer: Vec::new(), data: String::new(), finished: false }
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
            }
            Api::Responses => match value["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => events.extend(text(&value["delta"]).map(Event::Text)),
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    events.extend(text(&value["delta"]).map(Event::Reasoning));
                }
                "response.completed" | "response.incomplete" => {
                    self.finished = true;
                    events.push(Event::Done);
                }
                _ => {}
            },
            Api::Anthropic => match value["type"].as_str().unwrap_or_default() {
                "content_block_delta" => {
                    let delta = &value["delta"];
                    events.extend(text(&delta["text"]).map(Event::Text));
                    events.extend(text(&delta["thinking"]).map(Event::Reasoning));
                }
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
    let list = [&value["data"], &value["models"], &value]
        .into_iter()
        .find_map(Value::as_array)
        .ok_or_else(unreadable)?;
    let mut names: Vec<String> = Vec::new();
    for entry in list {
        // Gemini lists embedding models too, marked by what they can be asked.
        let methods = entry["supportedGenerationMethods"].as_array();
        if methods.is_some_and(|methods| !methods.iter().any(|method| method == "generateContent")) {
            continue;
        }
        let name = entry["id"].as_str().or(entry["name"].as_str()).or(entry.as_str()).unwrap_or("");
        let name = name.trim_start_matches("models/");
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
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
        ChatRequest { model: "m", system: "be brief", turns, reasoning: None, temperature: None, info: None }
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
        assert_eq!(http.body["temperature"], 0.5);
        assert_eq!(http.body["stream"], true);
    }

    #[test]
    fn responses_request() {
        let turns = [Turn { pictures: vec![picture()], ..Turn::user("see") }, Turn::assistant("ok")];
        let mut request = chat(&turns);
        request.reasoning = Some(Reasoning::High);
        let http = build(Api::Responses, "https://x.test/v1", "k", &request);
        assert_eq!(http.url, "https://x.test/v1/responses");
        assert_eq!(http.body["instructions"], "be brief");
        assert_eq!(http.body["input"][0]["content"][0], json!({ "type": "input_text", "text": "see" }));
        assert_eq!(http.body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(http.body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(http.body["reasoning"]["effort"], "high");
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
                {"name":"models/embed","supportedGenerationMethods":["embedContent"]}]}"#
            ),
            ["g-1"]
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
}
