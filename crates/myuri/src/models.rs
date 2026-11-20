//! The model catalog: the copy built into the program, or a newer one
//! fetched with `myuri models refresh`.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use myuri_core::catalog::{Catalog, Kind};
use myuri_core::config::{self, Api, ImageProvider, Provider};
use myuri_core::edit::{self, Field};
use myuri_core::{image::ImageApi, llm};

use crate::{net, secrets, vertex};

const BUILT_IN: &str = include_str!("../assets/models.json");
const FILE: &str = "models.json";
const SOURCE: &str = "https://models.dev/api.json";
/// Safety cap on paginated `/models` responses.
const MAX_PAGES: usize = 5;

pub fn load(dir: &Path) -> Catalog {
    let cached = fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|snapshot| Catalog::from_snapshot(&snapshot).ok())
        .filter(|catalog| !catalog.is_empty());
    let builtin = || Catalog::from_snapshot(BUILT_IN).expect("valid built-in model catalog");
    match cached {
        Some(mut cached) if cached.vendors().next().is_none() => {
            cached.supplement_legacy(&builtin());
            cached
        }
        Some(cached) => cached,
        None => builtin(),
    }
}

/// When the local catalog file was last updated, or `None` when using the built-in copy.
pub fn updated_at(dir: &Path) -> Option<SystemTime> {
    let path = dir.join(FILE);
    let snapshot = fs::read_to_string(&path).ok()?;
    let catalog = Catalog::from_snapshot(&snapshot).ok()?;
    if catalog.is_empty() {
        return None;
    }
    fs::metadata(&path).ok()?.modified().ok()
}

/// Download the current catalog and keep it in the configuration folder.
/// Returns the number of models.
pub fn refresh(dir: &Path) -> Result<usize, String> {
    let body = net::get(SOURCE, &[])?;
    let catalog = Catalog::from_models_dev(&String::from_utf8_lossy(&body))?;
    fs::write(dir.join(FILE), catalog.to_snapshot()).map_err(|error| error.to_string())?;
    Ok(catalog.len())
}

pub fn guess_vendor(id: &str, name: &str, base_url: &str, catalog: &Catalog) -> Option<String> {
    const ALIASES: &[(&str, &[&str])] = &[
        ("openai", &["openai", "chatgpt", "gpt-image"]),
        ("anthropic", &["anthropic", "claude"]),
        ("google", &["google", "gemini", "vertex", "imagen", "nano banana"]),
        ("xai", &["xai", "x.ai", "grok"]),
        ("deepseek", &["deepseek", "深度求索"]),
        ("mistral", &["mistral"]),
        ("moonshotai", &["moonshot", "kimi", "月之暗面"]),
        ("alibaba", &["alibaba", "aliyun", "dashscope", "qwen", "通义", "阿里"]),
        ("zhipuai", &["zhipu", "bigmodel", "智谱", "glm-"]),
        ("minimax", &["minimax", "海螺"]),
        ("cohere", &["cohere"]),
        ("perplexity", &["perplexity"]),
        ("meta", &["meta", "llama"]),
    ];
    let available = |vendor| !catalog.vendor_models(vendor).is_empty();
    if available(id) {
        return Some(id.to_string());
    }
    for field in [id, name, base_url] {
        let field = field.to_lowercase();
        if let Some((vendor, _)) = ALIASES.iter().find(|(vendor, aliases)| {
            available(vendor) && aliases.iter().any(|alias| field.contains(alias))
        }) {
            return Some((*vendor).to_string());
        }
    }
    None
}

/// How a provider's own model list compares with the one on file.
#[derive(Clone, Debug)]
pub struct Upstream {
    pub names: Vec<String>,
    /// Offered upstream but not on file.
    pub added: Vec<String>,
    /// On file but no longer offered.
    pub gone: Vec<String>,
}

