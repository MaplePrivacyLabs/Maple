//! Selected directory conventions from `config.ts`; identity and environment are host-owned.
use crate::utils::paths::{PathInputOptions, normalize_path};
use indexmap::IndexMap;
use pi_ai::utils::uuid::UuidV7Generator;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub package_name: String,
    pub app_name: String,
    pub app_title: String,
    pub version: String,
    pub config_dir_name: String,
    pub home_dir: PathBuf,
    pub process_cwd: PathBuf,
    pub environment: IndexMap<String, String>,
    pub agent_dir: Option<PathBuf>,
    /// Share wherever upstream would share its loaded UUID module.
    pub uuid_generator: Arc<UuidV7Generator>,
}
impl HostConfig {
    pub fn new(
        app_name: impl Into<String>,
        config_dir_name: impl Into<String>,
        home_dir: impl Into<PathBuf>,
        process_cwd: impl Into<PathBuf>,
    ) -> Self {
        let app_name = app_name.into();
        Self {
            package_name: app_name.clone(),
            app_title: app_name.clone(),
            version: "0.0.0".into(),
            app_name,
            config_dir_name: config_dir_name.into(),
            home_dir: home_dir.into(),
            process_cwd: process_cwd.into(),
            environment: IndexMap::new(),
            agent_dir: None,
            uuid_generator: Arc::new(UuidV7Generator::new()),
        }
    }
    pub fn maple(home_dir: impl Into<PathBuf>, process_cwd: impl Into<PathBuf>) -> Self {
        Self::new("maple", ".maple", home_dir, process_cwd)
    }
    pub fn env_agent_dir(&self) -> String {
        format!("{}_CODING_AGENT_DIR", self.app_name.to_uppercase())
    }
    pub fn env_session_dir(&self) -> String {
        format!("{}_CODING_AGENT_SESSION_DIR", self.app_name.to_uppercase())
    }
    pub fn expand_tilde_path(&self, path: &str) -> Result<String, String> {
        normalize_path(
            path,
            &PathInputOptions {
                home_dir: Some(self.home_dir.to_string_lossy().into()),
                ..Default::default()
            },
        )
    }
    pub fn get_agent_dir(&self) -> Result<PathBuf, String> {
        if let Some(dir) = &self.agent_dir {
            return Ok(dir.clone());
        }
        if let Some(dir) = self
            .environment
            .get(&self.env_agent_dir())
            .filter(|v| !v.is_empty())
        {
            return normalize_path(
                dir,
                &PathInputOptions {
                    home_dir: Some(self.home_dir.to_string_lossy().into()),
                    ..Default::default()
                },
            )
            .map(PathBuf::from);
        }
        Ok(self.home_dir.join(&self.config_dir_name).join("agent"))
    }
    pub fn get_settings_path(&self) -> Result<PathBuf, String> {
        Ok(self.get_agent_dir()?.join("settings.json"))
    }
    pub fn get_prompts_dir(&self) -> Result<PathBuf, String> {
        Ok(self.get_agent_dir()?.join("prompts"))
    }
    pub fn get_sessions_dir(&self) -> Result<PathBuf, String> {
        Ok(self.get_agent_dir()?.join("sessions"))
    }
}
