//! Picture generation. Every service has its own request and reply; this
//! module turns one description of what is wanted into each of them, and
//! reads the replies back into pictures or a task to wait for.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::llm::HttpRequest;

/// Which service's picture API a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageApi {
    /// `POST /images/generations` as OpenAI defines it.
    Openai,
    /// xAI: the same path, with aspect ratio and resolution instead of size.
    Xai,
    /// Google's `POST /interactions`.
    Gemini,
    /// EvoLink: a task is created, then asked about until it is done.
    Evolink,
    /// WaveSpeed: one address per model, then a prediction to ask about.
    Wavespeed,
}

impl ImageApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Xai => "xai",
            Self::Gemini => "gemini",
            Self::Evolink => "evolink",
            Self::Wavespeed => "wavespeed",
        }
    }
}

pub struct ImageRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    /// An aspect ratio such as `16:9`, or pixels such as `1536x1024`.
    pub size: Option<&'a str>,
    /// `low`, `medium`, `high` and whatever else the model takes.
    pub quality: Option<&'a str>,
    /// `png`, `jpeg`, `webp`, and for models that can, `gif` or `svg`.
    pub format: Option<&'a str>,
}

/// A picture as a service hands it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picture {
    Base64(String),
    Url(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Done(Vec<Picture>),
    /// Not ready: ask again at this address.
    Wait { url: String, headers: Vec<(String, String)> },
}

fn bearer(key: &str) -> Vec<(String, String)> {
    vec![("authorization".into(), format!("Bearer {key}")), ("content-type".into(), "application/json".into())]
}

/// OpenAI takes pixels. Ratios become the nearest recommended size.
fn openai_size(size: &str) -> String {
    match size.split_once(':').and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))) {
        Some((w, h)) if w > h * 1.1 => "1536x1024".into(),
        Some((w, h)) if h > w * 1.1 => "1024x1536".into(),
        Some(_) => "1024x1024".into(),
        None => size.to_string(),
    }
}

pub fn build(api: ImageApi, base_url: &str, key: &str, request: &ImageRequest) -> HttpRequest {
    let base = base_url.trim_end_matches('/');
    let mut body = Map::new();
    let mut set = |name: &str, value: Option<&str>| {
        if let Some(value) = value.filter(|value| !value.is_empty() && *value != "auto") {
            body.insert(name.into(), json!(value));
        }
    };
    let (url, mut headers) = match api {
        ImageApi::Openai => {
            set("model", Some(request.model));
            set("prompt", Some(request.prompt));
            set("size", request.size.map(openai_size).as_deref());
            set("quality", request.quality);
            set("output_format", request.format);
            (format!("{base}/images/generations"), bearer(key))
        }
        ImageApi::Xai => {
            set("model", Some(request.model));
            set("prompt", Some(request.prompt));
            set("aspect_ratio", request.size.filter(|size| size.contains(':')));
            // xAI has no quality above medium.
            set("quality", request.quality.filter(|quality| matches!(*quality, "low" | "medium")));
            set("response_format", Some("b64_json"));
            (format!("{base}/images/generations"), bearer(key))
        }
        ImageApi::Evolink => {
            set("model", Some(request.model));
            set("prompt", Some(request.prompt));
            set("size", request.size);
            set("quality", request.quality);
            set("output_format", request.format);
            (format!("{base}/images/generations"), bearer(key))
        }
        ImageApi::Wavespeed => {
            set("prompt", Some(request.prompt));
            set("aspect_ratio", request.size.filter(|size| size.contains(':')));
            set("size", request.size.filter(|size| size.contains('x')).map(|s| s.replace('x', "*")).as_deref());
            set("output_format", request.format);
            (format!("{base}/{}", request.model), bearer(key))
        }
        ImageApi::Gemini => {
            let headers = vec![
                ("x-goog-api-key".to_string(), key.to_string()),
                ("content-type".to_string(), "application/json".to_string()),
                ("api-revision".to_string(), "2026-05-20".to_string()),
            ];
            let mut wanted = json!({ "type": "image" });
            if let Some(ratio) = request.size.filter(|size| size.contains(':')) {
                wanted["aspect_ratio"] = json!(ratio);
            }
            let body = json!({
                "model": request.model,
                "input": [{ "type": "text", "text": request.prompt }],
                "response_format": wanted,
            });
            return HttpRequest { url: format!("{base}/interactions"), headers, body };
        }
    };
    if api != ImageApi::Wavespeed && api != ImageApi::Xai {
        body.insert("n".into(), json!(1));
    }
    headers.dedup();
    HttpRequest { url, headers, body: Value::Object(body) }
}

