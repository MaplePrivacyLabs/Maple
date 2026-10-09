//! Which external agents a task may use: those switched on in Settings that
//! the user also switched on for the task, in a desktop task. Each task
//! starts with none; a choice applies from the task's next run.

use serde_json::Value;

use super::PROVIDERS;
use crate::agent::store::{TaskKind, TaskRow};
use crate::agent::{
    AgentIntegration, AgentIntegrationAvailability, AgentRuntimeHandle,
    AgentSessionIntegrationKind, AgentSessionMcpServer,
};

/// The task setting that lists the external agents switched on for a task.
const TASK_AGENTS: &str = "externalAgents";

fn chosen_providers(row: &TaskRow) -> Vec<String> {
    row.settings
        .get(TASK_AGENTS)
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn set_chosen_providers(row: &mut TaskRow, providers: Vec<String>) {
    if !row.settings.is_object() {
        row.settings = Value::Object(Default::default());
    }
    row.settings[TASK_AGENTS] = Value::from(providers);
}

/// The providers a task's tools may use: switched on in Settings and for
/// the task, in a desktop task.
pub(in crate::agent) fn task_providers(settings_on: &[String], row: &TaskRow) -> Vec<String> {
    if row.kind != TaskKind::Desktop {
        return Vec::new();
    }
    let chosen = chosen_providers(row);
    PROVIDERS
        .iter()
        .filter(|provider| {
            settings_on.iter().any(|on| on == *provider)
                && chosen.iter().any(|chosen| chosen == *provider)
        })
        .map(|provider| provider.to_string())
        .collect()
}

/// A task's rows in the MCP menu for the external agents switched on in
/// Settings, given their cards; none outside a desktop task.
pub(in crate::agent) fn session_rows(
    cards: &[AgentIntegration],
    row: &TaskRow,
) -> Vec<AgentSessionMcpServer> {
    if row.kind != TaskKind::Desktop {
        return Vec::new();
    }
    let chosen = chosen_providers(row);
    cards
        .iter()
        .filter(|card| card.is_external_agent() && card.enabled_for_new_tasks)
        .map(|card| AgentSessionMcpServer {
            name: card.id.clone(),
            kind: AgentSessionIntegrationKind::ExternalAgent,
            display_name: card.name.clone(),
            description: card.description.clone(),
            transport: "external_agent".to_string(),
            enabled: chosen.contains(&card.id),
            available: card.availability == AgentIntegrationAvailability::Available,
        })
        .collect()
}

impl AgentRuntimeHandle {
    /// Switch an external agent on or off for a task, from its next run.
    /// Switching one on needs it on in Settings and installed. Returns the
    /// task as it is now, and the external agents' cards.
    pub(in crate::agent) async fn set_task_external_agent(
        &self,
        session_id: &str,
        provider: &str,
        enabled: bool,
    ) -> Result<(TaskRow, Vec<AgentIntegration>), String> {
        let runtime = self.runtime().await?;
        if runtime.runs.is_running(session_id) {
            return Err("Stop the running agent before changing MCP servers".to_string());
        }
        if !PROVIDERS.contains(&provider) {
            return Err("Unknown external agent integration".to_string());
        }
        let store = self.store()?;
        let row = store
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        if row.kind != TaskKind::Desktop {
            return Err("External agents are available only in desktop tasks".to_string());
        }
        let cards = self.external_agent_cards().await?;
        if enabled {
            let card = cards
                .iter()
                .find(|card| card.id == provider)
                .ok_or_else(|| {
                    "Enable this integration in Settings before selecting it for this task"
                        .to_string()
                })?;
            if card.availability != AgentIntegrationAvailability::Available {
                return Err(card.detail.clone().unwrap_or_else(|| {
                    "Set up this integration in Settings before enabling it".to_string()
                }));
            }
            if let Err(error) = super::sync_skills(self.paths(), &self.user_id, true) {
                log::warn!("Failed to install the external agent skills: {error}");
            }
        }
        let row = store
            .update(session_id, |row| {
                let mut chosen = chosen_providers(row);
                chosen.retain(|chosen| chosen != provider);
                if enabled {
                    chosen.push(provider.to_string());
                }
                set_chosen_providers(row, chosen);
            })?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        Ok((row, cards))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: TaskKind, chosen: &[&str]) -> TaskRow {
        let mut row = TaskRow::new(
            "task".to_string(),
            "Task".to_string(),
            "/tmp".to_string(),
            kind,
            None,
            0,
        );
        set_chosen_providers(
            &mut row,
            chosen.iter().map(|provider| provider.to_string()).collect(),
        );
        row
    }

    #[test]
    fn a_task_uses_the_agents_on_in_settings_and_for_the_task() {
        let on = vec!["codex".to_string(), "claude".to_string()];
        assert_eq!(
            task_providers(&on, &row(TaskKind::Desktop, &["claude"])),
            ["claude"]
        );
        assert_eq!(
            task_providers(&["codex".to_string()], &row(TaskKind::Desktop, &["claude"])),
            Vec::<String>::new()
        );
        assert_eq!(
            task_providers(&on, &row(TaskKind::Desktop, &["claude", "codex", "other"])),
            ["codex", "claude"]
        );
        assert!(task_providers(&on, &row(TaskKind::Acp, &["codex"])).is_empty());
        assert!(task_providers(&on, &row(TaskKind::Desktop, &[])).is_empty());
    }

    #[test]
    fn rows_show_the_agents_on_in_settings_for_desktop_tasks() {
        let card = |id: &str, on: bool, availability| AgentIntegration {
            id: id.to_string(),
            name: id.to_uppercase(),
            description: format!("{id} card"),
            availability,
            backend: None,
            version: None,
            permissions: None,
            setup_available: false,
            enabled_for_new_tasks: on,
            detail: None,
        };
        let cards = vec![
            card("cua-driver", true, AgentIntegrationAvailability::Available),
            card("codex", true, AgentIntegrationAvailability::NotDetected),
            card("claude", true, AgentIntegrationAvailability::Available),
        ];
        let rows = session_rows(&cards, &row(TaskKind::Desktop, &["claude"]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "codex");
        assert!(!rows[0].enabled && !rows[0].available);
        assert_eq!(rows[1].name, "claude");
        assert!(rows[1].enabled && rows[1].available);
        assert_eq!(rows[1].kind, AgentSessionIntegrationKind::ExternalAgent);
        assert_eq!(rows[1].transport, "external_agent");
        assert!(session_rows(&cards, &row(TaskKind::Acp, &["claude"])).is_empty());
        let off = vec![card(
            "codex",
            false,
            AgentIntegrationAvailability::Available,
        )];
        assert!(session_rows(&off, &row(TaskKind::Desktop, &["codex"])).is_empty());
    }
}
