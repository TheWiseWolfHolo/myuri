//! What each model can do, taken from the models.dev catalog: what kind of
//! model it is, whether it reads pictures, and how its reasoning is
//! controlled. Providers name the same model in many ways, so lookups go
//! through a normalized name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Reasoning;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Chat,
    Image,
    Video,
    Speech,
    Transcription,
    Embedding,
    Reranking,
    Realtime,
}

/// Field names are single letters because a few thousand of these are stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    #[serde(rename = "k", default, skip_serializing_if = "is_chat")]
    pub kind: Kind,
    /// Reads pictures.
    #[serde(rename = "v", default, skip_serializing_if = "is_false")]
    pub vision: bool,
    #[serde(rename = "r", default, skip_serializing_if = "is_false")]
    pub reasoning: bool,
    /// Named effort levels the model accepts, weakest first.
    #[serde(rename = "e", default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<Reasoning>,
    /// Smallest and largest thinking budget in tokens.
    #[serde(rename = "b", default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<(u32, u32)>,
    /// Thinking can be switched off and on.
    #[serde(rename = "t", default, skip_serializing_if = "is_false")]
    pub toggle: bool,
}

fn is_chat(kind: &Kind) -> bool {
    *kind == Kind::Chat
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Budget assumed when the catalog gives no upper bound.
const LARGEST_BUDGET: u32 = 32_000;

impl ModelInfo {
    /// The reasoning settings worth offering, weakest first. Empty when the
    /// model does not reason.
    pub fn levels(&self) -> Vec<Reasoning> {
        use Reasoning::{High, Low, Medium, None};
        if !self.reasoning {
            return Vec::new();
        }
        if !self.efforts.is_empty() {
            return self.efforts.clone();
        }
        let can_stop = self.toggle || self.budget.is_some_and(|(least, _)| least == 0);
        let mut levels = if can_stop { vec![None] } else { Vec::new() };
        match self.budget {
            Some(_) => levels.extend([Low, Medium, High]),
            // On or off only; "medium" stands for on.
            Option::None if self.toggle => levels.push(Medium),
            // The catalog does not say how; offer the common three.
            Option::None => levels.extend([Low, Medium, High]),
        }
        levels
    }

    /// Whether the only choice is between thinking and not thinking.
    pub fn switch_only(&self) -> bool {
        self.reasoning && self.efforts.is_empty() && self.budget.is_none() && self.toggle
    }

    /// The offered level closest to `wanted`.
    pub fn fit(&self, wanted: Reasoning) -> Option<Reasoning> {
        nearest(&self.levels(), wanted)
    }

    /// The thinking budget that stands for `level`.
    pub fn budget_for(&self, level: Reasoning) -> u32 {
        let (least, most) = self.budget.unwrap_or((1_024, LARGEST_BUDGET));
        let tokens = match level {
            Reasoning::None => return if self.toggle { 0 } else { least },
            Reasoning::Minimal => 1_024,
            Reasoning::Low => 2_000,
            Reasoning::Medium => 8_000,
            Reasoning::High => 24_000,
            Reasoning::Xhigh => 32_000,
            Reasoning::Max => most,
        };
        tokens.clamp(least.max(1), most.max(least).max(1))
    }
}

/// The entry of `levels` closest to `wanted`; the weaker one on a tie.
pub fn nearest(levels: &[Reasoning], wanted: Reasoning) -> Option<Reasoning> {
    levels.iter().copied().min_by_key(|&level| {
        let distance = (level as i32 - wanted as i32).abs();
        (distance, level as i32)
    })
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Catalog {
    models: BTreeMap<String, ModelInfo>,
}

/// Providers describing their own models; their word wins over a reseller's.
const FIRST_PARTY: &[&str] = &[
    "openai", "anthropic", "google", "xai", "deepseek", "mistral", "moonshotai", "alibaba",
    "zhipuai", "minimax", "cohere", "perplexity", "meta", "llama",
];

impl Catalog {
    /// Build from the models.dev `api.json` document.
    pub fn from_models_dev(source: &str) -> Result<Self, String> {
        let document: Value = serde_json::from_str(source).map_err(|e| e.to_string())?;
        let providers = document.as_object().ok_or("不是 models.dev 的目录格式")?;
        let rank = |id: &str| FIRST_PARTY.iter().position(|first| *first == id).unwrap_or(usize::MAX);
        let mut ordered: Vec<(&String, &Value)> = providers.iter().collect();
        ordered.sort_by_key(|(id, _)| rank(id));
        let mut models = BTreeMap::new();
        for (_, provider) in ordered {
            for (id, model) in provider["models"].as_object().into_iter().flatten() {
                // First come, first kept: first parties were sorted to the front.
                models.entry(normalize(id)).or_insert_with(|| describe(model));
            }
        }
        if models.is_empty() {
            return Err("目录里没有模型".into());
        }
        Ok(Self { models })
    }

    pub fn from_snapshot(source: &str) -> Result<Self, String> {
        serde_json::from_str(source).map_err(|e| e.to_string())
    }

    pub fn to_snapshot(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// What is known about a model, whatever the provider calls it. Relays add
    /// suffixes such as `-low` or `-nothinking`; those are peeled off one at
    /// a time until a known name is left.
    pub fn lookup(&self, model: &str) -> Option<&ModelInfo> {
        let name = normalize(model);
        let mut candidate = name.as_str();
        loop {
            if let Some(info) = self.models.get(candidate) {
                return Some(info);
            }
            let shorter = &candidate[..candidate.rfind('-')?];
            // One word alone ("gpt", "claude") names a family, not a model.
            if !shorter.contains('-') {
                return None;
            }
            candidate = shorter;
        }
    }
}

/// Lowercase, without the vendor prefix and the `:free` style tag, and with
/// every separator a hyphen: `Org/Claude-Opus-4.5:free` is `claude-opus-4-5`.
pub fn normalize(model: &str) -> String {
    let lower = model.trim().to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let name = name.split(':').next().unwrap_or(name);
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let c = if matches!(c, '.' | '_' | ' ') { '-' } else { c };
        if c != '-' || !out.ends_with('-') {
            out.push(c);
        }
    }
    out.trim_matches('-').to_string()
}

fn describe(model: &Value) -> ModelInfo {
    let has = |list: &Value, what: &str| list.as_array().is_some_and(|l| l.iter().any(|v| v == what));
    let (input, output) = (&model["modalities"]["input"], &model["modalities"]["output"]);
    let kind = match model["type"].as_str() {
        Some("image") => Kind::Image,
        Some("video") => Kind::Video,
        Some("speech") => Kind::Speech,
        Some("transcription") => Kind::Transcription,
        Some("embedding") => Kind::Embedding,
        Some("reranking") => Kind::Reranking,
        Some("realtime") => Kind::Realtime,
        _ if has(output, "text") => Kind::Chat,
        _ if has(output, "image") => Kind::Image,
        _ if has(output, "video") => Kind::Video,
        _ if has(output, "audio") => Kind::Speech,
        _ => Kind::Chat,
    };
    let mut info = ModelInfo {
        kind,
        vision: has(input, "image"),
        reasoning: model["reasoning"] == true,
        ..ModelInfo::default()
    };
    for option in model["reasoning_options"].as_array().into_iter().flatten() {
        match option["type"].as_str() {
            Some("effort") => {
                let mut efforts: Vec<Reasoning> = option["values"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| Reasoning::parse(value.as_str()?))
                    .collect();
                efforts.sort();
                efforts.dedup();
                info.efforts = efforts;
            }
            Some("budget_tokens") => {
                let bound = |key: &str| option[key].as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32);
                info.budget = Some((bound("min").unwrap_or(0), bound("max").unwrap_or(LARGEST_BUDGET)));
            }
            Some("toggle") => info.toggle = true,
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use Reasoning::*;

    const SAMPLE: &str = r#"{
      "reseller": {"models": {
        "vendor/GPT-9.5:free": {"reasoning": false, "modalities": {"input": ["text"], "output": ["text"]}},
        "painter-1": {"reasoning": false, "modalities": {"input": ["text"], "output": ["image"]}}
      }},
      "openai": {"models": {
        "gpt-9.5": {"reasoning": true,
          "reasoning_options": [{"type": "effort", "values": ["none", "low", "high", "xhigh", "bogus"]}],
          "modalities": {"input": ["text", "image"], "output": ["text"]}},
        "embed-3": {"type": "embedding", "reasoning": false, "modalities": {"input": ["text"], "output": ["text"]}}
      }},
      "google": {"models": {
        "gemini-2.5-flash": {"reasoning": true,
          "reasoning_options": [{"type": "toggle"}, {"type": "budget_tokens", "min": 0, "max": 24576}],
          "modalities": {"input": ["text", "image"], "output": ["text"]}}
      }},
      "anthropic": {"models": {
        "claude-sonnet-4-5": {"reasoning": true, "reasoning_options": [{"type": "budget_tokens", "min": 1024}],
          "modalities": {"input": ["text", "image"], "output": ["text"]}}
      }},
      "zhipuai": {"models": {
        "glm-5": {"reasoning": true, "reasoning_options": [{"type": "toggle"}],
          "modalities": {"input": ["text"], "output": ["text"]}},
        "mystery-r1": {"reasoning": true, "modalities": {"input": ["text"], "output": ["text"]}}
      }}
    }"#;

    fn catalog() -> Catalog {
        Catalog::from_models_dev(SAMPLE).unwrap()
    }

    #[test]
    fn names_are_normalized() {
        assert_eq!(normalize("Org/Claude-Opus-4.5:free"), "claude-opus-4-5");
        assert_eq!(normalize(" gemini_3.8 flash "), "gemini-3-8-flash");
        assert_eq!(normalize("假流式/gpt-oss-120b"), "gpt-oss-120b");
    }

    #[test]
    fn first_parties_win_and_kinds_are_read() {
        let catalog = catalog();
        assert_eq!(catalog.len(), 7);
        let gpt = catalog.lookup("gpt-9.5").unwrap();
        // The reseller's entry said no reasoning; OpenAI's own is kept.
        assert!(gpt.reasoning && gpt.vision);
        assert_eq!(gpt.efforts, [None, Low, High, Xhigh]);
        assert_eq!(catalog.lookup("painter-1").unwrap().kind, Kind::Image);
        assert_eq!(catalog.lookup("embed-3").unwrap().kind, Kind::Embedding);
    }

    #[test]
    fn relay_names_resolve_to_the_model() {
        let catalog = catalog();
        let known = catalog.lookup("gpt-9.5").unwrap();
        assert_eq!(catalog.lookup("抗截断/GPT-9.5-high"), Some(known));
        assert_eq!(catalog.lookup("gpt-9-5-nothinking"), Some(known));
        assert_eq!(catalog.lookup("claude-sonnet-4.5-20250929").map(|m| m.budget), Some(Some((1024, 32000))));
        assert_eq!(catalog.lookup("gpt-10"), Option::None);
        assert_eq!(catalog.lookup("gpt"), Option::None);
        assert_eq!(catalog.lookup(""), Option::None);
    }

    #[test]
    fn levels_follow_how_the_model_is_controlled() {
        let catalog = catalog();
        let levels = |name: &str| catalog.lookup(name).unwrap().levels();
        assert_eq!(levels("gpt-9.5"), [None, Low, High, Xhigh]);
        assert_eq!(levels("gemini-2.5-flash"), [None, Low, Medium, High]);
        assert_eq!(levels("claude-sonnet-4-5"), [Low, Medium, High]);
        assert_eq!(levels("glm-5"), [None, Medium]);
        assert!(catalog.lookup("glm-5").unwrap().switch_only());
        assert_eq!(levels("mystery-r1"), [Low, Medium, High]);
        assert!(levels("painter-1").is_empty());
    }

    #[test]
    fn a_wanted_level_is_fitted_to_what_exists() {
        let catalog = catalog();
        let gpt = catalog.lookup("gpt-9.5").unwrap();
        assert_eq!(gpt.fit(Medium), Some(Low));
        assert_eq!(gpt.fit(Max), Some(Xhigh));
        assert_eq!(gpt.fit(Minimal), Some(None));
        assert_eq!(catalog.lookup("claude-sonnet-4-5").unwrap().fit(None), Some(Low));
        assert_eq!(catalog.lookup("painter-1").unwrap().fit(High), Option::None);
    }

    #[test]
    fn budgets_stay_inside_the_range() {
        let catalog = catalog();
        let gemini = catalog.lookup("gemini-2.5-flash").unwrap();
        assert_eq!(gemini.budget_for(None), 0);
        assert_eq!(gemini.budget_for(Medium), 8000);
        assert_eq!(gemini.budget_for(Max), 24576);
        assert_eq!(gemini.budget_for(Xhigh), 24576);
        let claude = catalog.lookup("claude-sonnet-4-5").unwrap();
        assert_eq!(claude.budget_for(None), 1024);
        assert_eq!(claude.budget_for(High), 24000);
    }

    #[test]
    fn snapshots_round_trip() {
        let catalog = catalog();
        let snapshot = catalog.to_snapshot();
        assert!(snapshot.len() < SAMPLE.len() / 2);
        assert_eq!(Catalog::from_snapshot(&snapshot), Ok(catalog));
        assert!(Catalog::from_models_dev("[]").is_err());
        assert!(Catalog::from_models_dev("{}").is_err());
    }
}