/// Read a reply, either to the request or to asking about a task.
pub fn read(api: ImageApi, base_url: &str, key: &str, body: &str) -> Result<Step, String> {
    let base = base_url.trim_end_matches('/');
    let value: Value = serde_json::from_str(body).map_err(|_| "生图服务返回的不是 JSON".to_string())?;
    let failure = |value: &Value| {
        let error = &value["error"];
        error["message"].as_str().or(error.as_str()).or(value["message"].as_str()).map(String::from)
    };
    let pictures = match api {
        ImageApi::Openai | ImageApi::Xai => {
            if let Some(message) = failure(&value).filter(|_| value["data"].is_null()) {
                return Err(message);
            }
            entries(&value["data"])
        }
        ImageApi::Gemini => {
            if let Some(message) = failure(&value) {
                return Err(message);
            }
            let mut found = Vec::new();
            inline_pictures(&value, &mut found);
            found
        }
        ImageApi::Evolink => match value["status"].as_str() {
            Some("completed") => entries(&value["results"]),
            Some("failed") => return Err(failure(&value).unwrap_or_else(|| "任务失败了".into())),
            Some(_) => {
                let id = value["id"].as_str().ok_or("生图服务没有给任务号")?;
                return Ok(Step::Wait { url: format!("{base}/tasks/{id}"), headers: bearer(key) });
            }
            None => return Err(failure(&value).unwrap_or_else(|| "看不懂生图服务的回复".into())),
        },
        ImageApi::Wavespeed => {
            let data = if value["data"].is_object() { &value["data"] } else { &value };
            match data["status"].as_str() {
                Some("completed") => entries(&data["outputs"]),
                Some("failed") => {
                    let reason = data["error"].as_str().map(String::from).or_else(|| failure(&value));
                    return Err(reason.unwrap_or_else(|| "任务失败了".into()));
                }
                Some(_) => {
                    let id = data["id"].as_str().ok_or("生图服务没有给任务号")?;
                    let url = format!("{base}/predictions/{id}/result");
                    return Ok(Step::Wait { url, headers: bearer(key) });
                }
                None => return Err(failure(&value).unwrap_or_else(|| "看不懂生图服务的回复".into())),
            }
        }
    };
    if pictures.is_empty() {
        return Err(failure(&value).unwrap_or_else(|| "生图服务没有返回图片".into()));
    }
    Ok(Step::Done(pictures))
}

/// Pictures in a list whose items are links, or objects holding a link or
/// base64 data.
fn entries(list: &Value) -> Vec<Picture> {
    let one = |item: &Value| {
        let link = item.as_str().or(item["url"].as_str());
        match item["b64_json"].as_str() {
            Some(data) => Some(Picture::Base64(data.to_string())),
            None => link.map(|link| Picture::Url(link.to_string())),
        }
    };
    list.as_array().into_iter().flatten().filter_map(one).collect()
}

/// Every `{"type": "image", "data": ...}` anywhere in a Gemini reply.
fn inline_pictures(value: &Value, found: &mut Vec<Picture>) {
    match value {
        Value::Object(map) => {
            if let (Some("image"), Some(data)) = (map.get("type").and_then(Value::as_str), map.get("data").and_then(Value::as_str)) {
                found.push(Picture::Base64(data.to_string()));
            }
            map.values().for_each(|inner| inline_pictures(inner, found));
        }
        Value::Array(items) => items.iter().for_each(|inner| inline_pictures(inner, found)),
        _ => {}
    }
}

