//! Picture generation. Every service has its own request and reply; this
//! module turns one description of what is wanted into each of them, and
//! reads the replies back into pictures or a task to wait for.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::llm::HttpRequest;

/// Which service's picture API a provider speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageApi {
    /// `POST /images/generations` as OpenAI defines it.
    #[default]
    Openai,
    /// xAI: the same path, with aspect ratio and resolution instead of size.
    Xai,
    /// Google's `generateContent` method.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl ReferenceImage {
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let mime = match extension(&bytes) {
            "png" => "image/png",
            "jpg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            _ => return None,
        };
        Some(Self {
            mime: mime.into(),
            bytes,
        })
    }

    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, STANDARD.encode(&self.bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSpec {
    pub sizes: &'static [(&'static str, &'static str)],
    pub default_size: &'static str,
    pub resolutions: &'static [&'static str],
    pub default_resolution: Option<&'static str>,
    pub qualities: &'static [(&'static str, &'static str)],
    pub default_quality: Option<&'static str>,
    pub formats: &'static [&'static str],
    pub default_format: Option<&'static str>,
    pub supports_edit: bool,
}

const RATIOS: &[(&str, &str)] = &[
    ("auto", "自动"),
    ("1:1", "1:1"),
    ("16:9", "16:9"),
    ("9:16", "9:16"),
    ("4:3", "4:3"),
    ("3:4", "3:4"),
    ("3:2", "3:2"),
    ("2:3", "2:3"),
    ("21:9", "21:9"),
];
const RESOLUTIONS: &[&str] = &["1K", "2K", "4K"];
const FORMATS: &[&str] = &["png", "webp", "jpeg"];
const QUALITIES: &[(&str, &str)] = &[
    ("low", "低"),
    ("medium", "中"),
    ("high", "高"),
    ("auto", "自动"),
];

impl ImageSpec {
    pub fn for_model(api: ImageApi, model: &str) -> Self {
        match api {
            ImageApi::Openai if is_dalle3(model) => Self {
                sizes: &[
                    ("1024x1024", "1:1 (1024)"),
                    ("1792x1024", "宽屏"),
                    ("1024x1792", "竖屏"),
                ],
                default_size: "1024x1024",
                resolutions: &["1K"],
                default_resolution: Some("1K"),
                qualities: &[("standard", "标准"), ("hd", "高清")],
                default_quality: Some("standard"),
                formats: &[],
                default_format: None,
                supports_edit: false,
            },
            ImageApi::Openai => Self {
                sizes: &[
                    ("1024x1024", "1:1"),
                    ("1536x1024", "3:2"),
                    ("1024x1536", "2:3"),
                    ("auto", "自动"),
                ],
                default_size: "1024x1024",
                resolutions: &["1K"],
                default_resolution: Some("1K"),
                qualities: QUALITIES,
                default_quality: Some("medium"),
                formats: FORMATS,
                default_format: Some("png"),
                supports_edit: true,
            },
            ImageApi::Gemini => Self {
                sizes: RATIOS,
                default_size: "auto",
                resolutions: RESOLUTIONS,
                default_resolution: Some("1K"),
                qualities: &[],
                default_quality: None,
                formats: &[],
                default_format: None,
                supports_edit: true,
            },
            ImageApi::Xai => Self {
                sizes: &RATIOS[..8],
                default_size: "1:1",
                resolutions: &[],
                default_resolution: None,
                qualities: &QUALITIES[..2],
                default_quality: Some("medium"),
                formats: &[],
                default_format: None,
                supports_edit: true,
            },
            ImageApi::Evolink => Self {
                sizes: &RATIOS[..8],
                default_size: "auto",
                resolutions: RESOLUTIONS,
                default_resolution: Some("1K"),
                qualities: &QUALITIES[..3],
                default_quality: Some("medium"),
                formats: FORMATS,
                default_format: Some("png"),
                supports_edit: true,
            },
            ImageApi::Wavespeed => Self {
                sizes: &RATIOS[..6],
                default_size: "1:1",
                resolutions: RESOLUTIONS,
                default_resolution: Some("1K"),
                qualities: &[],
                default_quality: None,
                formats: FORMATS,
                default_format: Some("png"),
                supports_edit: true,
            },
        }
    }
}

