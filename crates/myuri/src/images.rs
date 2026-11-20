//! Making pictures: sends the request, waits for services that work through
//! tasks, and fetches the results.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use myuri_core::config::ImageProvider;
use myuri_core::image::{self, ImageRequest, ImageSpec, ImageWire, Picture, Step};
use myuri_core::llm;

use crate::{net::Abort, secrets, vertex};

/// How long a task may take before giving up.
const PATIENCE: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Default)]
pub struct ImageOptions {
    pub size: Option<&'static str>,
    pub resolution: Option<&'static str>,
    pub quality: Option<&'static str>,
    pub format: Option<&'static str>,
}

impl ImageOptions {
    pub fn new(provider: &ImageProvider, spec: ImageSpec) -> Self {
        fn pick(
            chosen: &str,
            mut choices: impl Iterator<Item = &'static str>,
            default: Option<&'static str>,
        ) -> Option<&'static str> {
            choices.find(|value| *value == chosen).or(default)
        }
        Self {
            size: pick(
                &provider.default_size,
                spec.sizes.iter().map(|&(value, _)| value),
                Some(spec.default_size),
            ),
            resolution: pick(
                &provider.default_resolution,
                spec.resolutions.iter().copied(),
                spec.default_resolution,
            ),
            quality: pick(
                &provider.default_quality,
                spec.qualities.iter().map(|&(value, _)| value),
                spec.default_quality,
            ),
            format: pick(
                &provider.default_format,
                spec.formats.iter().copied(),
                spec.default_format,
            ),
        }
    }
}

/// Generate pictures. Blocks; call it off the UI thread. Returns the bytes
/// of every picture. With several keys, the next one is tried when a key is
/// refused or the service cannot be reached.
pub fn generate(
    provider: &ImageProvider,
    request: &ImageRequest,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Vec<u8>>, String> {
    generate_with_abort(provider, request, &Abort::with_flag(cancel))
}

pub fn generate_with_abort(
    provider: &ImageProvider,
    request: &ImageRequest,
    abort: &Abort,
) -> Result<Vec<Vec<u8>>, String> {
    let spec = ImageSpec::for_model(provider.api, request.model);
    if !request.references.is_empty() && !spec.supports_edit {
        return Err("这个模型不支持参考图".into());
    }
    let options = ImageOptions::new(provider, spec);
    let request = ImageRequest {
        size: request.size.or(options.size),
        resolution: request.resolution.or(options.resolution),
        quality: request.quality.or(options.quality),
        format: request.format.or(options.format),
        ..*request
    };
    let stored = secrets::read_in(secrets::NS_IMAGE, &provider.id).ok_or_else(|| {
        format!(
            "生图渠道 \"{0}\" 还没有密钥，用 myuri key --image {0} 设置",
            provider.id
        )
    })?;
    let mut failure = "密钥都停用了".to_string();
    static TURN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let turn = TURN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for key in provider.keys_to_try(&stored, turn) {
        if abort.is_stopped() {
            return Err("请求已停止".into());
        }
        match with_key(provider, key, &request, abort) {
            Ok(pictures) => return Ok(pictures),
            Err((reason, try_next)) => {
                failure = reason;
                if !try_next || abort.is_stopped() {
                    break;
                }
            }
        }
    }
    Err(failure)
}

/// One attempt. The flag in the error says whether another key may do better.
fn with_key(
    provider: &ImageProvider,
    key: &str,
    request: &ImageRequest,
    abort: &Abort,
) -> Result<Vec<Vec<u8>>, (String, bool)> {
    let api = provider.api;
    let fatal = |reason: String| (reason, false);
    let token = if provider.vertex {
        Some(vertex::token(&provider.id, key).map_err(fatal)?)
    } else {
        None
    };
    let api_key = if token.is_some() { "" } else { key };
    let (url, mut headers, body) =
        match image::build_wire(api, &provider.base_url, api_key, request) {
            ImageWire::Json(sent) => (sent.url, sent.headers, sent.body.to_string().into_bytes()),
            ImageWire::Multipart { url, headers, body } => (url, headers, body),
        };
    merge_headers(provider, &mut headers);
    if let Some(token) = token {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
        vertex::authorize(&mut headers, &token);
    }
    let (status, body) = abort
        .post(&url, &headers, &body)
        .map_err(|reason| (reason, true))?;
    let mut reply = String::from_utf8_lossy(&body).into_owned();
    if !(200..300).contains(&status) {
        return Err((
            llm::describe_failure(status, &reply),
            llm::another_key_may_work(status),
        ));
    }
    let started = Instant::now();
    let pictures = loop {
        if abort.is_stopped() {
            return Err(fatal("请求已停止".into()));
        }
        match image::read(api, &provider.base_url, api_key, &reply).map_err(fatal)? {
            Step::Done(pictures) => break pictures,
            Step::Wait { url, mut headers } => {
                if started.elapsed() > PATIENCE {
                    return Err(fatal("等了十分钟还没画完".into()));
                }
                let poll_at = Instant::now() + Duration::from_secs(2);
                while Instant::now() < poll_at {
                    if abort.is_stopped() {
                        return Err(fatal("请求已停止".into()));
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
                merge_headers(provider, &mut headers);
                reply = String::from_utf8_lossy(&abort.get(&url, &headers).map_err(fatal)?)
                    .into_owned();
            }
        }
    };
    pictures
        .into_iter()
        .map(|picture| {
            if abort.is_stopped() {
                return Err("请求已停止".into());
            }
            let bytes = match picture {
                Picture::Url(url) => abort.get(&url, &[])?,
                Picture::Base64(data) => base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .map_err(|_| "图片数据解不开".to_string())?,
            };
            if !valid_image(&bytes) {
                return Err("生图服务返回的数据不是有效图片".into());
            }
            if abort.is_stopped() {
                return Err("请求已停止".into());
            }
            Ok(bytes)
        })
        .collect::<Result<_, _>>()
        .map_err(fatal)
}

fn merge_headers(provider: &ImageProvider, headers: &mut Vec<(String, String)>) {
    for (name, value) in &provider.headers {
        if let Some((_, current)) = headers
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            if !current.starts_with("multipart/form-data; boundary=") {
                current.clone_from(value);
            }
        } else {
            headers.push((name.clone(), value.clone()));
        }
    }
}

fn valid_image(bytes: &[u8]) -> bool {
    match image::extension(bytes) {
        "" => false,
        "svg" => true,
        _ => ::image::load_from_memory(bytes).is_ok(),
    }
}

/// Where generated pictures are kept: `Pictures\Myuri` in the user's folder.
pub fn folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Pictures").join("Myuri")
}

/// Save a picture under a name made from the time. Returns its path.
pub fn save(folder: &Path, bytes: &[u8], index: usize) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    let extension = image::extension(bytes);
    if extension.is_empty() {
        return Err("未知图片格式".into());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_millis())
        .unwrap_or(0);
    for index in index.. {
        let path = folder.join(format!("{stamp}-{index}.{extension}"));
        match std::fs::File::create_new(&path) {
            Ok(mut file) => {
                file.write_all(bytes).map_err(|error| error.to_string())?;
                return Ok(path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!()
}
