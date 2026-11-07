//! MCP editor state contains names and presence only until the user enters a
//! replacement. Keep, Set and Clear remain distinct at the request boundary.

use crate::ui::i18n::{tr, tr_args};
use gpui::{Context, Div, Entity, div, prelude::*};
use maple_agent::store::records::{
    McpDefinition, McpSecretEdit, McpSecretUpdate, McpServerEdit, SecretString,
};
use zeroize::Zeroizing;

use super::{SettingsScreen, TextInput, pill_button, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum McpSecretAction {
    Keep,
    Set,
    Clear,
}

pub(super) struct McpSecretField {
    pub name: String,
    pub has_value: bool,
    pub action: McpSecretAction,
    pub input: Entity<TextInput>,
}

impl SettingsScreen {
    pub(super) fn render_mcp_secret_fields(
        &self,
        fields: &[McpSecretField],
        headers: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(fields.iter().enumerate().map(|(index, field)| {
                let mut row = div().flex().flex_col().gap_1().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().text_sm().child(field.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(match field.action {
                                    McpSecretAction::Keep if field.has_value => {
                                        tr("settings-mcp-saved-value")
                                    }
                                    McpSecretAction::Keep => tr("settings-mcp-no-saved-value"),
                                    McpSecretAction::Set => tr("settings-mcp-new-value"),
                                    McpSecretAction::Clear => tr("settings-mcp-will-clear"),
                                }),
                        ),
                );
                row = row.child(
                    div().flex().gap_2().children(
                        [
                            (McpSecretAction::Keep, "Keep", tr("settings-mcp-keep")),
                            (McpSecretAction::Set, "Replace", tr("settings-mcp-replace")),
                            (McpSecretAction::Clear, "Clear", tr("settings-mcp-clear")),
                        ]
                        .into_iter()
                        .map(|(action, id, label)| {
                            pill_button(
                                format!(
                                    "mcp-secret-{}-{index}-{id}",
                                    if headers { "header" } else { "env" }
                                ),
                                label,
                                field.action == action,
                                cx.listener(move |this, _event, _window, cx| {
                                    if let Some(editor) = &mut this.mcp_editor {
                                        let fields = if headers {
                                            &mut editor.saved_headers
                                        } else {
                                            &mut editor.saved_environment
                                        };
                                        if let Some(field) = fields.get_mut(index) {
                                            field.action = action;
                                            if action != McpSecretAction::Set {
                                                field.input.update(cx, |input, cx| input.clear(cx));
                                            }
                                        }
                                    }
                                    cx.notify();
                                }),
                            )
                        }),
                    ),
                );
                if field.action == McpSecretAction::Set {
                    row = row.child(field.input.clone());
                }
                row
            }))
    }
}

pub(super) fn definition_edit(definition: &McpDefinition) -> McpServerEdit {
    let keep = |presence: &maple_agent::store::records::McpSecretPresence| McpSecretEdit {
        name: presence.name.clone(),
        update: McpSecretUpdate::Keep,
    };
    McpServerEdit {
        key: definition.key.clone(),
        name: definition.name.clone(),
        description: definition.description.clone(),
        enabled: definition.enabled,
        timeout_seconds: definition.timeout_seconds,
        target: definition.target.clone(),
        environment: definition.environment.iter().map(keep).collect(),
        headers: definition.headers.iter().map(keep).collect(),
    }
}

pub(super) fn name_collides(servers: &[McpDefinition], original: Option<&str>, name: &str) -> bool {
    let key = maple_agent::agent::AgentMcpServer::key_for_name(name);
    servers
        .iter()
        .any(|server| server.key == key && Some(server.name.as_str()) != original)
}

pub(super) fn field_edits(
    fields: &[McpSecretField],
    cx: &Context<SettingsScreen>,
) -> Result<Vec<McpSecretEdit>, String> {
    fields
        .iter()
        .map(|field| {
            let update = match field.action {
                McpSecretAction::Keep => McpSecretUpdate::Keep,
                McpSecretAction::Clear => McpSecretUpdate::Clear,
                McpSecretAction::Set => {
                    let value = Zeroizing::new(field.input.read(cx).text());
                    if value.is_empty() {
                        return Err(tr_args(
                            "settings-mcp-secret-required",
                            [("arg1", (field.name).to_string().into())],
                        )
                        .to_string());
                    }
                    McpSecretUpdate::Set(SecretString::new(value.to_string()))
                }
            };
            Ok(McpSecretEdit {
                name: field.name.clone(),
                update,
            })
        })
        .collect()
}

