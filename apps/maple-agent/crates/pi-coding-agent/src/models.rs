use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use pi_ai::{ApiRegistry, Model, StreamFn};

/// Supplies credentials per provider, looked up for every request.
#[async_trait]
pub trait ApiKeySource: Send + Sync {
    async fn api_key(&self, provider: &str) -> Option<String>;
}

/// Fixed keys by provider.
#[derive(Clone, Debug, Default)]
pub struct StaticKeys(pub HashMap<String, String>);

#[async_trait]
impl ApiKeySource for StaticKeys {
    async fn api_key(&self, provider: &str) -> Option<String> {
        self.0.get(provider).cloned()
    }
}

/// The models a session can use, the APIs that serve them and their credentials.
#[derive(Clone)]
pub struct ModelRegistry {
    models: Arc<RwLock<Vec<Model>>>,
    apis: ApiRegistry,
    keys: Arc<dyn ApiKeySource>,
}

impl ModelRegistry {
    pub fn new(keys: Arc<dyn ApiKeySource>) -> Self {
        Self {
            models: Arc::default(),
            apis: ApiRegistry::new(),
            keys,
        }
    }

    /// Serve models whose `api` is `api` with `implementation`.
    pub fn register_api(&self, api: &str, implementation: Arc<dyn StreamFn>) {
        self.apis.register(api, implementation);
    }

    /// Add models, replacing any with the same provider and id.
    pub fn register_models(&self, models: impl IntoIterator<Item = Model>) {
        let mut known = self
            .models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for model in models {
            known.retain(|existing| {
                !(existing.provider == model.provider && existing.id == model.id)
            });
            known.push(model);
        }
    }

    pub fn unregister_provider(&self, provider: &str) {
        self.models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|model| model.provider != provider);
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<Model> {
        self.models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .cloned()
    }

    pub fn models(&self) -> Vec<Model> {
        self.models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Streams through whichever API serves the requested model.
    pub fn stream_fn(&self) -> Arc<dyn StreamFn> {
        Arc::new(self.apis.clone())
    }

    pub async fn api_key(&self, provider: &str) -> Option<String> {
        self.keys.api_key(provider).await
    }
}
