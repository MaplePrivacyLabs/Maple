//! The account's model catalog: the models a task may pick, their context
//! windows and vision, and the voice endpoints.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::config::{account_store, load_agent_config_inner, save_agent_config_inner, unix_ms};
use super::provider::{CatalogEntry, catalog_entry};
use super::types::{
    TRANSCRIPTION_MODEL, TTS_MODEL, audio_error_message, selectable_agent_model_id,
    speech_audio_from_body,
};
use super::{
    AgentPathLayout, AgentRuntimeHandle, AudioCapabilities, DEFAULT_AGENT_MODEL,
    PREVIOUS_BUILT_IN_AGENT_MODEL,
};
use crate::maple_api::MapleApiSession;

/// A cached catalog answers for this long without a network round trip.
/// The ACP process is short-lived, so the cache lives on disk next to the
/// account config and survives restarts.
const MODEL_CATALOG_TTL_MS: u128 = 10 * 60 * 1000;

const MODEL_CATALOG_CACHE_KEY: &str = "cache.model_catalog";

/// The model the user last picked in the composer. Kept apart from
/// `agent.defaults`, whose saved model is also written on every runtime
/// start, so a pick is never confused with a fallback.
const MODEL_CHOICE_KEY: &str = "agent.model_choice";

/// One background refresh at a time per process.
static MODEL_CATALOG_REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
struct CachedModelCatalog {
    fetched_at_ms: u64,
    catalog: maple_sdk::ModelCatalogResponse,
}

/// `model` is `None` once an account is known to have no pick, so the
/// one-time adoption below never runs twice.
#[derive(Serialize, Deserialize)]
struct ModelChoice {
    #[serde(default)]
    model: Option<String>,
}

fn is_fresh(fetched_at_ms: u64) -> bool {
    unix_ms().saturating_sub(u128::from(fetched_at_ms)) < MODEL_CATALOG_TTL_MS
}

