//! The account's model catalog: the models a task may pick, their context
//! windows and vision, and the voice endpoints.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::config::{agent_config_dir, unix_ms};
use super::provider::catalog_entry;
use super::types::{
    TRANSCRIPTION_MODEL, TTS_MODEL, audio_error_message, selectable_agent_model_id,
    speech_audio_from_body,
};
use super::{AgentPathLayout, AgentRuntimeHandle, AudioCapabilities};
use crate::maple_api::MapleApiSession;

/// A cached catalog answers for this long without a network round trip.
/// The ACP process is short-lived, so the cache lives on disk next to the
/// account config and survives restarts.
const MODEL_CATALOG_TTL_MS: u128 = 10 * 60 * 1000;

/// One background refresh at a time per process.
static MODEL_CATALOG_REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
struct CachedModelCatalog {
    fetched_at_ms: u64,
    models: Vec<String>,
}

fn model_catalog_cache_path(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(agent_config_dir(paths, user_id)
        .map_err(|error| error.to_string())?
        .join("model-catalog.json"))
}

fn is_fresh(fetched_at_ms: u64) -> bool {
    unix_ms().saturating_sub(u128::from(fetched_at_ms)) < MODEL_CATALOG_TTL_MS
}

/// The account's model ids: from a fresh cache, from a stale one while a
/// refresh runs in the background, or fetched when there is no cache.
async fn cached_model_ids(
    paths: &AgentPathLayout,
    user_id: &str,
    api: &Arc<MapleApiSession>,
) -> Vec<String> {
    let cache_path = match model_catalog_cache_path(paths, user_id) {
        Ok(path) => Some(path),
        Err(error) => {
            log::warn!("Failed to place the model catalog cache: {error}");
            None
        }
    };
    let cached = match cache_path.clone() {
        Some(path) => tokio::task::spawn_blocking(move || {
            fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<CachedModelCatalog>(&text).ok())
        })
        .await
        .ok()
        .flatten(),
        None => None,
    };
    if let Some(record) = cached {
        if is_fresh(record.fetched_at_ms) {
            return record.models;
        }
        // Answer from the stale cache and refresh it for the next caller,
        // so no request waits on the catalog after the first fetch.
        if let Some(path) = cache_path
            && MODEL_CATALOG_REFRESH_IN_FLIGHT
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let api = Arc::clone(api);
            tokio::spawn(async move {
                match api.model_ids().await {
                    Ok(models) => write_model_catalog(path, models).await,
                    Err(error) => log::warn!("Background model catalog refresh failed: {error}"),
                }
                MODEL_CATALOG_REFRESH_IN_FLIGHT.store(false, Ordering::Relaxed);
            });
        }
        return record.models;
    }
    match api.model_ids().await {
        Ok(models) => {
            if let Some(path) = cache_path {
                write_model_catalog(path, models.clone()).await;
            }
            models
        }
        Err(error) => {
            log::warn!("Failed to fetch the Maple Agent model catalog: {error}");
            Vec::new()
        }
    }
}

async fn write_model_catalog(path: PathBuf, models: Vec<String>) {
    let record = CachedModelCatalog {
        fetched_at_ms: u64::try_from(unix_ms()).unwrap_or(u64::MAX),
        models,
    };
    let Ok(text) = serde_json::to_string(&record) else {
        return;
    };
    let written = tokio::task::spawn_blocking(move || write_model_catalog_file(&path, &text))
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
    if let Err(error) = written {
        log::warn!("Failed to write the model catalog cache: {error}");
    }
}

fn write_model_catalog_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

impl AgentRuntimeHandle {
    async fn api(&self) -> Result<Arc<MapleApiSession>, String> {
        Ok(Arc::clone(&self.runtime().await?.api))
    }

    /// The models a task may pick, the runtime's default first.
    pub async fn available_model_ids(&self) -> Result<Vec<String>, String> {
        let runtime = self.runtime().await?;
        let mut models = cached_model_ids(self.paths(), &self.user_id, &runtime.api).await;
        // A request can outlive a sign-out; answer only for this account.
        self.verify_generation().await?;
        let default_model = runtime.model.clone();
        let mut seen = HashSet::new();
        models.retain(|model| {
            selectable_agent_model_id(model)
                && !model.trim().is_empty()
                && model != &default_model
                && seen.insert(model.clone())
        });
        models.insert(0, default_model);
        Ok(models)
    }

    /// A model's context window from the live catalog, an alias resolved to
    /// its target. `None` when the catalog does not say.
    pub async fn context_limit_for_model(&self, model_id: &str) -> Result<Option<i64>, String> {
        if model_id.trim().is_empty() {
            return Ok(None);
        }
        let catalog = match self.api().await?.model_catalog().await {
            Ok(catalog) => catalog,
            Err(error) => {
                log::warn!("Failed to fetch the Maple Agent model catalog: {error}");
                return Ok(None);
            }
        };
        Ok(catalog_entry(&catalog, model_id)
            .and_then(|entry| entry.context_window)
            .and_then(|window| i64::try_from(window).ok()))
    }

    /// Whether the catalog marks a model, or an alias's target, as seeing
    /// images. `None` when the catalog does not say.
    pub async fn model_supports_vision(&self, model_id: &str) -> Result<Option<bool>, String> {
        if model_id.trim().is_empty() {
            return Ok(None);
        }
        let catalog = match self.api().await?.model_catalog().await {
            Ok(catalog) => catalog,
            Err(error) => {
                log::warn!("Failed to fetch the Maple Agent model catalog: {error}");
                return Ok(None);
            }
        };
        Ok(catalog_entry(&catalog, model_id).and_then(|entry| entry.vision))
    }

    /// Which voice endpoints the account's models offer: a `whisper` model
    /// transcribes, a `tts` model speaks.
    pub async fn audio_capabilities(&self) -> Result<AudioCapabilities, String> {
        let models = self.api().await?.model_ids().await?;
        let has = |marker: &str| {
            models
                .iter()
                .any(|model| model.to_ascii_lowercase().contains(marker))
        };
        Ok(AudioCapabilities {
            transcription: has("whisper"),
            speech: has("tts"),
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
