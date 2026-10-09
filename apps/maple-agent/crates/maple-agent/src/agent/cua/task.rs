//! Built-in CUA in one task: whether the task has it on, its row in the
//! task's MCP menu, and its binding while the task's session is loaded.
//!
//! A task records its own choice once CUA is set up on the device, and
//! keeps it when Settings changes the default for new tasks. A task with no
//! choice has CUA off.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pi_coding_agent::ModelRegistry;
use pi_coding_agent::extensions::{
    BeforeAgentStart, BeforeAgentStartResult, Extension, ExtensionContext, SessionShutdown,
    extension,
};
use serde_json::Value;

use super::{INSTRUCTIONS, INSTRUCTIONS_SECTION, missing_permission_message};
use crate::agent::integrations::{
    CUA_CARD_DESCRIPTION, CUA_CARD_NAME, CUA_INTEGRATION_ID, cua_default, is_cua_identity,
};
use crate::agent::store::{TaskKind, TaskRow};
use crate::agent::{AgentRuntimeHandle, AgentSessionIntegrationKind, AgentSessionMcpServer};

/// The task setting that records the task's choice.
const TASK_CHOICE: &str = "computerUse";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The task's choice of built-in CUA, if it made one.
pub(in crate::agent) fn task_choice(row: &TaskRow) -> Option<bool> {
    row.settings.get(TASK_CHOICE).and_then(Value::as_bool)
}

pub(in crate::agent) fn set_task_choice(row: &mut TaskRow, enabled: bool) {
    if !row.settings.is_object() {
        row.settings = Value::Object(Default::default());
    }
    row.settings[TASK_CHOICE] = Value::Bool(enabled);
}

/// What a new desktop task records once CUA is set up on the device: on
/// when the composer names it, or, when the composer names no servers, the
/// device's default for new tasks. Nothing otherwise.
pub(in crate::agent) fn choice_for_new_task(
    device: Option<bool>,
    requested: Option<&[String]>,
) -> Option<bool> {
    let device = device?;
    Some(requested.map_or(device, |names| {
        names.iter().any(|name| is_cua_identity(name))
    }))
}

/// A desktop task's row for built-in CUA: on when the task chose it, and
/// available while CUA can run here. It reads as not set up until CUA is
/// set up on the device or the task chose it.
pub(in crate::agent) fn session_row(
    row: &TaskRow,
    device: Option<bool>,
    ready: bool,
) -> Option<AgentSessionMcpServer> {
    if row.kind != TaskKind::Desktop {
        return None;
    }
    let choice = task_choice(row);
    let set_up = choice.is_some() || device.is_some();
    Some(AgentSessionMcpServer {
        name: CUA_INTEGRATION_ID.to_string(),
        kind: AgentSessionIntegrationKind::Mcp,
        display_name: CUA_CARD_NAME.to_string(),
        description: CUA_CARD_DESCRIPTION.to_string(),
        transport: if set_up { "embedded" } else { "unconfigured" }.to_string(),
        enabled: set_up && choice == Some(true),
        available: set_up && ready,
    })
}

/// A loaded task's built-in CUA: its binding to the task's CUA session, and
/// the tools that call through it. A Pi extension of the task's session
/// tells the model how to use them and ends the binding with the session.
pub(crate) struct TaskCua {
    account_scope: String,
    session_id: String,
    /// Describes screenshots for a model without vision.
    models: ModelRegistry,
    context: OnceLock<ExtensionContext>,
    bound: Mutex<Bound>,
    /// One binding change at a time.
    changing: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Bound {
    #[cfg(embedded_cua)]
    binding: Option<Arc<super::driver::Binding>>,
    /// The tools registered for the binding.
    tools: Vec<String>,
}

impl TaskCua {
    pub(crate) fn new(account_scope: &str, session_id: &str, models: ModelRegistry) -> Arc<Self> {
        Arc::new(Self {
            account_scope: account_scope.to_string(),
            session_id: session_id.to_string(),
            models,
            context: OnceLock::new(),
            bound: Mutex::new(Bound::default()),
            changing: tokio::sync::Mutex::new(()),
        })
    }