impl Upstream {
    /// Narrow the offer to the names `wanted` accepts. Models on file that
    /// it rejects count as gone.
    pub fn only(&mut self, current: &[String], wanted: impl Fn(&str) -> bool) {
        self.names.retain(|name| wanted(name));
        (self.added, self.gone) = self.diff(current);
    }

    /// Recompute `added` and `gone` against `current` (e.g. as the user edits the form).
    pub fn diff(&self, current: &[String]) -> (Vec<String>, Vec<String>) {
        let added = self
            .names
            .iter()
            .filter(|name| !current.iter().any(|known| known == *name))
            .cloned()
            .collect();
        let gone = current
            .iter()
            .filter(|known| !self.names.iter().any(|name| name == *known))
            .cloned()
            .collect();
        (added, gone)
    }
}

/// Whether `name` describes an image generation model.
pub fn is_picture(name: &str, catalog: &Catalog) -> bool {
    if let Some(info) = catalog.lookup(name) {
        return info.kind == Kind::Image;
    }
    looks_like_picture(name)
}

/// Whether the catalog classifies `name` as something other than a chat model
/// (image, video, speech, transcription, embedding, reranking, realtime).
pub fn is_non_chat(name: &str, catalog: &Catalog) -> bool {
    if is_picture(name, catalog) {
        return true;
    }
    catalog.lookup(name).is_some_and(|info| info.kind != Kind::Chat)
}

fn looks_like_picture(name: &str) -> bool {
    let lower = name.to_lowercase();
    let short = lower.rsplit('/').next().unwrap_or(&lower);
    const PATTERNS: &[&str] = &[
        "dall-e",
        "gpt-image",
        "chatgpt-image",
        "imagen",
        "flux",
        "ideogram",
        "recraft",
        "midjourney",
        "seedream",
        "kolors",
        "wanx",
        "grok-2-image",
        "grok-imagine",
        "nano-banana",
        "stable-diffusion",
        "sdxl",
    ];
    PATTERNS.iter().any(|pat| short.contains(pat))
        || short.ends_with("-image")
        || short.contains("-image-")
}

/// Ask a provider which models it offers, using keys from the credential store.
pub fn upstream(provider: &Provider) -> Result<Upstream, String> {
    upstream_with_secret(provider, None)
}

/// Ask a provider which models it offers. When `stored_override` is `Some`,
/// those credentials are used instead of the Windows credential store so
/// unsaved edits in the settings form can be tested directly.
pub fn upstream_with_secret(
    provider: &Provider,
    stored_override: Option<&str>,
) -> Result<Upstream, String> {
    let discovery = Discovery {
        api: provider.api, base: &provider.base_url, headers: &provider.headers,
        vertex: provider.vertex, namespace: secrets::NS_CHAT, id: &provider.id,
    };
    discovery.upstream(stored_override, &provider.models, |stored| provider.keys_to_try(stored, 0))
}

/// Discover models for the picture draft or its saved credentials.
pub fn upstream_image(provider: &ImageProvider, stored_override: Option<&str>) -> Result<Upstream, String> {
    let api = match provider.api {
        ImageApi::Gemini => Api::Gemini,
        _ => Api::ChatCompletions,
    };
    let discovery = Discovery {
        api, base: &provider.base_url, headers: &provider.headers,
        vertex: provider.vertex, namespace: secrets::NS_IMAGE, id: &provider.id,
    };
    discovery.upstream(stored_override, &provider.models, |stored| provider.keys_to_try(stored, 0))
}

struct Discovery<'a> {
    api: Api,
    base: &'a str,
    headers: &'a std::collections::BTreeMap<String, String>,
    vertex: bool,
    namespace: &'static str,
    id: &'a str,
}

