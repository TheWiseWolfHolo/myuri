//! Making pictures: sends the request, waits for services that work through
//! tasks, and fetches the results.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use myuri_core::config::Provider;
use myuri_core::image::{self, ImageRequest, Picture, Step};
use myuri_core::llm;

use crate::{net, secrets};

/// How long a task may take before giving up.
const PATIENCE: Duration = Duration::from_secs(600);

/// Generate pictures. Blocks; call it off the UI thread. Returns the bytes
/// of every picture. With several keys, the next one is tried when a key is
/// refused or the service cannot be reached.
pub fn generate(provider: &Provider, request: &ImageRequest) -> Result<Vec<Vec<u8>>, String> {
    let stored = secrets::read(&provider.id)
        .ok_or_else(|| format!("供应商 \"{0}\" 还没有密钥，用 myuri key {0} 设置", provider.id))?;
    let mut failure = "密钥都停用了".to_string();
    for key in provider.keys_to_try(&stored, 0) {
        match with_key(provider, key, request) {
            Ok(pictures) => return Ok(pictures),
            Err((reason, try_next)) => {
                failure = reason;
                if !try_next {
                    break;
                }
            }
        }
    }
    Err(failure)
}

/// One attempt. The flag in the error says whether another key may do better.
fn with_key(
    provider: &Provider,
    key: &str,
    request: &ImageRequest,
) -> Result<Vec<Vec<u8>>, (String, bool)> {
    let api = provider.image_api();
    let mut sent = image::build(api, &provider.base_url, key, request);
    sent.headers.extend(provider.headers.iter().map(|(name, value)| (name.clone(), value.clone())));
    let (status, body) = net::post(&sent.url, &sent.headers, sent.body.to_string().as_bytes())
        .map_err(|reason| (reason, true))?;
    let mut reply = String::from_utf8_lossy(&body).into_owned();
    if !(200..300).contains(&status) {
        return Err((llm::describe_failure(status, &reply), llm::another_key_may_work(status)));
    }
    let fatal = |reason: String| (reason, false);
    let started = Instant::now();
    let pictures = loop {
        match image::read(api, &provider.base_url, key, &reply).map_err(fatal)? {
            Step::Done(pictures) => break pictures,
            Step::Wait { url, headers } => {
                if started.elapsed() > PATIENCE {
                    return Err(fatal("等了十分钟还没画完".into()));
                }
                std::thread::sleep(Duration::from_secs(2));
                reply = String::from_utf8_lossy(&net::get(&url, &headers).map_err(fatal)?).into_owned();
            }
        }
    };
    pictures
        .into_iter()
        .map(|picture| match picture {
            Picture::Url(url) => net::get(&url, &[]),
            Picture::Base64(data) => base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|_| "图片数据解不开".to_string()),
        })
        .collect::<Result<_, _>>()
        .map_err(fatal)
}

/// Where generated pictures are kept: `Pictures\Myuri` in the user's folder.
pub fn folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    home.join("Pictures").join("Myuri")
}

/// Save a picture under a name made from the time. Returns its path.
pub fn save(folder: &Path, bytes: &[u8], index: usize) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|now| now.as_secs()).unwrap_or(0);
    let path = folder.join(format!("{stamp}-{index}.{}", image::extension(bytes)));
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path)
}