    /// The extension that gives the session the instructions while the task
    /// is bound, and ends the binding when the session shuts down.
    pub(crate) fn extension(self: &Arc<Self>) -> Arc<dyn Extension> {
        let cua = Arc::clone(self);
        extension("maple-cua", move |api| {
            let _ = cua.context.set(api.context());
            let prompt = Arc::clone(&cua);
            api.on(move |event: BeforeAgentStart, _context| {
                let bound = prompt.is_bound();
                async move {
                    let mut options = event.options;
                    if bound {
                        options.sections.insert(
                            INSTRUCTIONS_SECTION.to_string(),
                            format!(
                                "## Computer use ({}*)\n\n{INSTRUCTIONS}",
                                super::TOOL_PREFIX
                            ),
                        );
                    } else {
                        options.sections.shift_remove(INSTRUCTIONS_SECTION);
                    }
                    Ok(BeforeAgentStartResult {
                        options: Some(options),
                        ..BeforeAgentStartResult::default()
                    })
                }
            });
            let shutdown = Arc::clone(&cua);
            api.on(move |_event: SessionShutdown, _context| {
                let shutdown = Arc::clone(&shutdown);
                async move {
                    shutdown.unbind().await;
                    Ok(())
                }
            });
        })
    }

    /// Whether the task has CUA's tools now.
    pub(crate) fn is_bound(&self) -> bool {
        !lock(&self.bound).tools.is_empty()
    }

    /// Bind the task to its CUA session anew, which renews its authorization,
    /// and give the session the tools, for a model with or without `vision`.
    /// A failure leaves the task without them.
    pub(crate) async fn bind(&self, vision: bool) -> Result<(), String> {
        let _changing = self.changing.lock().await;
        self.release();
        #[cfg(embedded_cua)]
        {
            use pi_agent_core::AgentTool;

            let context = self
                .context
                .get()
                .ok_or_else(|| "The task's session is not open".to_string())?;
            let binding = Arc::new(
                super::driver::Binding::open(&self.account_scope, &self.session_id).await?,
            );
            let mut tools = Vec::with_capacity(binding.catalog.len());
            for tool in binding.catalog.iter() {
                let tool = super::driver::CuaTool::new(
                    tool,
                    Arc::clone(&binding),
                    vision,
                    self.models.clone(),
                    self.session_id.clone(),
                );
                tools.push(tool.name().to_string());
                context.register_tool(Arc::new(tool), Default::default(), true);
            }
            log::info!(
                "Built-in CUA bound with {} tools for task {}",
                tools.len(),
                self.session_id
            );
            let mut bound = lock(&self.bound);
            bound.binding = Some(binding);
            bound.tools = tools;
            Ok(())
        }
        #[cfg(not(embedded_cua))]
        {
            let _ = (vision, &self.account_scope, &self.session_id, &self.models);
            Err(super::NOT_ON_THIS_PLATFORM.to_string())
        }
    }

    /// Take the tools away and close the binding.
    pub(crate) async fn unbind(&self) {
        let _changing = self.changing.lock().await;
        self.release();
    }