pub struct ImageRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    /// An aspect ratio such as `16:9`, or pixels such as `1536x1024`.
    pub size: Option<&'a str>,
    /// Model-specific resolution tier, such as `2K`.
    pub resolution: Option<&'a str>,
    /// `low`, `medium`, `high` and whatever else the model takes.
    pub quality: Option<&'a str>,
    /// `png`, `jpeg`, `webp`, and for models that can, `gif` or `svg`.
    pub format: Option<&'a str>,
    pub references: &'a [ReferenceImage],
}

pub enum ImageWire {
    Json(HttpRequest),
    Multipart {
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
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
    Wait {
        url: String,
        headers: Vec<(String, String)>,
    },
}

fn bearer(key: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {key}")),
        ("content-type".into(), "application/json".into()),
    ]
}

fn is_dalle3(model: &str) -> bool {
    model.to_ascii_lowercase().contains("dall-e-3")
}

fn gemini_supports_image_size(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    !lower.contains("gemini-2.5-flash-image") && !lower.contains("gemini-2.0-")
}

/// OpenAI takes pixels. Ratios become the nearest recommended size.
fn openai_size(size: &str, dalle3: bool) -> &str {
    match size
        .split_once(':')
        .and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))
    {
        Some((w, h)) if w > h * 1.1 => {
            if dalle3 {
                "1792x1024"
            } else {
                "1536x1024"
            }
        }
        Some((w, h)) if h > w * 1.1 => {
            if dalle3 {
                "1024x1792"
            } else {
                "1024x1536"
            }
        }
        Some(_) => "1024x1024",
        None => size,
    }
}

fn openai_fields<'a>(request: &ImageRequest<'a>) -> Vec<(&'static str, &'a str)> {
    let spec = ImageSpec::for_model(ImageApi::Openai, request.model);
    let dalle3 = !spec.supports_edit;
    let size = request
        .size
        .filter(|value| !value.is_empty() && (!dalle3 || *value != "auto"))
        .map(|value| openai_size(value, dalle3))
        .filter(|value| !dalle3 || spec.sizes.iter().any(|(allowed, _)| allowed == value))
        .unwrap_or(spec.default_size);
    let quality = request
        .quality
        .filter(|value| {
            !value.is_empty()
                && (!dalle3 || spec.qualities.iter().any(|(allowed, _)| allowed == value))
        })
        .or(spec.default_quality);
    let format = request
        .format
        .filter(|value| !spec.formats.is_empty() && !value.is_empty());
    let mut fields = vec![
        ("model", request.model),
        ("prompt", request.prompt),
        ("size", size),
    ];
    fields.extend(quality.map(|value| ("quality", value)));
    fields.extend(format.map(|value| ("output_format", value)));
    fields
}

pub(crate) struct Upload<'a> {
    pub field: &'a str,
    pub filename: &'a str,
    pub mime: &'a str,
    pub bytes: &'a [u8],
}

pub(crate) fn multipart(fields: &[(&str, &str)], files: &[Upload<'_>]) -> (Vec<u8>, String) {
    let boundary = format!("myuri-{:016x}{:016x}", OsRng.next_u64(), OsRng.next_u64());
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    for file in files {
        let Upload {
            field,
            filename,
            mime,
            bytes,
        } = file;
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, boundary)
}

pub fn build_wire(api: ImageApi, base_url: &str, key: &str, request: &ImageRequest) -> ImageWire {
    if api != ImageApi::Openai || request.references.is_empty() {
        return ImageWire::Json(build(api, base_url, key, request));
    }

    let field = if request.references.len() == 1 {
        "image"
    } else {
        "image[]"
    };
    let names: Vec<_> = request
        .references
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let extension = match extension(&image.bytes) {
                "" => "bin",
                extension => extension,
            };
            format!("reference-{}.{extension}", index + 1)
        })
        .collect();
    let files: Vec<_> = request
        .references
        .iter()
        .zip(&names)
        .map(|(image, name)| {
            let mime = if image.mime.starts_with("image/")
                && image.mime.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.')
                }) {
                &image.mime
            } else {
                "application/octet-stream"
            };
            Upload {
                field,
                filename: name,
                mime,
                bytes: &image.bytes,
            }
        })
        .collect();
    let (body, boundary) = multipart(&openai_fields(request), &files);
    let headers = vec![
        ("authorization".into(), format!("Bearer {key}")),
        (
            "content-type".into(),
            format!("multipart/form-data; boundary={boundary}"),
        ),
    ];
    let url = format!("{}/images/edits", base_url.trim_end_matches('/'));
    ImageWire::Multipart { url, headers, body }
}

