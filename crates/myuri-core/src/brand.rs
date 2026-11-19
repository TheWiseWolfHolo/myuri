//! Which brand icon goes with a model or a provider. The rules come from
//! Lobe Icons (`tools/lobehub-map.py` writes them as JSON): a model matches
//! when one of a rule's keywords, read as a case-insensitive regular
//! expression, is found in its name; a provider when a keyword equals it.

use std::collections::BTreeSet;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// An icon of the set. `color` says a colored variant exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brand {
    pub icon: String,
    pub color: bool,
}

impl Brand {
    /// The file name without its extension, colored when there is a choice.
    pub fn file(&self) -> String {
        if self.color { format!("{}-color", self.icon) } else { self.icon.clone() }
    }
}

pub struct Brands {
    version: String,
    models: Vec<(Vec<Regex>, Brand)>,
    providers: Vec<(Vec<String>, Brand)>,
    available: BTreeSet<String>,
}

#[derive(Deserialize)]
struct Rule {
    keywords: Vec<String>,
    icon: String,
    color: bool,
}

#[derive(Deserialize)]
struct Data {
    static_version: String,
    models: Vec<Rule>,
    providers: Vec<Rule>,
    available: Vec<String>,
}

/// Parts of a host name that say nothing about whose it is.
const PLAIN_LABELS: &[&str] = &["api", "www", "com", "net", "org", "cn", "io", "ai", "dev", "app", "co"];

impl Brands {
    pub fn parse(json: &str) -> Result<Self, String> {
        let data: Data = serde_json::from_str(json).map_err(|error| error.to_string())?;
        let brand = |rule: &Rule| Brand { icon: rule.icon.clone(), color: rule.color };
        let models = data
            .models
            .iter()
            .map(|rule| {
                // The few expressions this engine cannot read are left out.
                let keywords = rule
                    .keywords
                    .iter()
                    .filter_map(|word| RegexBuilder::new(word).case_insensitive(true).build().ok())
                    .collect();
                (keywords, brand(rule))
            })
            .collect();
        let providers = data
            .providers
            .iter()
            .map(|rule| (rule.keywords.iter().map(|word| word.to_lowercase()).collect(), brand(rule)))
            .collect();
        Ok(Self {
            version: data.static_version,
            models,
            providers,
            available: data.available.into_iter().collect(),
        })
    }

    /// The version of the icon package the names belong to.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The icon for a model name. `vendor/model` is also tried without the
    /// vendor, since some rules anchor at the start.
    pub fn model(&self, name: &str) -> Option<&Brand> {
        let short = name.rsplit('/').next().unwrap_or(name);
        [name, short].into_iter().find_map(|name| {
            self.models
                .iter()
                .find(|(keywords, _)| keywords.iter().any(|keyword| keyword.is_match(name)))
                .map(|(_, brand)| brand)
        })
    }

    /// The icon for a provider, from its id, its name and the host it is
    /// reached at; failing those, from what its name says about its models.
    pub fn provider(&self, id: &str, name: &str, base_url: &str) -> Option<&Brand> {
        let squeeze = |text: &str| text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
        let words = |text: &str| {
            text.to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| word.len() >= 3)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let host = base_url.split("://").nth(1).unwrap_or(base_url).split(['/', ':']).next().unwrap_or_default();
        let labels = host.to_lowercase().split('.').filter(|label| !PLAIN_LABELS.contains(label)).map(str::to_string).collect::<Vec<_>>();
        let mut candidates = vec![squeeze(id), squeeze(name)];
        candidates.extend(labels);
        candidates.extend(words(name));
        candidates.extend(words(id));
        candidates
            .iter()
            .filter(|candidate| !candidate.is_empty())
            .find_map(|candidate| {
                self.providers
                    .iter()
                    .find(|(keywords, _)| keywords.iter().any(|keyword| keyword == candidate))
                    .map(|(_, brand)| brand)
            })
            .or_else(|| self.model(name))
            .or_else(|| self.model(id))
    }

    /// An icon written by hand, such as `openai` or `gemini-color`: its file
    /// name if the set has it.
    pub fn named(&self, written: &str) -> Option<String> {
        let name = written.trim().to_lowercase();
        self.available.contains(&name).then_some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = r#"{
        "static_version": "1.0.0",
        "models": [
            {"keywords": ["gpt-", "^o[134]"], "icon": "openai", "color": false},
            {"keywords": ["gemini"], "icon": "gemini", "color": true},
            {"keywords": ["(?<!x)bad"], "icon": "bad", "color": false}
        ],
        "providers": [
            {"keywords": ["openai"], "icon": "openai", "color": false},
            {"keywords": ["groq"], "icon": "groq", "color": false},
            {"keywords": ["google"], "icon": "google", "color": true}
        ],
        "available": ["openai", "gemini", "gemini-color", "groq", "google-color"]
    }"#;

    #[test]
    fn models_match_by_expression() {
        let brands = Brands::parse(DATA).unwrap();
        assert_eq!(brands.model("GPT-6.1-terra").unwrap().file(), "openai");
        assert_eq!(brands.model("openai/o3-mini").unwrap().icon, "openai");
        assert_eq!(brands.model("gemini-4-carbon").unwrap().file(), "gemini-color");
        assert!(brands.model("solo-1").is_none());
        assert!(brands.model("translate").is_none());
    }

    #[test]
    fn providers_match_by_id_name_and_host() {
        let brands = Brands::parse(DATA).unwrap();
        assert_eq!(brands.provider("openai", "", "https://x.test/v1").unwrap().icon, "openai");
        assert_eq!(brands.provider("fast", "Groq Cloud", "https://x.test").unwrap().icon, "groq");
        assert_eq!(brands.provider("a", "b", "https://api.groq.com/openai/v1").unwrap().icon, "groq");
        // Nothing names the provider, but its name says what it serves.
        assert_eq!(brands.provider("relay-2", "Relay Gemini", "https://x.test").unwrap().icon, "gemini");
        assert!(brands.provider("relay", "Relay", "https://x.test").is_none());
    }

    #[test]
    fn written_names_must_exist() {
        let brands = Brands::parse(DATA).unwrap();
        assert_eq!(brands.named(" Gemini-Color "), Some("gemini-color".into()));
        assert_eq!(brands.named("nothing"), None);
        assert_eq!(brands.version(), "1.0.0");
    }
}