/// The account's catalog: from a fresh cache, from a stale one while a
/// refresh runs in the background, or fetched when there is no cache.
/// `None` only when there is no cache and the fetch fails.
async fn cached_catalog(
    paths: &AgentPathLayout,
    user_id: &str,
    api: &Arc<MapleApiSession>,
) -> Option<maple_sdk::ModelCatalogResponse> {
    let cache = account_store(paths, user_id)
        .ok()
        .and_then(|(_, account)| paths.store().ok().map(|store| (Arc::clone(store), account)));
    let cached = if let Some((store, account)) = cache.clone() {
        tokio::task::spawn_blocking(move || {
            store
                .account_setting(account, MODEL_CATALOG_CACHE_KEY)
                .ok()
                .flatten()
                // An entry from an older build holds bare ids; refetch.
                .and_then(|value| serde_json::from_value::<CachedModelCatalog>(value).ok())
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    if let Some(record) = cached {
        if is_fresh(record.fetched_at_ms) {
            return Some(record.catalog);
        }
        // Answer from the stale cache and refresh it for the next caller,
        // so no request waits on the catalog after the first fetch.
        if let Some(cache) = cache
            && MODEL_CATALOG_REFRESH_IN_FLIGHT
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let api = Arc::clone(api);
            tokio::spawn(async move {
                match api.model_catalog().await {
                    Ok(catalog) => write_model_catalog(cache, catalog).await,
                    Err(error) => log::warn!("Background model catalog refresh failed: {error}"),
                }
                MODEL_CATALOG_REFRESH_IN_FLIGHT.store(false, Ordering::Relaxed);
            });
        }
        return Some(record.catalog);
    }
    match api.model_catalog().await {
        Ok(catalog) => {
            if let Some(cache) = cache {
                write_model_catalog(cache, catalog.clone()).await;
            }
            Some(catalog)
        }
        Err(error) => {
            log::warn!("Failed to fetch the Maple Work model catalog: {error}");
            None
        }
    }
}

async fn write_model_catalog(
    (store, account): (Arc<crate::store::Store>, crate::store::AccountId),
    catalog: maple_sdk::ModelCatalogResponse,
) {
    let record = CachedModelCatalog {
        fetched_at_ms: u64::try_from(unix_ms()).unwrap_or(u64::MAX),
        catalog,
    };
    let Ok(value) = serde_json::to_value(record) else {
        return;
    };
    let written = tokio::task::spawn_blocking(move || {
        store.set_account_setting(account, MODEL_CATALOG_CACHE_KEY, &value, pi_ai::now_ms())
    })
    .await;
    if !matches!(written, Ok(Ok(()))) {
        log::warn!("Failed to write the model catalog cache");
    }
}

/// The catalog's chat models a task may pick, in catalog order. Aliases are
/// left out: the server may retarget them, and a local agent cannot follow a
/// switch in context window or vision mid-task. A model without
/// `available_in_agent` is offered, since older servers do not send it.
pub(super) fn selectable_catalog_models(catalog: &maple_sdk::ModelCatalogResponse) -> Vec<String> {
    let mut seen = HashSet::new();
    catalog
        .data
        .iter()
        .filter(|model| {
            model.enabled != Some(false)
                && model.deprecated != Some(true)
                && model.available_in_agent != Some(false)
                && model
                    .capabilities
                    .as_ref()
                    .is_none_or(|capabilities| capabilities.chat)
                && !model.id.trim().is_empty()
                && selectable_agent_model_id(&model.id)
        })
        .filter(|model| seen.insert(model.id.clone()))
        .map(|model| model.id.clone())
        .collect()
}

/// The names surfaces show for the catalog's models and aliases, with
/// Research's precedence: the short name, then the full one, then the id.
/// IDs remain the keys used for selection and persisted tasks.
pub fn catalog_model_labels(
    catalog: &maple_sdk::ModelCatalogResponse,
) -> std::collections::HashMap<String, String> {
    let label = |short: Option<&str>, full: Option<&str>, id: &str| {
        short
            .filter(|value| !value.trim().is_empty())
            .or_else(|| full.filter(|value| !value.trim().is_empty()))
            .unwrap_or(id)
            .to_string()
    };
    catalog
        .data
        .iter()
        .map(|model| {
            (
                model.id.clone(),
                label(
                    model.short_name.as_deref(),
                    model.display_name.as_deref(),
                    &model.id,
                ),
            )
        })
        .chain(catalog.aliases.iter().map(|alias| {
            (
                alias.id.clone(),
                label(
                    alias.short_name.as_deref(),
                    alias.label.as_deref(),
                    &alias.id,
                ),
            )
        }))
        .collect()
}

/// The model a new task starts on: the user's last pick, then the server's
/// suggestion, then the built-in default, then the catalog's first model;
/// each only while the catalog offers it. With no catalog to check against,
/// the pick or `unchecked`, the model the runtime started on.
pub(super) fn resolve_default_model(
    models: &[String],
    choice: Option<&str>,
    suggested: Option<&str>,
    unchecked: &str,
) -> String {
    let offered = |model: &&str| models.iter().any(|candidate| candidate == model);
    if models.is_empty() {
        return choice.unwrap_or(unchecked).to_string();
    }
    choice
        .filter(offered)
        .or(suggested.filter(offered))
        .or(Some(DEFAULT_AGENT_MODEL).filter(offered))
        .unwrap_or(&models[0])
        .to_string()
}

fn model_choice_inner(paths: &AgentPathLayout, user_id: &str) -> Result<Option<String>, String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    if let Some(choice) = store
        .account_setting(account, MODEL_CHOICE_KEY)
        .map_err(|error| error.to_string())?
        .and_then(|value| serde_json::from_value::<ModelChoice>(value).ok())
    {
        return Ok(choice.model.filter(|model| !model.trim().is_empty()));
    }
    // Builds before the separate key saved only `defaultModel`, which held
    // the built-in default unless the user picked something else. Adopt it
    // once, so a later change of built-in default never reads as a pick.
    let legacy = load_agent_config_inner(paths, user_id)
        .map_err(|error| error.to_string())?
        .default_model;
    let model = Some(legacy).filter(|model| {
        !model.trim().is_empty()
            && model != PREVIOUS_BUILT_IN_AGENT_MODEL
            && model != DEFAULT_AGENT_MODEL
    });
    write_model_choice(store, account, model.clone())?;
    Ok(model)
}

fn write_model_choice(
    store: &crate::store::Store,
    account: crate::store::AccountId,
    model: Option<String>,
) -> Result<(), String> {
    let value = serde_json::to_value(ModelChoice { model }).map_err(|error| error.to_string())?;
    store
        .set_account_setting(account, MODEL_CHOICE_KEY, &value, pi_ai::now_ms())
        .map_err(|error| error.to_string())
}

impl AgentRuntimeHandle {
    /// The model the user last picked, if any. Not checked against the
    /// catalog; [`Self::available_model_ids`] lists the resolved default first.
    pub async fn model_choice(&self) -> Result<Option<String>, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        model_choice_inner(self.paths(), &self.user_id)
    }

    /// Remember a model the user picked. It becomes the account default for
    /// new tasks while the catalog offers it.
    pub async fn save_model_choice(&self, model: String) -> Result<(), String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _settings = self.lock_settings().await;
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|error| error.to_string())?;
        write_model_choice(store, account, Some(model.clone()))?;
        // Older builds read the pick from `agent.defaults`.
        let mut config =
            load_agent_config_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())?;
        config.default_model = model;
        save_agent_config_inner(self.paths(), &self.user_id, &config).map_err(|e| e.to_string())
    }

    async fn api(&self) -> Result<Arc<MapleApiSession>, String> {
        Ok(Arc::clone(&self.runtime().await?.api))
    }

    /// Full presentation metadata from the advanced catalog, including labels
    /// for concrete models and aliases, served from the disk cache when it
    /// has one. Does not select or add any models.
    pub async fn model_catalog(&self) -> Result<maple_sdk::ModelCatalogResponse, String> {
        let api = self.api().await?;
        let catalog = cached_catalog(self.paths(), &self.user_id, &api)
            .await
            .ok_or_else(|| "The model catalog is unavailable".to_string())?;
        self.verify_generation().await?;
        Ok(catalog)
    }

    /// The models a task may pick, the default for a new task first.
    pub async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        let runtime = self.runtime().await?;
        let catalog = cached_catalog(self.paths(), &self.user_id, &runtime.api).await;
        // A request can outlive a sign-out; answer only for this account.
        self.verify_generation().await?;
        let mut models = catalog
            .as_ref()
            .map(selectable_catalog_models)
            .unwrap_or_default();
        let choice = self.model_choice().await?;
        let suggested = catalog
            .as_ref()
            .and_then(|catalog| catalog.defaults.agent.as_deref());
        let default_model =
            resolve_default_model(&models, choice.as_deref(), suggested, &runtime.model);
        models.retain(|model| model != &default_model);
        models.insert(0, default_model);
        Ok(models)
    }

    /// What the live catalog says about a model, an alias resolved to its
    /// target. `None` when the catalog cannot be read; a model it does not
    /// list has an empty entry.
    pub async fn model_catalog_entry(
        &self,
        model_id: &str,
    ) -> Result<Option<CatalogEntry>, String> {
        if model_id.trim().is_empty() {
            return Ok(Some(CatalogEntry::default()));
        }
        match self.api().await?.model_catalog().await {
            Ok(catalog) => Ok(Some(catalog_entry(&catalog, model_id).unwrap_or_default())),
            Err(error) => {
                log::warn!("Failed to fetch the Maple Work model catalog: {error}");
                Ok(None)
            }
        }
    }

    /// A model's context window from the live catalog, an alias resolved to
    /// its target. `None` when the catalog does not say.
    pub async fn context_limit_for_model(&self, model_id: &str) -> Result<Option<i64>, String> {
        Ok(self
            .model_catalog_entry(model_id)
            .await?
            .and_then(|entry| entry.context_window)
            .and_then(|window| i64::try_from(window).ok()))
    }

    /// Which voice endpoints the account's catalog offers.
    pub async fn audio_capabilities(&self) -> Result<AudioCapabilities, String> {
        let audio = self.model_catalog().await?.audio.unwrap_or_default();
        let available = |endpoint: Option<&maple_sdk::CatalogAudioEndpoint>| {
            endpoint.is_some_and(|endpoint| endpoint.available)
        };
        Ok(AudioCapabilities {
            transcription: available(audio.transcription.as_ref()),
            speech: available(audio.speech.as_ref()),
        })
    }

    /// Turn `text` into WAV audio with Maple's text-to-speech model.
    pub async fn synthesize_speech(
        &self,
        text: &str,
        voice: &str,
        speed: f32,
    ) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "input": text,
            "model": TTS_MODEL,
            "voice": voice,
            "speed": speed,
        }))
        .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let response = self
            .api()
            .await?
            .audio_request("/v1/audio/speech", "audio/wav", body)
            .await?;
        log::info!(
            "text-to-speech: HTTP {} ({}, {} bytes) after {:?} for {} chars",
            response.status,
            response.content_type,
            response.body.len(),
            started.elapsed(),
            text.chars().count()
        );
        if let Some(message) = audio_error_message(&response) {
            log::warn!("text-to-speech failed: {message}");
            return Err(message);
        }
        speech_audio_from_body(response.body)
    }

    /// Transcribe WAV audio with Maple's Whisper model.
    pub async fn transcribe_audio(&self, wav: Vec<u8>) -> Result<String, String> {
        use base64::Engine;

        let body = serde_json::to_vec(&serde_json::json!({
            "file": base64::engine::general_purpose::STANDARD.encode(&wav),
            "filename": "recording.wav",
            "content_type": "audio/wav",
            "model": TRANSCRIPTION_MODEL,
        }))
        .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let response = self
            .api()
            .await?
            .audio_request("/v1/audio/transcriptions", "application/json", body)
            .await?;
        log::info!(
            "transcription: HTTP {} ({} bytes) after {:?}",
            response.status,
            response.body.len(),
            started.elapsed()
        );
        if let Some(message) = audio_error_message(&response) {
            log::warn!("transcription failed: {message}");
            return Err(message);
        }
        let parsed: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|error| format!("Transcription returned invalid JSON: {error}"))?;
        Ok(parsed
            .get("text")
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::catalog_model_labels;

    #[test]
    fn model_labels_follow_research_catalog_precedence() {
        let catalog = serde_json::from_value(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "model-a", "short_name": "Compact", "display_name": "Full name"},
                {"id": "model-b", "display_name": "Server supplied name"},
                {"id": "model-c", "short_name": " ", "display_name": "Fallback name"},
                {"id": "model-d"}
            ],
            "aliases": [
                {"id": "alias-a", "short_name": "Alias", "label": "Full alias", "target_model": "model-a"},
                {"id": "alias-b", "label": "Automatic", "target_model": "model-b"},
                {"id": "alias-c", "short_name": "", "label": " ", "target_model": "model-c"}
            ]
        }))
        .unwrap();
        let labels = catalog_model_labels(&catalog);
        assert_eq!(labels["model-a"], "Compact");
        assert_eq!(labels["model-b"], "Server supplied name");
        assert_eq!(labels["model-c"], "Fallback name");
        assert_eq!(labels["model-d"], "model-d");
        assert_eq!(labels["alias-a"], "Alias");
        assert_eq!(labels["alias-b"], "Automatic");
        assert_eq!(labels["alias-c"], "alias-c");
    }
}
