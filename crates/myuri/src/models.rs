//! The model catalog: the copy built into the program, or a newer one
//! fetched with `myuri models refresh`.

use std::fs;
use std::path::Path;

use myuri_core::catalog::{Catalog, Kind};
use myuri_core::config::{self, Provider};
use myuri_core::llm;

use crate::{net, secrets};

const BUILT_IN: &str = include_str!("../assets/models.json");
const FILE: &str = "models.json";
const SOURCE: &str = "https://models.dev/api.json";

pub fn load(dir: &Path) -> Catalog {
    fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|snapshot| Catalog::from_snapshot(&snapshot).ok())
        .filter(|catalog| !catalog.is_empty())
        .unwrap_or_else(|| Catalog::from_snapshot(BUILT_IN).unwrap_or_default())
}

/// Download the current catalog and keep it in the configuration folder.
/// Returns the number of models.
pub fn refresh(dir: &Path) -> Result<usize, String> {
    let body = net::get(SOURCE, &[])?;
    let catalog = Catalog::from_models_dev(&String::from_utf8_lossy(&body))?;
    fs::write(dir.join(FILE), catalog.to_snapshot()).map_err(|error| error.to_string())?;
    Ok(catalog.len())
}

/// How a provider's own model list compares with the one on file.
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
    pub fn only(&mut self, provider: &Provider, wanted: impl Fn(&str) -> bool) {
        self.names.retain(|name| wanted(name));
        self.added.retain(|name| wanted(name));
        let on_file = provider.models.iter().chain(&provider.image_models);
        self.gone = on_file.filter(|known| !self.names.contains(known)).cloned().collect();
    }
}

/// Ask a provider which models it offers.
pub fn upstream(provider: &Provider) -> Result<Upstream, String> {
    let key = secrets::read(&provider.id)
        .ok_or_else(|| format!("还没有密钥，用 myuri key {} 设置", provider.id))?;
    let (url, mut headers) =
        llm::models_request(provider.api, &provider.base_url, llm::first_key(&key));
    headers.extend(provider.headers.iter().map(|(name, value)| (name.clone(), value.clone())));
    let names = llm::parse_models(&String::from_utf8_lossy(&net::get(&url, &headers)?))?;
    let on_file = || provider.models.iter().chain(&provider.image_models);
    Ok(Upstream {
        added: names.iter().filter(|name| !on_file().any(|known| known == *name)).cloned().collect(),
        gone: on_file().filter(|known| !names.contains(known)).cloned().collect(),
        names,
    })
}

/// Make the lists on file match what the provider offers: models already
/// listed keep their place, new ones follow, and vanished ones go. Picture
/// models are told apart by the catalog and by where they were before.
pub fn sync(
    dir: &Path,
    provider: &Provider,
    upstream: &Upstream,
    catalog: &Catalog,
) -> Result<(), String> {
    let is_picture = |name: &String| {
        provider.image_models.contains(name)
            || catalog.lookup(name).is_some_and(|info| info.kind == Kind::Image)
    };
    let on_file = provider.models.iter().chain(&provider.image_models);
    let kept = on_file.filter(|name| upstream.names.contains(name));
    let (pictures, models): (Vec<String>, Vec<String>) =
        kept.chain(&upstream.added).cloned().partition(is_picture);
    let path = dir.join(config::PROVIDERS_FILE);
    let source = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let updated = config::with_models(&source, &provider.id, &models, &pictures)?;
    fs::write(&path, updated).map_err(|error| error.to_string())
}

/// Turn a downloaded `api.json` into a snapshot file.
pub fn build(from: &str, to: &str) -> Result<usize, String> {
    let source = fs::read_to_string(from).map_err(|error| error.to_string())?;
    let catalog = Catalog::from_models_dev(&source)?;
    fs::write(to, catalog.to_snapshot()).map_err(|error| error.to_string())?;
    Ok(catalog.len())
}