    fn release(&self) {
        let bound = std::mem::take(&mut *lock(&self.bound));
        if let Some(context) = self.context.get() {
            for name in &bound.tools {
                context.unregister_tool(name);
            }
        }
        #[cfg(embedded_cua)]
        if let Some(binding) = bound.binding {
            binding.close();
        }
    }
}

impl AgentRuntimeHandle {
    /// Switch built-in CUA on or off for a desktop task. A loaded task binds
    /// at once, or keeps it off with the reason; the next run binds again
    /// with the run's model. Returns the task as it is now.
    pub(in crate::agent) async fn set_task_cua(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<TaskRow, String> {
        let runtime = self.runtime().await?;
        if runtime.runs.is_running(session_id) {
            return Err("Stop the running agent before changing MCP servers".to_string());
        }
        let store = self.store()?;
        let row = store
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        if row.kind != TaskKind::Desktop {
            return Err(
                "Built-in CUA is available only to tasks running in the Maple desktop app"
                    .to_string(),
            );
        }
        let not_found = || format!("Failed to find Agent task {session_id}");
        let loaded = runtime.loaded_cua(session_id).await;
        if !enabled {
            let row = store
                .update(session_id, |row| set_task_choice(row, false))?
                .ok_or_else(not_found)?;
            if let Some(cua) = loaded {
                cua.unbind().await;
            }
            return Ok(row);
        }
        if task_choice(&row).is_none() && cua_default(self.paths(), &self.user_id).is_none() {
            return Err(
                "Set up built-in CUA in Settings > Integrations before enabling it for a task"
                    .to_string(),
            );
        }
        match super::readiness() {
            Some(readiness) if !readiness.permissions.ready() => {
                return Err(missing_permission_message(&readiness.permissions));
            }
            Some(_) => {}
            #[cfg(not(embedded_cua))]
            None => return Err(super::NOT_ON_THIS_PLATFORM.to_string()),
            #[cfg(embedded_cua)]
            None => {}
        }
        // Bound without vision until the next run knows the model: a
        // screenshot is then described rather than lost.
        if let Some(cua) = &loaded {
            cua.bind(false)
                .await
                .map_err(|error| format!("Failed to start built-in CUA: {error}"))?;
        }
        match store.update(session_id, |row| set_task_choice(row, true)) {
            Ok(Some(row)) => Ok(row),
            failed => {
                if let Some(cua) = &loaded {
                    cua.unbind().await;
                }
                failed?.ok_or_else(not_found)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: TaskKind, choice: Option<bool>) -> TaskRow {
        let mut row = TaskRow::new(
            "task".to_string(),
            "Task".to_string(),
            "/tmp".to_string(),
            kind,
            None,
            0,
        );
        if let Some(choice) = choice {
            set_task_choice(&mut row, choice);
        }
        row
    }

    #[test]
    fn a_new_task_records_the_composers_choice_or_the_device_default() {
        let named = ["cua-driver".to_string()];
        let others = ["github".to_string()];
        // Not set up on the device: nothing to record.
        assert_eq!(choice_for_new_task(None, Some(&named)), None);
        assert_eq!(choice_for_new_task(None, None), None);
        // The composer named its servers: on exactly when CUA is among them,
        // under any of its names.
        assert_eq!(choice_for_new_task(Some(false), Some(&named)), Some(true));
        assert_eq!(
            choice_for_new_task(Some(false), Some(&["Computer use (CUA)".to_string()])),
            Some(true)
        );
        assert_eq!(choice_for_new_task(Some(true), Some(&others)), Some(false));
        // It named none: the device's default for new tasks.
        assert_eq!(choice_for_new_task(Some(true), None), Some(true));
        assert_eq!(choice_for_new_task(Some(false), None), Some(false));
    }

    #[test]
    fn the_row_follows_the_tasks_choice_the_device_and_readiness() {
        let unset = session_row(&row(TaskKind::Desktop, None), None, true).unwrap();
        assert_eq!(unset.name, "cua-driver");
        assert_eq!(unset.display_name, "Cua");
        assert_eq!(unset.kind, AgentSessionIntegrationKind::Mcp);
        assert_eq!(unset.transport, "unconfigured");
        assert!(!unset.enabled && !unset.available);

        let off = session_row(&row(TaskKind::Desktop, None), Some(true), true).unwrap();
        assert_eq!(off.transport, "embedded");
        assert!(!off.enabled && off.available);

        let on = session_row(&row(TaskKind::Desktop, Some(true)), None, false).unwrap();
        assert!(on.enabled && !on.available);
        assert_eq!(on.transport, "embedded");

        assert!(session_row(&row(TaskKind::Acp, Some(true)), Some(true), true).is_none());
    }
}
