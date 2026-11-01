//! Composer thinking controls backed by Pi's session and account defaults.

use pi_coding_agent::AgentSession;

use super::config::{load_agent_config_inner, save_agent_config_inner};
use super::{AgentRuntimeHandle, AgentThinkingState, ThinkingLevel};

impl AgentRuntimeHandle {
    /// The task's session on `model_id`, or `None` while a started task is
    /// on another model: it switches with its next run.
    async fn thinking_session(
        &self,
        session_id: &str,
        model_id: &str,
    ) -> Result<Option<AgentSession>, String> {
        let runtime = self.runtime().await?;
        let loaded = runtime.loaded_session(session_id).await;
        if let Some(session) = &loaded
            && session.model().is_some_and(|model| model.id == model_id)
        {
            return Ok(loaded);
        }
        let row = runtime
            .store
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        let current = match &loaded {
            Some(session) => session.model().map(|model| model.id),
            None => row.model.clone(),
        };
        if row.message_count > 0 && current.is_some_and(|current| current != model_id) {
            return Ok(None);
        }
        let model = runtime.pi_model(model_id, None, None).await;
        runtime.task_session(&row, model).await.map(Some)
    }

    /// A task reads its persisted Pi level. A new-task composer previews
    /// the saved default, or Pi's Medium default, clamped by its model. A
    /// task previews another model with the level a switch gives it: the
    /// saved default, as Pi's switch takes its default.
    pub async fn thinking_state(
        &self,
        session_id: Option<&str>,
        model_id: &str,
    ) -> Result<AgentThinkingState, String> {
        if let Some(session_id) = session_id {
            self.verify_generation().await?;
            let runtime = self.runtime().await?;
            let (task_model, task_level) = match runtime.loaded_session(session_id).await {
                Some(session) => {
                    let model = session.model().ok_or("No model selected")?;
                    if model.id == model_id {
                        return Ok(AgentThinkingState {
                            levels: model.thinking_levels(),
                            current: session.thinking_level(),
                        });
                    }
                    (Some(model.id), Some(session.thinking_level()))
                }
                None => {
                    let store = self.store()?;
                    let facts = store
                        .store()
                        .task_facts(store.account(), session_id)
                        .map_err(|error| error.to_string())?;
                    let saved = facts
                        .thinking_level
                        .as_deref()
                        .map(|value| {
                            serde_json::from_value::<ThinkingLevel>(serde_json::Value::String(
                                value.to_owned(),
                            ))
                        })
                        .transpose()
                        .map_err(|_| "Invalid saved thinking level".to_string())?;
                    (facts.model, saved)
                }
            };
            let default = self.load_config().await?.default_thinking_level;
            let level = if task_model.as_deref().is_none_or(|model| model == model_id) {
                task_level.or(default)
            } else {
                default.or(task_level)
            };
            let model = runtime.pi_model(model_id, None, None).await;
            return Ok(AgentThinkingState {
                levels: model.thinking_levels(),
                current: model.clamp_thinking_level(level.unwrap_or(ThinkingLevel::Medium)),
            });
        }
        let runtime = self.runtime().await?;
        let model = runtime.pi_model(model_id, None, None).await;
        let default = self
            .load_config()
            .await?
            .default_thinking_level
            .unwrap_or(ThinkingLevel::Medium);
        Ok(AgentThinkingState {
            levels: model.thinking_levels(),
            current: model.clamp_thinking_level(default),
        })
    }

    /// Pi records the task's selection. Save the actual clamped level for
    /// future tasks without overwriting other account settings.
    pub async fn set_thinking_level(
        &self,
        session_id: Option<&str>,
        model_id: &str,
        level: ThinkingLevel,
    ) -> Result<AgentThinkingState, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session = match session_id {
            Some(session_id) => self.thinking_session(session_id, model_id).await?,
            None => None,
        };
        let state = if let Some(session) = session {
            session.set_thinking_level(level).await;
            AgentThinkingState {
                levels: session
                    .model()
                    .ok_or("No model selected")?
                    .thinking_levels(),
                current: session.thinking_level(),
            }
        } else {
            // A new task, or a task's next model, which its switch gives the
            // default saved below.
            let runtime = self.runtime().await?;
            let model = runtime.pi_model(model_id, None, None).await;
            AgentThinkingState {
                levels: model.thinking_levels(),
                current: model.clamp_thinking_level(level),
            }
        };
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        config.default_thinking_level = Some(state.current);
        save_agent_config_inner(self.paths(), &self.user_id, &config)
            .map_err(|error| error.to_string())?;
        if let Some(session_id) = session_id {
            super::emit_agent_event(
                &self.service.host.events,
                super::AgentServiceEvent::ThinkingChanged {
                    session_id: session_id.into(),
                    state: state.clone(),
                },
            );
        }
        Ok(state)
    }
}