/// New entries always mean Set. Blank fields mean no new entries; malformed
/// entries never disappear silently and errors never quote the supplied value.
pub(super) fn new_value_edits(text: &str) -> Result<Vec<McpSecretEdit>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    text.split(';')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let (name, value) = entry.trim().split_once('=').ok_or_else(|| {
                tr("settings-mcp-enter-new-values-as-name-value-separated-by-semicolons")
                    .to_string()
            })?;
            if name.trim().is_empty() || value.is_empty() {
                return Err(tr("settings-mcp-each-new-entry-needs-a-name-and-value").into());
            }
            Ok(McpSecretEdit {
                name: name.trim().to_string(),
                update: McpSecretUpdate::Set(SecretString::new(value.to_owned())),
            })
        })
        .collect()
}

pub(super) fn require_target_choice(
    original: Option<&McpDefinition>,
    edit: &McpServerEdit,
) -> Result<(), String> {
    let Some(original) = original else {
        return Ok(());
    };
    if original.target == edit.target && original.key == edit.key {
        return Ok(());
    }
    for (saved, edits) in [
        (&original.environment, &edit.environment),
        (&original.headers, &edit.headers),
    ] {
        if saved
            .iter()
            .filter(|presence| presence.has_value)
            .any(|presence| {
                edits
                    .iter()
                    .find(|entry| entry.name.eq_ignore_ascii_case(&presence.name))
                    .is_some_and(|entry| matches!(entry.update, McpSecretUpdate::Keep))
            })
        {
            return Err(tr(
                "settings-mcp-the-server-target-or-name-changed-replace-or-clear-each-saved-val",
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::records::{McpSecretPresence, McpTarget};

    fn saved() -> McpDefinition {
        McpDefinition {
            key: "example".into(),
            name: "Example".into(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 30,
            target: McpTarget::Stdio {
                command: "example --stdio".into(),
            },
            environment: vec![McpSecretPresence {
                name: "TOKEN".into(),
                has_value: true,
            }],
            headers: vec![],
        }
    }

    #[test]
    fn metadata_echo_keeps_saved_values_without_serializing_values() {
        let definition = saved();
        let response = serde_json::to_string(&definition).unwrap();
        assert!(response.contains("hasValue"));
        assert!(!response.contains("synthetic-secret"));
        let edit = definition_edit(&definition);
        assert!(matches!(edit.environment[0].update, McpSecretUpdate::Keep));
        assert!(require_target_choice(Some(&definition), &edit).is_ok());
    }

    #[test]
    fn changed_target_requires_explicit_replacement_or_clear() {
        let definition = saved();
        let mut edit = definition_edit(&definition);
        edit.target = McpTarget::Stdio {
            command: "different --stdio".into(),
        };
        assert!(require_target_choice(Some(&definition), &edit).is_err());
        edit.environment[0].update =
            McpSecretUpdate::Set(SecretString::new("synthetic-new".into()));
        assert!(require_target_choice(Some(&definition), &edit).is_ok());
        edit.environment[0].update = McpSecretUpdate::Clear;
        assert!(require_target_choice(Some(&definition), &edit).is_ok());
        let mut renamed = definition_edit(&definition);
        renamed.key = "renamed".into();
        assert!(require_target_choice(Some(&definition), &renamed).is_err());
    }

    #[test]
    fn new_environment_and_header_values_are_redacted_set_edits() {
        let edits =
            new_value_edits("TOKEN=synthetic-secret; Authorization=Bearer synthetic-header==")
                .unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].name, "TOKEN");
        assert!(
            matches!(&edits[0].update,McpSecretUpdate::Set(value) if value.expose()=="synthetic-secret")
        );
        assert_eq!(edits[1].name, "Authorization");
        assert!(
            matches!(&edits[1].update,McpSecretUpdate::Set(value) if value.expose()=="Bearer synthetic-header==")
        );
        assert!(!format!("{edits:?}").contains("synthetic-secret"));
        assert!(!format!("{edits:?}").contains("synthetic-header"));
        assert!(new_value_edits(" ; ").unwrap().is_empty());
    }

    #[test]
    fn malformed_new_entries_are_rejected_without_echo() {
        for input in [
            "synthetic-secret",
            "TOKEN=synthetic-secret; malformed-synthetic-secret",
            "=synthetic-secret",
            "TOKEN=",
        ] {
            let error = new_value_edits(input).unwrap_err();
            assert!(!error.contains("synthetic-secret"));
        }
    }
}
