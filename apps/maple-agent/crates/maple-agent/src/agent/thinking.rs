//! Composer thinking controls backed by Pi's sessions, the catalog's
//! recommended levels and the account's New Task pick.

use pi_coding_agent::AgentSession;

use super::catalog::save_new_task_pick;
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

    /// A task reads its persisted Pi level; one that never recorded a
    /// level previews the level a new task starts at. A new-task composer
    /// previews that level: the one picked on New Task with this model,
    /// else the catalog's recommendation. A task previews another model
    /// with the level a switch gives it: one picked for it while the task
    /// waits, else the recommendation, never the current model's level.
    pub async fn thinking_state(
        &self,
        session_id: Option<&str>,
        model_id: &str,
    ) -> Result<AgentThinkingState, String> {
        let Some(session_id) = session_id else {
            let runtime = self.runtime().await?;
            let (model, recommended) = runtime.thinking_model(model_id).await;
            return Ok(AgentThinkingState {
                levels: model.thinking_levels(),
                current: runtime.new_task_level(&model, recommended),
                recommended: Some(recommended),
            });
        };
        self.verify_generation().await?;
        let runtime = self.runtime().await?;
        let (task_model, task_level) = match runtime.loaded_session(session_id).await {
            Some(session) => {
                let model = session.model().ok_or("No model selected")?;
                if model.id == model_id {
                    return Ok(AgentThinkingState {
                        levels: model.thinking_levels(),
                        current: session.thinking_level(),
                        recommended: Some(runtime.recommended_level(&model).await),
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
        let (model, recommended) = runtime.thinking_model(model_id).await;
        let current = if task_model.as_deref().is_none_or(|model| model == model_id) {
            match task_level {
                Some(level) => model.clamp_thinking_level(level),
                None => runtime.new_task_level(&model, recommended),
            }
        } else {
            runtime
                .next_level(session_id, model_id)
                .map_or(recommended, |level| model.clamp_thinking_level(level))
        };
        Ok(AgentThinkingState {
            levels: model.thinking_levels(),
            current,
            recommended: Some(recommended),
        })
    }

    /// Pi records a task's selection; a pick for a task's next model waits
    /// for its switch. A pick on New Task (no task) is remembered with its
    /// model for the next new task, replacing the previous pair; a task's
    /// pick leaves that pair as it is.
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
        let runtime = self.runtime().await?;
        let state = if let Some(session) = &session {
            session.set_thinking_level(level).await;
            let model = session.model().ok_or("No model selected")?;
            AgentThinkingState {
                levels: model.thinking_levels(),
                current: session.thinking_level(),
                recommended: Some(runtime.recommended_level(&model).await),
            }
        } else {
            let (model, recommended) = runtime.thinking_model(model_id).await;
            AgentThinkingState {
                levels: model.thinking_levels(),
                current: model.clamp_thinking_level(level),
                recommended: Some(recommended),
            }
        };
        self.verify_generation().await?;
        match session_id {
            Some(session_id) => {
                if session.is_none() {
                    // The task's next model, which takes it as it switches.
                    runtime.set_next_level(session_id, model_id, state.current);
                }
                super::emit_agent_event(
                    &self.service.host.events,
                    super::AgentServiceEvent::ThinkingChanged {
                        session_id: session_id.into(),
                        state: state.clone(),
                    },
                );
            }
            None => {
                let _settings = self.lock_settings().await;
                save_new_task_pick(self.paths(), &self.user_id, model_id, state.current)?;
            }
        }
        Ok(state)
    }
}
