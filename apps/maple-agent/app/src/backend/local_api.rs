//! Account-owned preferences; the independent HTTP service never borrows the
//! desktop SDK session or keeps account/store guards during inference streams.
use super::LocalBackend;
use crate::local_api::{LocalApiConfig, LocalApiStatus};
use std::future::Future;

const SETTING: &str = "local_api.config";

#[derive(Default)]
pub(super) struct Control {
    epoch: u64,
    suspended: bool,
}

impl LocalBackend {
    #[cfg(test)]
    pub(crate) fn local_api_suspended_for_test(&self) -> bool {
        self.local_api_control.lock().unwrap().suspended
    }
    pub(crate) fn local_api_control_epoch(&self) -> u64 {
        self.local_api_control.lock().unwrap().epoch
    }
    pub async fn get_local_api_status(&self) -> Result<LocalApiStatus, String> {
        Ok(self.local_api.status())
    }

    /// Runtime stop remains available even when the encrypted preferences cannot
    /// be read. The settings switch uses set_local_api_config to persist Off.
    pub async fn stop_local_api(&self) -> Result<LocalApiStatus, String> {
        {
            let mut control = self.local_api_control.lock().unwrap();
            control.epoch = control.epoch.wrapping_add(1);
            control.suspended = true;
        }
        Ok(self.local_api.stop().await)
    }

    pub(crate) async fn local_api_while_current<T>(
        &self,
        generation: u64,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let mut changes = self.local_api.subscribe();
        tokio::pin!(operation);
        loop {
            if self.local_api.generation() != generation {
                return Err("Local API operation was cancelled. Try again.".into());
            }
            tokio::select! {
                biased;
                changed = changes.changed() => {
                    if changed.is_err() { return Err("Local API is unavailable".into()); }
                }
                result = &mut operation => return result,
            }
        }
    }

    pub async fn set_local_api_config(
        &self,
        user_id: &str,
        config: LocalApiConfig,
    ) -> Result<LocalApiStatus, String> {
        config.validate()?;
        // Serialize durable settings edits, without preventing an independent
        // Stop/logout from cancelling an authentication or disk wait.
        let control_epoch = self.local_api_control_epoch();
        let generation = self.local_api.generation();
        let _settings = self.local_api_settings_gate.lock().await;
        if self.local_api.generation() != generation {
            return Err("Local API operation was cancelled. Try again.".into());
        }
        let (store, account) = self.presentation_account(user_id)?;
        // Only a deliberate On clears the pause. Off must stay latched even
        // if its write fails and the previous durable setting still says On.
        // Apply this before awaits so a later Stop cannot be overwritten.
        {
            let mut control = self.local_api_control.lock().unwrap();
            if control.epoch != control_epoch {
                return Err("Local API operation was cancelled. Try again.".into());
            }
            control.epoch = control.epoch.wrapping_add(1);
            control.suspended = !config.enabled;
        }
        if config.enabled {
            self.local_api_while_current(generation, async {
                self.session_for(user_id).await.map(|_| ())
            })
            .await?;
        } else {
            // Turn off network access even if the subsequent durable write fails.
            self.local_api.stop().await;
        }
        let generation = if config.enabled {
            generation
        } else {
            self.local_api.generation()
        };
        if self.local_api.generation() != generation {
            return Err("Local API operation was cancelled. Try again.".into());
        }
        let saved = config.clone();
        tokio::task::spawn_blocking(move || {
            store
                .set_account_setting(
                    account,
                    SETTING,
                    &serde_json::to_value(saved).map_err(|_| "Invalid Local API settings")?,
                    crate::settings::timestamp_ms(),
                )
                .map_err(|_| "Could not save Local API settings".to_string())
        })
        .await
        .map_err(|_| "Could not save Local API settings".to_string())??;
        self.authorize_account(user_id)?;
        self.local_api
            .configure_if_current(config, user_id.to_owned(), generation)
            .await
    }

    /// Called only under the host's short admission/account guard after a
    /// verified restore or sign-in. It never performs account validation itself.
    pub(crate) async fn resume_local_api(&self, generation: u64) -> Result<(), String> {
        if self.local_api.generation() != generation
            || self.local_api_control.lock().unwrap().suspended
        {
            return Ok(());
        }
        let Some(user_id) = self.saved_user_id() else {
            return Ok(());
        };
        if self
            .local_api_while_current(generation, async {
                self.auth.session_for(&user_id).await.map(|_| ())
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        let (store, account) = self.presentation_account(&user_id)?;
        let config = tokio::task::spawn_blocking(move || {
            let config = store
                .account_setting(account, SETTING)
                .map_err(|_| "Could not read Local API settings")?;
            match config {
                Some(value) => serde_json::from_value::<LocalApiConfig>(value)
                    .map_err(|_| "Saved Local API settings are invalid".to_string()),
                None => Ok(LocalApiConfig::default()),
            }
        })
        .await
        .map_err(|_| "Could not read Local API settings".to_string())??;
        self.authorize_account(&user_id)?;
        self.local_api
            .configure_if_current(config, user_id, generation)
            .await?;
        Ok(())
    }
}
