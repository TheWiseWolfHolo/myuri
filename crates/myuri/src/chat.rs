//! From an assistant to a request on the wire: which provider and model it
//! uses, with which key, and how the request is sent.

use myuri_core::catalog::ModelInfo;
use myuri_core::config::{Api, Assistant, KeyStrategy, ModelRef, Provider, Reasoning, Settings};
use myuri_core::llm::{self, ChatRequest, Event, Turn};

use std::collections::HashMap;
use std::sync::Mutex;

use crate::net::{self, Abort, Attempt};
use crate::{secrets, vertex};

/// Counts the requests made through each provider, for taking turns among
/// its keys.
fn next_turn(provider: &str) -> usize {
    static TURNS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
    let mut turns = TURNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let turn = turns.get_or_insert_with(HashMap::new).entry(provider.to_string()).or_insert(0);
    *turn += 1;
    *turn - 1
}

/// Where an assistant's requests go.
pub struct Route {
    api: Api,
    base_url: String,
    headers: Vec<(String, String)>,
    /// The keys as the credential store holds them.
    stored: String,
    /// The provider's key settings.
    provider: Provider,
    /// Where a sticky conversation starts among the keys.
    seat: usize,
    pub model: String,
    pub provider_id: String,
}

impl Route {
    /// Explains what is missing when the assistant cannot be reached.
    pub fn find(
        settings: &Settings,
        providers: &[Provider],
        assistant: &Assistant,
    ) -> Result<Self, String> {
        Self::to(providers, &settings.model_for(assistant)?)
    }

    /// The route to one particular model.
    pub fn to(providers: &[Provider], model: &ModelRef) -> Result<Self, String> {
        let provider = providers
            .iter()
            .find(|provider| provider.id == model.provider)
            .ok_or_else(|| format!("供应商 \"{}\" 不在 providers.toml 里", model.provider))?;
        if !provider.enabled {
            return Err(format!("供应商 \"{}\" 被停用了", provider.id));
        }
        let stored = secrets::read(&provider.id).ok_or_else(|| {
            format!("供应商 \"{0}\" 还没有密钥，用 myuri key {0} 设置", provider.id)
        })?;
        if provider.keys_to_try(&stored, 0).is_empty() {
            return Err(format!("供应商 \"{}\" 的密钥都停用了", provider.id));
        }
        // A sticky conversation takes the next seat once and keeps it.
        let seat = match provider.key_strategy {
            KeyStrategy::Sticky => next_turn(&provider.id),
            _ => 0,
        };
        Ok(Self {
            api: provider.api,
            base_url: provider.base_url.clone(),
            headers: provider.headers.clone().into_iter().collect(),
            stored,
            seat,
            provider: provider.clone(),
            model: model.model.clone(),
            provider_id: provider.id.clone(),
        })
    }

    /// Send a conversation. `on_event` runs on a worker thread; its last
    /// event is always `Done` or `Error`.
    pub fn send(
        &self,
        system: &str,
        turns: &[Turn],
        reasoning: Option<Reasoning>,
        temperature: Option<f32>,
        info: Option<&ModelInfo>,
        tools: &[llm::Tool],
        on_event: impl FnMut(Event) + Send + 'static,
    ) -> Abort {
        // A level the model lacks becomes the nearest one it has.
        let reasoning = match (reasoning, info) {
            (Some(level), Some(info)) => info.fit(level),
            (level, _) => level,
        };
        let build = |key: &str, reasoning| {
            let chat = ChatRequest {
                model: &self.model,
                system,
                turns,
                reasoning,
                temperature,
                info,
                tools,
            };
            let mut request = llm::build(self.api, &self.base_url, key, &chat);
            request.headers.extend(self.headers.iter().cloned());
            request
        };
        let turn = match self.provider.key_strategy {
            KeyStrategy::Fallback => 0,
            KeyStrategy::RoundRobin => next_turn(&self.provider_id),
            KeyStrategy::Sticky => self.seat,
        };
        let account = self.provider.vertex.then(|| (self.provider_id.clone(), self.stored.clone()));
        let attempts = self
            .provider
            .keys_to_try(&self.stored, turn)
            .into_iter()
            .map(|key| {
                let key = if account.is_some() { "" } else { key };
                Attempt {
                    request: build(key, reasoning),
                    // Some models refuse a reasoning level they do not have. Keep
                    // a request without one ready for that case.
                    plain: reasoning.map(|_| build(key, None)),
                }
            })
            .collect();
        net::stream(attempts, self.api, move |attempts| {
            if let Some((id, account)) = account {
                let token = vertex::token(&id, &account)?;
                for attempt in attempts {
                    vertex::authorize(&mut attempt.request.headers, &token);
                    if let Some(plain) = &mut attempt.plain {
                        vertex::authorize(&mut plain.headers, &token);
                    }
                }
            }
            Ok(())
        }, on_event)
    }
}