/// Build the JSON request. Use `build_wire` when sending OpenAI edits.
pub fn build(api: ImageApi, base_url: &str, key: &str, request: &ImageRequest) -> HttpRequest {
    let base = base_url.trim_end_matches('/');
    let mut body = Map::new();
    fn set(body: &mut Map<String, Value>, name: &str, value: Option<&str>) {
        if let Some(value) = value.filter(|value| !value.is_empty() && *value != "auto") {
            body.insert(name.into(), json!(value));
        }
    }
    let (url, headers) = match api {
        ImageApi::Openai => {
            body.extend(
                openai_fields(request)
                    .into_iter()
                    .map(|(name, value)| (name.into(), json!(value))),
            );
            (format!("{base}/images/generations"), bearer(key))
        }
        ImageApi::Xai => {
            body.insert("model".into(), json!(request.model));
            body.insert("prompt".into(), json!(request.prompt));
            set(
                &mut body,
                "aspect_ratio",
                request.size.filter(|size| size.contains(':')),
            );
            // xAI has no quality above medium.
            set(
                &mut body,
                "quality",
                request
                    .quality
                    .filter(|quality| matches!(*quality, "low" | "medium")),
            );
            set(&mut body, "response_format", Some("b64_json"));
            let operation = match request.references {
                [] => "generations",
                [image] => {
                    body.insert(
                        "image".into(),
                        json!({ "url": image.data_url(), "type": "image_url" }),
                    );
                    "edits"
                }
                images => {
                    let entries: Vec<_> = images
                        .iter()
                        .map(|image| json!({ "url": image.data_url(), "type": "image_url" }))
                        .collect();
                    body.insert("images".into(), json!(entries));
                    "edits"
                }
            };
            (format!("{base}/images/{operation}"), bearer(key))
        }
        ImageApi::Evolink => {
            body.insert("model".into(), json!(request.model));
            body.insert("prompt".into(), json!(request.prompt));
            set(&mut body, "size", request.size);
            let model = request.model.to_ascii_lowercase();
            let resolution = request
                .resolution
                .filter(|value| !value.is_empty() && *value != "auto");
            let quality = if model.contains("gemini") || model.contains("nano-banana") {
                resolution.or(request.quality)
            } else {
                request.quality
            };
            set(&mut body, "quality", quality);
            set(&mut body, "output_format", request.format);
            if !request.references.is_empty() {
                body.insert("image_urls".into(), json!(data_urls(request.references)));
            }
            (format!("{base}/images/generations"), bearer(key))
        }
        ImageApi::Wavespeed => {
            body.insert("prompt".into(), json!(request.prompt));
            set(
                &mut body,
                "aspect_ratio",
                request.size.filter(|size| size.contains(':')),
            );
            set(
                &mut body,
                "size",
                request
                    .size
                    .filter(|size| size.contains('x'))
                    .map(|s| s.replace('x', "*"))
                    .as_deref(),
            );
            set(
                &mut body,
                "resolution",
                request.resolution.map(str::to_ascii_lowercase).as_deref(),
            );
            set(&mut body, "output_format", request.format);
            if !request.references.is_empty() {
                let urls = data_urls(request.references);
                body.insert("image".into(), json!(urls[0]));
                body.insert("images".into(), json!(urls));
            }
            (format!("{base}/{}", request.model), bearer(key))
        }
        ImageApi::Gemini => {
            let headers = vec![
                ("x-goog-api-key".to_string(), key.to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ];
            let mut image_config = Map::new();
            if gemini_supports_image_size(request.model) {
                let resolution = request
                    .resolution
                    .filter(|value| !value.is_empty() && *value != "auto")
                    .unwrap_or("1K");
                image_config.insert("imageSize".into(), json!(resolution));
            }
            if let Some(ratio) = request.size.filter(|size| size.contains(':')) {
                image_config.insert("aspectRatio".into(), json!(ratio));
            }
            let mut parts: Vec<_> = request.references.iter().map(|image| {
                json!({ "inlineData": { "mimeType": image.mime, "data": STANDARD.encode(&image.bytes) } })
            }).collect();
            parts.push(json!({ "text": request.prompt }));
            let mut generation_config = json!({ "responseModalities": ["IMAGE", "TEXT"] });
            if !image_config.is_empty() {
                generation_config["imageConfig"] = Value::Object(image_config);
            }
            let body = json!({
                "contents": [{ "role": "user", "parts": parts }],
                "generationConfig": generation_config,
            });
            let model = request
                .model
                .strip_prefix("models/")
                .unwrap_or(request.model);
            return HttpRequest {
                url: format!("{base}/models/{model}:generateContent"),
                headers,
                body,
            };
        }
    };
    if api != ImageApi::Wavespeed && api != ImageApi::Xai {
        body.insert("n".into(), json!(1));
    }
    HttpRequest {
        url,
        headers,
        body: Value::Object(body),
    }
}

fn data_urls(images: &[ReferenceImage]) -> Vec<String> {
    images.iter().map(ReferenceImage::data_url).collect()
}

/// Read a reply, either to the request or to asking about a task.
pub fn read(api: ImageApi, base_url: &str, key: &str, body: &str) -> Result<Step, String> {
    let base = base_url.trim_end_matches('/');
    let value: Value =
        serde_json::from_str(body).map_err(|_| "生图服务返回的不是 JSON".to_string())?;
    let failure = |value: &Value| {
        let error = &value["error"];
        error["message"]
            .as_str()
            .or(error.as_str())
            .or(value["message"].as_str())
            .map(String::from)
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
            Some("failed" | "cancelled" | "canceled" | "expired" | "error") => {
                return Err(failure(&value).unwrap_or_else(|| "任务失败了".into()));
            }
            Some(_) => {
                let id = value["id"].as_str().ok_or("生图服务没有给任务号")?;
                return Ok(Step::Wait {
                    url: format!("{base}/tasks/{id}"),
                    headers: bearer(key),
                });
            }
            None => return Err(failure(&value).unwrap_or_else(|| "看不懂生图服务的回复".into())),
        },
        ImageApi::Wavespeed => {
            let data = if value["data"].is_object() {
                &value["data"]
            } else {
                &value
            };
            match data["status"].as_str() {
                Some("completed") => entries(&data["outputs"]),
                Some("failed" | "cancelled" | "canceled" | "expired" | "error") => {
                    let reason = failure(data).or_else(|| failure(&value));
                    return Err(reason.unwrap_or_else(|| "任务失败了".into()));
                }
                Some(_) => {
                    let id = data["id"].as_str().ok_or("生图服务没有给任务号")?;
                    let url = format!("{base}/predictions/{id}/result");
                    return Ok(Step::Wait {
                        url,
                        headers: bearer(key),
                    });
                }
                None => {
                    return Err(failure(&value).unwrap_or_else(|| "看不懂生图服务的回复".into()));
                }
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
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(one)
        .collect()
}

/// Read the inline image parts from generateContent and older Interactions replies.
fn inline_pictures(value: &Value, found: &mut Vec<Picture>) {
    match value {
        Value::Object(map) => {
            if map.get("thought") == Some(&Value::Bool(true)) {
                return;
            }
            if let Some(inline) = map.get("inlineData").or_else(|| map.get("inline_data")) {
                let mime = inline
                    .get("mimeType")
                    .or_else(|| inline.get("mime_type"))
                    .and_then(Value::as_str);
                let is_image = mime.is_none_or(|mime| mime.to_ascii_lowercase().starts_with("image/"));
                if is_image && let Some(data) = inline["data"].as_str().filter(|data| !data.is_empty()) {
                    found.push(Picture::Base64(data.to_string()));
                }
            }
            if let (Some("image"), Some(data)) = (
                map.get("type").and_then(Value::as_str),
                map.get("data").and_then(Value::as_str).filter(|data| !data.is_empty()),
            ) {
                found.push(Picture::Base64(data.to_string()));
            }
            map.values().for_each(|inner| inline_pictures(inner, found));
        }
        Value::Array(items) => items.iter().for_each(|inner| inline_pictures(inner, found)),
        _ => {}
    }
}

/// The file extension for recognizable picture bytes, or empty if unknown.
pub fn extension(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xFF, 0xD8, ..] => "jpg",
        [b'G', b'I', b'F', ..] => "gif",
        [b'B', b'M', ..] => "bmp",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "webp",
        _ if svg_root(bytes) => "svg",
        _ => "",
    }
}

fn svg_root(bytes: &[u8]) -> bool {
    let mut start = bytes
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(bytes)
        .trim_ascii_start();
    if start.starts_with(b"<?xml") {
        let Some(index) = start.windows(2).position(|part| part == b"?>") else {
            return false;
        };
        start = start[index + 2..].trim_ascii_start();
    }
    start
        .strip_prefix(b"<svg")
        .and_then(|rest| rest.first())
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>() -> ImageRequest<'a> {
        ImageRequest {
            model: "m",
            prompt: "a fox",
            size: Some("16:9"),
            resolution: None,
            quality: Some("high"),
            format: Some("webp"),
            references: &[],
        }
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
        assert_eq!(gemini.url, "https://g.test/v1beta/models/m:generateContent");
        assert_eq!(
            gemini.body["generationConfig"]["imageConfig"],
            json!({ "imageSize": "1K", "aspectRatio": "16:9" })
        );
        assert_eq!(gemini.body["contents"][0]["parts"][0]["text"], "a fox");
        let wave = build(
            ImageApi::Wavespeed,
            "https://w.test/api/v3",
            "k",
            &request(),
        );
        assert_eq!(wave.url, "https://w.test/api/v3/m");
        assert_eq!(
            wave.body,
            json!({ "prompt": "a fox", "aspect_ratio": "16:9", "output_format": "webp" })
        );
        let plain = ImageRequest {
            size: Some("auto"),
            quality: None,
            format: None,
            ..request()
        };
        assert_eq!(
            build(ImageApi::Evolink, "https://e.test/v1", "k", &plain).body,
            json!({ "model": "m", "prompt": "a fox", "n": 1 })
        );
        assert_eq!(openai_size("9:16", false), "1024x1536");
        assert_eq!(openai_size("1:1", false), "1024x1024");
        assert_eq!(openai_size("1536x864", false), "1536x864");
    }

    #[test]
    fn openai_defaults_follow_the_model() {
        let request = ImageRequest {
            size: None,
            quality: None,
            format: Some("webp"),
            ..request()
        };
        let current = build(ImageApi::Openai, "https://o.test/v1", "k", &request);
        assert_eq!(current.body["size"], "1024x1024");
        assert_eq!(current.body["quality"], "medium");
        assert_eq!(current.body["output_format"], "webp");

        let legacy = ImageRequest {
            model: "prefix/DALL-E-3",
            size: Some("16:9"),
            ..request
        };
        assert!(!ImageSpec::for_model(ImageApi::Openai, legacy.model).supports_edit);
        let legacy = build(ImageApi::Openai, "https://o.test/v1", "k", &legacy);
        assert_eq!(legacy.body["size"], "1792x1024");
        assert_eq!(legacy.body["quality"], "standard");
        assert!(legacy.body.get("output_format").is_none());
    }

    #[test]
    fn openai_edits_upload_reference_files() {
        let png = ReferenceImage::from_bytes(vec![0x89, b'P', b'N', b'G', 0]).unwrap();
        let jpg = ReferenceImage::from_bytes(vec![0xFF, 0xD8, 0xFF]).unwrap();
        let expected = [
            ("image", "reference-1.png", "image/png"),
            ("image[]", "reference-2.jpg", "image/jpeg"),
        ];
        let images = [png, jpg];
        for (index, (field, filename, mime)) in expected.into_iter().enumerate() {
            let request = ImageRequest {
                references: &images[..=index],
                quality: None,
                ..request()
            };
            let ImageWire::Multipart { url, headers, body } =
                build_wire(ImageApi::Openai, "https://o.test/v1/", "k", &request)
            else {
                panic!("expected multipart")
            };
            assert_eq!(url, "https://o.test/v1/images/edits");
            let boundary = headers[1]
                .1
                .strip_prefix("multipart/form-data; boundary=")
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(text.starts_with(&format!("--{boundary}\r\n")));
            assert!(text.ends_with(&format!("--{boundary}--\r\n")));
            assert!(text.contains("name=\"quality\"\r\n\r\nmedium\r\n"));
            assert!(text.contains(&format!(
                "name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n"
            )));
            assert!(
                body.windows(images[index].bytes.len())
                    .any(|part| part == images[index].bytes)
            );
        }
    }

    #[test]
    fn edits_carry_the_reference_and_options_in_each_json_dialect() {
        let references = [ReferenceImage {
            mime: "image/png".into(),
            bytes: vec![1, 2, 3],
        }];
        let request = ImageRequest {
            model: "nano-banana",
            resolution: Some("4K"),
            size: Some("auto"),
            references: &references,
            ..request()
        };
        let gemini = build(ImageApi::Gemini, "https://g.test/v1beta/", "k", &request);
        assert_eq!(
            gemini.body["generationConfig"]["imageConfig"],
            json!({ "imageSize": "4K" })
        );
        assert_eq!(
            gemini.body["contents"][0]["parts"],
            json!([
                { "inlineData": { "mimeType": "image/png", "data": "AQID" } }, { "text": "a fox" }
            ])
        );
        let xai = build(ImageApi::Xai, "https://x.test/v1", "k", &request);
        let url = "data:image/png;base64,AQID";
        assert_eq!(xai.url, "https://x.test/v1/images/edits");
        assert_eq!(xai.body["image"]["url"], url);
        let evo = build(ImageApi::Evolink, "https://e.test/v1", "k", &request);
        assert_eq!(evo.body["image_urls"], json!([url]));
        assert_eq!(evo.body["quality"], "4K");
        let wave = build(ImageApi::Wavespeed, "https://w.test/v1", "k", &request);
        assert_eq!(wave.body["images"], json!([url]));
        assert_eq!(wave.body["image"], url);
        assert_eq!(wave.body["resolution"], "4k");
    }

    #[test]
    fn replies_are_read() {
        let read = |api, body: &str| super::read(api, "https://s.test/v1", "k", body);
        assert_eq!(
            read(
                ImageApi::Openai,
                r#"{"data":[{"b64_json":"QUJD"},{"url":"https://p.test/a.png"}]}"#
            ),
            Ok(Step::Done(vec![
                Picture::Base64("QUJD".into()),
                Picture::Url("https://p.test/a.png".into())
            ]))
        );
        assert_eq!(
            read(ImageApi::Openai, r#"{"error":{"message":"blocked"}}"#),
            Err("blocked".into())
        );
        assert!(read(ImageApi::Openai, r#"{"data":[]}"#).is_err());
        assert_eq!(
            read(
                ImageApi::Gemini,
                r#"{"outputs":[{"type":"text","text":"hi"},{"content":[{"type":"image","data":"QQ=="}]}]}"#
            ),
            Ok(Step::Done(vec![Picture::Base64("QQ==".into())]))
        );
        assert_eq!(
            read(
                ImageApi::Gemini,
                r#"{"candidates":[{"content":{"parts":[{"text":"hi"},{"inlineData":{"mimeType":"image/png","data":"QUJD"}}]}}]}"#
            ),
            Ok(Step::Done(vec![Picture::Base64("QUJD".into())]))
        );
        let waiting = read(ImageApi::Evolink, r#"{"id":"t1","status":"processing"}"#).unwrap();
        assert!(
            matches!(waiting, Step::Wait { ref url, .. } if url == "https://s.test/v1/tasks/t1")
        );
        assert_eq!(
            read(
                ImageApi::Evolink,
                r#"{"status":"completed","results":["https://p.test/b.png"]}"#
            ),
            Ok(Step::Done(vec![Picture::Url(
                "https://p.test/b.png".into()
            )]))
        );
        assert_eq!(
            read(
                ImageApi::Evolink,
                r#"{"status":"failed","error":{"message":"no credit"}}"#
            ),
            Err("no credit".into())
        );
        let waiting = read(
            ImageApi::Wavespeed,
            r#"{"code":200,"data":{"id":"p9","status":"created"}}"#,
        )
        .unwrap();
        assert!(
            matches!(waiting, Step::Wait { ref url, .. } if url == "https://s.test/v1/predictions/p9/result")
        );
        assert_eq!(
            read(
                ImageApi::Wavespeed,
                r#"{"data":{"status":"completed","outputs":["https://p.test/c.webp"]}}"#
            ),
            Ok(Step::Done(vec![Picture::Url(
                "https://p.test/c.webp".into()
            )]))
        );
        assert!(read(ImageApi::Wavespeed, "<html>").is_err());
        for status in ["cancelled", "canceled", "expired", "error"] {
            let result = json!({ "status": status, "id": "task", "error": "terminated" });
            assert_eq!(
                read(ImageApi::Evolink, &result.to_string()),
                Err("terminated".into())
            );
            assert_eq!(
                read(ImageApi::Wavespeed, &json!({ "data": result }).to_string()),
                Err("terminated".into())
            );
        }
    }

    #[test]
    fn extensions_come_from_the_bytes() {
        assert_eq!(extension(&[0x89, b'P', b'N', b'G', 1]), "png");
        assert_eq!(extension(&[0xFF, 0xD8, 0xFF]), "jpg");
        assert_eq!(extension(b"GIF89a"), "gif");
        assert_eq!(extension(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(extension(b"  <svg xmlns"), "svg");
        assert_eq!(extension(b"<?xml version=\"1.0\"?>\n<svg/>"), "svg");
        assert_eq!(extension(b"<html><svg/></html>"), "");
        assert!(ReferenceImage::from_bytes(b"<html>Bad Gateway</html>".to_vec()).is_none());
    }

    #[test]
    fn edge_cases_for_gemini_xai_dalle3_and_auto_prompt() {
        let refs = [
            ReferenceImage { mime: "image/png".into(), bytes: vec![1, 2, 3] },
            ReferenceImage { mime: "image/jpeg".into(), bytes: vec![4, 5, 6] },
        ];
        // xAI multi-reference uses `images` with `type: image_url`.
        let xai_multi = build(ImageApi::Xai, "https://x.test/v1", "k", &ImageRequest {
            prompt: "auto",
            references: &refs,
            ..request()
        });
        assert_eq!(xai_multi.url, "https://x.test/v1/images/edits");
        assert_eq!(xai_multi.body["prompt"], "auto");
        assert_eq!(xai_multi.body["images"][0]["type"], "image_url");
        assert_eq!(xai_multi.body["images"][1]["url"], "data:image/jpeg;base64,BAUG");

        // Gemini 2.5 Flash Image omits imageSize; Gemini thought drafts are skipped.
        let g25 = build(ImageApi::Gemini, "https://g.test/v1beta", "k", &ImageRequest {
            model: "gemini-2.5-flash-image",
            size: Some("16:9"),
            resolution: Some("2K"),
            ..request()
        });
        assert!(g25.body["generationConfig"]["imageConfig"].get("imageSize").is_none());
        assert_eq!(g25.body["generationConfig"]["imageConfig"]["aspectRatio"], "16:9");

        let reply = r#"{"candidates":[{"content":{"parts":[
            {"thought":true,"inlineData":{"mimeType":"image/png","data":"RFJBRlQ="}},
            {"inlineData":{"mimeType":"audio/wav","data":"QVVESU8="}},
            {"inlineData":{"mimeType":"image/png","data":""}},
            {"inlineData":{"mimeType":"image/png","data":"RklOQUw="}}
        ]}}]}"#;
        assert_eq!(
            super::read(ImageApi::Gemini, "https://g.test/v1beta", "k", reply),
            Ok(Step::Done(vec![Picture::Base64("RklOQUw=".into())]))
        );

        // DALL-E 3 coerces invalid size/quality back to valid defaults.
        let dalle = build(ImageApi::Openai, "https://o.test/v1", "k", &ImageRequest {
            model: "dall-e-3",
            size: Some("auto"),
            quality: Some("high"),
            ..request()
        });
        assert_eq!(dalle.body["size"], "1024x1024");
        assert_eq!(dalle.body["quality"], "standard");
    }
}