/// The file extension for picture bytes, told from how they begin.
pub fn extension(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xFF, 0xD8, ..] => "jpg",
        [b'G', b'I', b'F', ..] => "gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "webp",
        _ if bytes.trim_ascii_start().starts_with(b"<") => "svg",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>() -> ImageRequest<'a> {
        ImageRequest { model: "m", prompt: "a fox", size: Some("16:9"), quality: Some("high"), format: Some("webp") }
    }

    #[test]
    fn each_service_gets_its_own_request() {
        let openai = build(ImageApi::Openai, "https://o.test/v1/", "k", &request());
        assert_eq!(openai.url, "https://o.test/v1/images/generations");
        assert_eq!(
            openai.body,
            json!({ "model": "m", "prompt": "a fox", "size": "1536x1024", "quality": "high", "output_format": "webp", "n": 1 })
        );
        let xai = build(ImageApi::Xai, "https://x.test/v1", "k", &request());
        assert_eq!(
            xai.body,
            json!({ "model": "m", "prompt": "a fox", "aspect_ratio": "16:9", "response_format": "b64_json" })
        );
        let gemini = build(ImageApi::Gemini, "https://g.test/v1beta", "k", &request());
        assert_eq!(gemini.url, "https://g.test/v1beta/interactions");
        assert_eq!(gemini.body["response_format"], json!({ "type": "image", "aspect_ratio": "16:9" }));
        assert_eq!(gemini.body["input"][0]["text"], "a fox");
        let wave = build(ImageApi::Wavespeed, "https://w.test/api/v3", "k", &request());
        assert_eq!(wave.url, "https://w.test/api/v3/m");
        assert_eq!(wave.body, json!({ "prompt": "a fox", "aspect_ratio": "16:9", "output_format": "webp" }));
        let plain = ImageRequest { size: Some("auto"), quality: None, format: None, ..request() };
        assert_eq!(build(ImageApi::Evolink, "https://e.test/v1", "k", &plain).body, json!({ "model": "m", "prompt": "a fox", "n": 1 }));
        assert_eq!(openai_size("9:16"), "1024x1536");
        assert_eq!(openai_size("1:1"), "1024x1024");
        assert_eq!(openai_size("1536x864"), "1536x864");
    }

    #[test]
    fn replies_are_read() {
        let read = |api, body: &str| super::read(api, "https://s.test/v1", "k", body);
        assert_eq!(
            read(ImageApi::Openai, r#"{"data":[{"b64_json":"QUJD"},{"url":"https://p.test/a.png"}]}"#),
            Ok(Step::Done(vec![Picture::Base64("QUJD".into()), Picture::Url("https://p.test/a.png".into())]))
        );
        assert_eq!(read(ImageApi::Openai, r#"{"error":{"message":"blocked"}}"#), Err("blocked".into()));
        assert!(read(ImageApi::Openai, r#"{"data":[]}"#).is_err());
        assert_eq!(
            read(ImageApi::Gemini, r#"{"outputs":[{"type":"text","text":"hi"},{"content":[{"type":"image","data":"QQ=="}]}]}"#),
            Ok(Step::Done(vec![Picture::Base64("QQ==".into())]))
        );
        let waiting = read(ImageApi::Evolink, r#"{"id":"t1","status":"processing"}"#).unwrap();
        assert!(matches!(waiting, Step::Wait { ref url, .. } if url == "https://s.test/v1/tasks/t1"));
        assert_eq!(
            read(ImageApi::Evolink, r#"{"status":"completed","results":["https://p.test/b.png"]}"#),
            Ok(Step::Done(vec![Picture::Url("https://p.test/b.png".into())]))
        );
        assert_eq!(read(ImageApi::Evolink, r#"{"status":"failed","error":{"message":"no credit"}}"#), Err("no credit".into()));
        let waiting = read(ImageApi::Wavespeed, r#"{"code":200,"data":{"id":"p9","status":"created"}}"#).unwrap();
        assert!(matches!(waiting, Step::Wait { ref url, .. } if url == "https://s.test/v1/predictions/p9/result"));
        assert_eq!(
            read(ImageApi::Wavespeed, r#"{"data":{"status":"completed","outputs":["https://p.test/c.webp"]}}"#),
            Ok(Step::Done(vec![Picture::Url("https://p.test/c.webp".into())]))
        );
        assert!(read(ImageApi::Wavespeed, "<html>").is_err());
    }

    #[test]
    fn extensions_come_from_the_bytes() {
        assert_eq!(extension(&[0x89, b'P', b'N', b'G', 1]), "png");
        assert_eq!(extension(&[0xFF, 0xD8, 0xFF]), "jpg");
        assert_eq!(extension(b"GIF89a"), "gif");
        assert_eq!(extension(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(extension(b"  <svg xmlns"), "svg");
    }
}