impl Discovery<'_> {
    fn upstream(
        &self,
        stored_override: Option<&str>,
        current: &[String],
        keys_to_try: impl for<'a> FnOnce(&'a str) -> Vec<&'a str>,
    ) -> Result<Upstream, String> {
        if !self.base.starts_with("http://") && !self.base.starts_with("https://") {
            return Err("地址要以 http:// 或 https:// 开头".into());
        }
        let saved;
        let stored = match stored_override {
            Some(draft) => draft,
            None => {
                saved = secrets::read_in(self.namespace, self.id).ok_or("还没有密钥")?;
                &saved
            }
        };
        if stored.trim().is_empty() {
            return Err("还没有密钥".into());
        }
        let names = if self.vertex {
            let token = vertex::token(self.id, stored)?;
            self.fetch_pages("", Some(&token)).map_err(|(reason, _)| reason)?
        } else {
            let mut failure = "所有密钥都停用了".to_string();
            for key in keys_to_try(stored) {
                match self.fetch_pages(key, None) {
                    Ok(names) => return Ok(Self::compare(names, current)),
                    Err((reason, try_next)) => {
                        failure = reason;
                        if !try_next {
                            break;
                        }
                    }
                }
            }
            return Err(failure);
        };
        Ok(Self::compare(names, current))
    }

    fn compare(names: Vec<String>, current: &[String]) -> Upstream {
        let mut upstream = Upstream { names, added: Vec::new(), gone: Vec::new() };
        (upstream.added, upstream.gone) = upstream.diff(current);
        upstream
    }

    /// Fetch paginated model results. The bool in `Err` allows trying another key.
    fn fetch_pages(&self, key: &str, vertex_token: Option<&str>) -> Result<Vec<String>, (String, bool)> {
        let (mut url, mut headers) = llm::models_request(self.api, self.base, key);
        headers.extend(self.headers.iter().map(|(name, value)| (name.clone(), value.clone())));
        if let Some(token) = vertex_token {
            vertex::authorize(&mut headers, token);
        }
        let mut all = Vec::new();
        for _ in 0..MAX_PAGES {
            let (status, body) = net::get_status(&url, &headers).map_err(|reason| (reason, true))?;
            let text = String::from_utf8_lossy(&body);
            if !(200..300).contains(&status) {
                return Err((llm::describe_failure(status, &text), llm::another_key_may_work(status)));
            }
            for name in llm::parse_models(&text).map_err(|reason| (reason, false))? {
                if !all.contains(&name) {
                    all.push(name);
                }
            }
            match llm::next_models_page(self.api, self.base, &text) {
                Some(next) if next != url => url = next,
                _ => break,
            }
        }
        Ok(all)
    }
}

/// Make the lists on file match what the provider offers: models already
/// listed keep their place, new ones follow, and vanished ones go.
pub fn sync(
    dir: &Path,
    provider: &Provider,
    upstream: &Upstream,
    catalog: &Catalog,
) -> Result<(), String> {
    let kept = provider.models.iter().filter(|name| upstream.names.contains(name));
    let models = kept.chain(&upstream.added).filter(|name| !is_non_chat(name, catalog)).cloned().collect();
    let path = dir.join(config::PROVIDERS_FILE);
    let source = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let updated = edit::set_entry(&source, "provider", "id", &provider.id, &[("models", Field::List(models))])?;
    fs::write(&path, updated).map_err(|error| error.to_string())
}

/// Turn a downloaded `api.json` into a snapshot file.
pub fn build(from: &str, to: &str) -> Result<usize, String> {
    let source = fs::read_to_string(from).map_err(|error| error.to_string())?;
    let catalog = Catalog::from_models_dev(&source)?;
    fs::write(to, catalog.to_snapshot()).map_err(|error| error.to_string())?;
    Ok(catalog.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_name_heuristics_stay_conservative() {
        assert!(looks_like_picture("gpt-image-2.6-flare"));
        assert!(looks_like_picture("imagen-4.0-generate-001"));
        assert!(looks_like_picture("gemini-3.1-flash-image-preview"));
        assert!(looks_like_picture("flux-2-pro"));
        assert!(!looks_like_picture("gpt-5.4"));
        assert!(!looks_like_picture("claude-opus-4-6"));
        assert!(!looks_like_picture("gemini-4-neon"));
    }
}
