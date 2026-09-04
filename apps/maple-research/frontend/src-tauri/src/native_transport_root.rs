use crate::open_secret_config::normalize_api_url;
use maple_sdk::TransportV2CacheNamespaceRoot;
use std::collections::HashMap;
use tauri::State;
use tokio::sync::RwLock;

/// Process-private registry for the installation-scoped Transport V2 cache root.
///
/// The renderer owns durable storage. Native clients resolve the root through
/// this registry so it never becomes part of proxy configuration, status,
/// events, or native persistence.
pub struct TransportRootState {
    roots: RwLock<HashMap<String, TransportV2CacheNamespaceRoot>>,
}

impl TransportRootState {
    pub fn new() -> Self {
        Self {
            roots: RwLock::new(HashMap::new()),
        }
    }

    async fn install(&self, api_url: &str, root_base64: &str) -> Result<(), String> {
        let api_origin = normalize_api_url(api_url)?;
        let candidate = TransportV2CacheNamespaceRoot::from_base64(root_base64)
            .map_err(|_| "Native Transport V2 cache root is invalid".to_string())?;
        let mut roots = self.roots.write().await;
        match roots.get(&api_origin) {
            Some(installed) if installed == &candidate => Ok(()),
            Some(_) => Err(
                "A different Transport V2 cache root is already installed for this API origin"
                    .to_string(),
            ),
            None => {
                roots.insert(api_origin, candidate);
                Ok(())
            }
        }
    }

    pub async fn require(&self, api_url: &str) -> Result<TransportV2CacheNamespaceRoot, String> {
        let api_origin = normalize_api_url(api_url)?;
        self.roots
            .read()
            .await
            .get(&api_origin)
            .cloned()
            .ok_or_else(|| {
                "Transport V2 cache root is not installed for this API origin".to_string()
            })
    }

    pub async fn require_exact(
        &self,
        api_url: &str,
        root_base64: &str,
    ) -> Result<TransportV2CacheNamespaceRoot, String> {
        let installed = self.require(api_url).await?;
        let supplied = TransportV2CacheNamespaceRoot::from_base64(root_base64)
            .map_err(|_| "Native Transport V2 cache root is invalid".to_string())?;
        if supplied != installed {
            return Err(
                "Transport V2 cache root does not match the installed root for this API origin"
                    .to_string(),
            );
        }
        Ok(installed)
    }
}

#[tauri::command]
pub async fn install_native_transport_root(
    state: State<'_, TransportRootState>,
    api_url: String,
    root_base64: String,
) -> Result<(), String> {
    state.install(&api_url, &root_base64).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const ROOT_B: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

    #[tokio::test]
    async fn first_root_wins_and_identical_reinstallation_is_idempotent() {
        let state = TransportRootState::new();
        state
            .install("https://API.example.test/", ROOT_A)
            .await
            .unwrap();
        state
            .install("https://api.example.test", ROOT_A)
            .await
            .unwrap();

        assert!(state
            .install("https://api.example.test", ROOT_B)
            .await
            .is_err());
        assert_eq!(
            state
                .require("https://api.example.test")
                .await
                .unwrap()
                .to_base64(),
            ROOT_A
        );
    }

    #[tokio::test]
    async fn roots_are_isolated_by_canonical_api_origin() {
        let state = TransportRootState::new();
        state
            .install("https://one.example.test", ROOT_A)
            .await
            .unwrap();
        state
            .install("https://two.example.test", ROOT_B)
            .await
            .unwrap();

        assert!(state
            .require_exact("https://one.example.test", ROOT_A)
            .await
            .is_ok());
        assert!(state
            .require_exact("https://one.example.test", ROOT_B)
            .await
            .is_err());
        assert!(state.require("https://missing.example.test").await.is_err());
    }

    #[tokio::test]
    async fn invalid_origins_and_noncanonical_roots_are_rejected() {
        let state = TransportRootState::new();
        assert!(state
            .install("http://api.example.test", ROOT_A)
            .await
            .is_err());
        assert!(state
            .install("https://api.example.test", ROOT_A.trim_end_matches('='))
            .await
            .is_err());
    }
}
