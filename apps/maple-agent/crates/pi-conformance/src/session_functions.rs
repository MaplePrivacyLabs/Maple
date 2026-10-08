//! Input restoration and observations for the source-recorded session matrices.
//! Only calls selected Pi functions; harness failures are not Pi observations.
use pi_agent_core::types::{AgentMessage, AgentMessageValue};
use pi_ai::{
    env::PiEnv,
    types::{JsObject, JsString, JsValue, Usage},
    utils::{
        js_json::stringify,
        js_value::{from_js_value, to_js_value},
        json_parse::parse_json_utf16,
    },
};
use pi_coding_agent::{
    config::HostConfig,
    core::{
        messages as messages_api, session_cwd, session_export::serialize_session_branch,
        session_manager::*, usage_totals,
    },
    utils::text,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, sync::Arc};
pub type ReplayResult<T> = Result<T, String>;
pub const FUNCTION_IDS: &[&str] = &[
    "session.parseAndMigrate",
    "session.project",
    "session-manager.buildSessionContext",
    "session.entryToMessages",
    "session.assertValidId",
    "session.inMemory",
    "messages.convertToLlm",
    "messages.construct",
    "messages.bashExecutionToText",
    "usage.totals",
    "session.cwdFormatting",
    "text.bom",
];
fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    JsObject::from(entries).into()
}
fn encoded(value: &impl Serialize) -> ReplayResult<JsValue> {
    to_js_value(value).map_err(|e| e.to_string())
}
fn typed<T: DeserializeOwned>(value: &JsValue) -> ReplayResult<T> {
    from_js_value(value.clone()).map_err(|e| e.to_string())
}
fn required<'a>(input: &'a JsValue, key: &str) -> ReplayResult<&'a JsValue> {
    input.get(key).ok_or_else(|| format!("Fixture lacks {key}"))
}
fn field<T: DeserializeOwned>(input: &JsValue, key: &str) -> ReplayResult<T> {
    typed(required(input, key)?)
}
fn optional<T: DeserializeOwned>(input: &JsValue, key: &str) -> ReplayResult<Option<T>> {
    input
        .get(key)
        .filter(|v| !v.is_null())
        .map(typed)
        .transpose()
}
fn raw(input: &JsValue, key: &str) -> ReplayResult<JsValue> {
    if let Some(encoded) = input.get(format!("{key}Json").as_str()) {
        parse_json_utf16(
            encoded
                .as_js_str()
                .ok_or_else(|| format!("{key}Json is not a string"))?,
        )
        .map_err(|e| e.to_string())
    } else {
        required(input, key).cloned()
    }
}
fn strings(entries: &[SessionEntry]) -> JsValue {
    entries
        .iter()
        .map(|e| stringify(&e.value()).into())
        .collect::<Vec<JsValue>>()
        .into()
}
fn value(output: impl Serialize) -> ReplayResult<JsValue> {
    Ok(object([("value", encoded(&output)?)]))
}
fn absent() -> JsValue {
    JsObject::new().into()
}
fn error(error: SessionError) -> JsValue {
    object([
        ("error", error.message.into()),
        ("errorClass", error.name.into()),
    ])
}
fn invocation<T: Serialize>(result: SessionResult<T>) -> ReplayResult<JsValue> {
    match result {
        Ok(value_) => value(value_),
        Err(e) => Ok(error(e)),
    }
}
fn void_invocation(result: SessionResult<()>) -> ReplayResult<JsValue> {
    Ok(match result {
        Ok(()) => absent(),
        Err(e) => error(e),
    })
}
fn maybe_invocation<T: Serialize>(result: SessionResult<Option<T>>) -> ReplayResult<JsValue> {
    match result {
        Ok(Some(result)) => value(result),
        Ok(None) => Ok(absent()),
        Err(e) => Ok(error(e)),
    }
}
fn snapshot(value_: &JsValue) -> ReplayResult<JsValue> {
    pi_ai::utils::json_parse::parse_json(&stringify(value_)).map_err(|e| e.to_string())
}
fn raw_message(value: JsValue) -> ReplayResult<AgentMessage> {
    value
        .as_object()
        .cloned()
        .map(messages_api::raw_message)
        .ok_or_else(|| "Fixture message is not an object".into())
}
fn options(value: Option<&JsValue>) -> ReplayResult<Option<NewSessionOptions>> {
    value
        .map(|v| {
            Ok(NewSessionOptions {
                id: optional(v, "id")?,
                parent_session: optional(v, "parentSession")?,
            })
        })
        .transpose()
}
fn target(
    operation: &JsValue,
    key: &str,
    aliases: &BTreeMap<JsString, JsString>,
) -> ReplayResult<JsString> {
    let value: JsString = field(operation, key)?;
    Ok(aliases.get(&value).cloned().unwrap_or(value))
}
fn optional_target(
    operation: &JsValue,
    key: &str,
    aliases: &BTreeMap<JsString, JsString>,
) -> ReplayResult<Option<JsString>> {
    if operation.get(key).is_none_or(JsValue::is_null) {
        Ok(None)
    } else {
        target(operation, key, aliases).map(Some)
    }
}
fn observe_manager(manager: &SessionManager) -> ReplayResult<JsValue> {
    let mut entries = vec![];
    if let Some(header) = manager.get_header() {
        entries.push(header);
    }
    entries.extend(manager.get_entries());
    let serialized = entries
        .iter()
        .map(|entry| stringify(&entry.value()))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut state = JsObject::from([
        ("header", encoded(&manager.get_header())?),
        ("entries", encoded(&manager.get_entries())?),
        ("leafId", encoded(&manager.get_leaf_id())?),
        ("sessionId", manager.get_session_id().into()),
        ("persisted", manager.is_persisted().into()),
        ("count", (manager.get_entry_count() as f64).into()),
        ("branch", encoded(&manager.get_branch(None))?),
        ("tree", encoded(&manager.get_tree())?),
        ("projection", encoded(&manager.build_session_projection())?),
        ("serialized", serialized.into()),
    ]);
    if let Some(path) = manager.get_session_file() {
        state.insert("sessionFile", path.into());
    }
    if let Some(name) = manager.get_session_name() {
        state.insert("name", name.into());
    }
    Ok(state.into())
}
fn mutate_message(entry: &SessionEntry, fields: JsObject) -> ReplayResult<AgentMessage> {
    let message = entry.message().ok_or("Mutation fixture has no message")?;
    message.update(|message| match message {
        AgentMessageValue::Custom(raw) => {
            raw.update(|raw| raw.extend(fields));
            Ok(())
        }
        _ => Err("Mutation fixture message is not a raw object".to_owned()),
    })?;
    Ok(message)
}
fn memory(input: &JsValue, env: Arc<dyn PiEnv>, config: Arc<HostConfig>) -> ReplayResult<JsValue> {
    let entries = if input.get("entries").is_some() || input.get("entriesJson").is_some() {
        Some(typed(&raw(input, "entries")?)?)
    } else {
        None
    };
    let cwd: Option<String> = optional(input, "cwd")?;
    let mut manager = match SessionManager::in_memory(
        cwd.as_deref(),
        options(input.get("options"))?,
        entries,
        env,
        config,
    ) {
        Ok(m) => m,
        Err(e) => return Ok(error(e)),
    };
    let mut aliases = BTreeMap::new();
    let mut results = vec![];
    for operation in required(input, "operations")?
        .as_array()
        .ok_or("operations must be an array")?
    {
        let result = match required(operation, "operation")?
            .as_str()
            .ok_or("operation must be a string")?
        {
            "appendMessage" => {
                invocation(manager.append_message(raw_message(raw(operation, "message")?)?))?
            }
            "appendThinkingLevelChange" => invocation(
                manager
                    .append_thinking_level_change(field::<JsString>(operation, "thinkingLevel")?),
            )?,
            "appendModelChange" => invocation(manager.append_model_change(
                field::<JsString>(operation, "provider")?,
                field::<JsString>(operation, "modelId")?,
            ))?,
            "appendCustomEntry" => invocation(manager.append_custom_entry(
                field::<JsString>(operation, "customType")?,
                operation.get("data").cloned(),
            ))?,
            "appendCustomMessageEntry" => invocation(manager.append_custom_message_entry(
                field::<JsString>(operation, "customType")?,
                required(operation, "content")?.clone(),
                field(operation, "display")?,
                operation.get("details").cloned(),
            ))?,
            "appendContextEdit" => invocation(manager.append_context_edit(
                &target(operation, "target", &aliases)?,
                required(operation, "replacement")?.clone(),
            ))?,
            "appendLabelChange" => invocation(manager.append_label_change(
                &target(operation, "target", &aliases)?,
                optional(operation, "label")?,
            ))?,
            "appendSessionInfo" => {
                invocation(manager.append_session_info(field::<JsString>(operation, "name")?))?
            }
            "appendUsage" => invocation(manager.append_usage(
                field::<JsString>(operation, "kind")?,
                field::<JsString>(operation, "provider")?,
                field::<JsString>(operation, "model")?,
                field(operation, "usage")?,
                optional(operation, "note")?,
            ))?,
            "appendCompaction" => invocation(manager.append_compaction_raw_tokens(
                field::<JsString>(operation, "summary")?,
                optional_target(operation, "firstKept", &aliases)?,
                required(operation, "tokensBefore")?.as_f64().unwrap_or(0.0),
                operation.get("details").cloned(),
                optional(operation, "fromHook")?,
                optional(operation, "usage")?,
                Some(required(operation, "tokensBefore")?.clone()),
            ))?,
            "branchWithSummary" => invocation(manager.branch_with_summary(
                optional_target(operation, "branchFrom", &aliases)?,
                field::<JsString>(operation, "summary")?,
                operation.get("details").cloned(),
                optional(operation, "fromHook")?,
                optional(operation, "usage")?,
            ))?,
            "branch" => void_invocation(manager.branch(&target(operation, "target", &aliases)?))?,
            "resetLeaf" => {
                manager.reset_leaf();
                absent()
            }
            "createBranchedSession" => maybe_invocation(
                manager.create_branched_session(&target(operation, "target", &aliases)?),
            )?,
            "newSession" => {
                maybe_invocation(manager.new_session(options(operation.get("options"))?))?
            }
            "mutateEntry" => {
                let entry = manager
                    .get_entry(&target(operation, "target", &aliases)?)
                    .ok_or("Mutation fixture has no entry")?;
                let fields: JsObject = field(operation, "fields")?;
                entry.update(|raw| raw.extend(fields));
                value(entry)?
            }
            "mutateMessage" => {
                let entry = manager
                    .get_entry(&target(operation, "target", &aliases)?)
                    .ok_or("Mutation fixture has no entry")?;
                value(mutate_message(&entry, field(operation, "fields")?)?)?
            }
            "serializeBranch" => {
                let mut calls = vec![];
                let trailing: Option<Vec<JsObject>> = operation
                    .get("trailing")
                    .map(|value| {
                        value
                            .as_array()
                            .ok_or("trailing fixture must be an array")?
                            .iter()
                            .map(|entry| {
                                entry
                                    .as_object()
                                    .cloned()
                                    .ok_or("trailing fixture entry must be an object".to_owned())
                            })
                            .collect::<ReplayResult<Vec<_>>>()
                    })
                    .transpose()?;
                let mut callback = |parent: Option<&JsString>, timestamp: &JsString| {
                    calls.push(object([
                        (
                            "parentId",
                            parent
                                .cloned()
                                .map(JsValue::String)
                                .unwrap_or(JsValue::Null),
                        ),
                        ("timestamp", timestamp.clone().into()),
                    ]));
                    trailing
                        .as_ref()
                        .expect("callback exists only with validated trailing entries")
                        .iter()
                        .map(|entry| {
                            let mut entry = entry.clone();
                            entry.insert(
                                "parentId",
                                parent
                                    .cloned()
                                    .map(JsValue::String)
                                    .unwrap_or(JsValue::Null),
                            );
                            entry.insert("timestamp", timestamp.clone().into());
                            entry.into()
                        })
                        .collect()
                };
                let jsonl = serialize_session_branch(
                    &manager,
                    trailing.as_ref().map(|_| {
                        &mut callback
                            as &mut pi_coding_agent::core::session_export::TrailingEntries<'_>
                    }),
                );
                value(object([
                    ("jsonl", jsonl.into()),
                    ("callbackCalls", calls.into()),
                ]))?
            }
            unknown => return Err(format!("Unknown session operation {unknown}")),
        };
        if let Some(alias) = operation.get("alias").and_then(JsValue::as_js_str)
            && let Some(value) = result.get("value")
        {
            let id = value
                .as_js_str()
                .or_else(|| value.get("id").and_then(JsValue::as_js_str))
                .ok_or("Alias operation returned no ID")?;
            aliases.insert(alias.clone(), id.clone());
        }
        results.push(snapshot(&object([
            ("result", result),
            ("state", observe_manager(&manager)?),
        ]))?);
    }
    value(object([
        ("results", results.into()),
        (
            "aliases",
            JsValue::Object(
                aliases
                    .into_iter()
                    .map(|(key, value)| (key, value.into()))
                    .collect(),
            ),
        ),
    ]))
}
pub fn dispatch(
    id: &str,
    input: &JsValue,
    env: Arc<dyn PiEnv>,
    config: Arc<HostConfig>,
) -> ReplayResult<JsValue> {
    match id {
        "session.parseAndMigrate" => {
            let mut entries = parse_session_entries(&field(input, "content")?);
            let before = strings(&entries);
            if input.get("migrate").and_then(JsValue::as_bool) == Some(true)
                && let Err(error_) = migrate_session_entries(&mut entries, env.as_ref())
            {
                return Ok(error(error_));
            }
            let serialized = entries
                .iter()
                .map(|entry| stringify(&entry.value()))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            value(object([
                ("before", before),
                ("entries", encoded(&entries)?),
                ("serialized", serialized.into()),
            ]))
        }
        "session-manager.buildSessionContext" => {
            let entries: Vec<SessionEntry> = typed(&raw(input, "entries")?)?;
            let leaf = input.get("leafId").map(|value| value.as_js_str());
            value(build_session_context(&entries, leaf, None))
        }
        "session.project" => {
            let entries: Vec<SessionEntry> = typed(&raw(input, "entries")?)?;
            let leaf = input.get("leafId").map(|value| value.as_js_str());
            let projection = build_session_projection(&entries, leaf, None);
            let indices = projection
                .entries
                .iter()
                .map(|projected| {
                    entries
                        .iter()
                        .position(|entry| entry.ptr_eq(&projected.source_entry))
                        .map(|i| i as f64)
                        .unwrap_or(-1.0)
                        .into()
                })
                .collect::<Vec<JsValue>>();
            let original = projection
                .messages
                .iter()
                .map(|message| {
                    entries
                        .iter()
                        .position(|entry| {
                            entry.kind() == "message"
                                && entry.message().is_some_and(|input| input.ptr_eq(message))
                        })
                        .map(|i| i as f64)
                        .unwrap_or(-1.0)
                        .into()
                })
                .collect::<Vec<JsValue>>();
            let output = JsObject::from([
                (
                    "contextEntries",
                    encoded(&build_context_entries(&entries, leaf, None))?,
                ),
                ("projection", encoded(&projection)?),
                (
                    "context",
                    encoded(&build_session_context(&entries, leaf, None))?,
                ),
                ("sourceIndices", indices.into()),
                ("originalMessages", original.into()),
                ("rawEntriesAfter", strings(&entries)),
                (
                    "latestCompaction",
                    encoded(&get_latest_compaction_entry(&entries))?,
                ),
            ]);
            value(output)
        }
        "session.entryToMessages" => {
            let entry: SessionEntry = typed(&raw(input, "entry")?)?;
            value(object([
                (
                    "messages",
                    encoded(&session_entry_to_context_messages(&entry))?,
                ),
                ("rawEntryAfter", stringify(&entry.value()).into()),
            ]))
        }
        "session.assertValidId" => void_invocation(assert_valid_session_id(&field(input, "id")?)),
        "session.inMemory" => memory(input, env, config),
        "messages.convertToLlm" => {
            let messages = raw(input, "messages")?
                .as_array()
                .ok_or("messages must be an array")?
                .iter()
                .cloned()
                .map(raw_message)
                .collect::<ReplayResult<Vec<_>>>()?;
            let output = messages_api::convert_to_llm(&messages);
            let indices = output
                .iter()
                .map(|message| {
                    let message = AgentMessage::from(message.clone());
                    messages
                        .iter()
                        .position(|input| input.ptr_eq(&message))
                        .map(|i| i as f64)
                        .unwrap_or(-1.0)
                        .into()
                })
                .collect::<Vec<JsValue>>();
            let serialized = output
                .iter()
                .map(|message| encoded(message).map(|value| stringify(&value).into()))
                .collect::<ReplayResult<Vec<JsValue>>>()?;
            let inputs = messages
                .iter()
                .map(|message| stringify(&messages_api::message_value(message)).into())
                .collect::<Vec<JsValue>>();
            value(object([
                ("messages", encoded(&output)?),
                ("inputIndices", indices.into()),
                ("rawInputsAfter", inputs.into()),
                ("serialized", serialized.into()),
            ]))
        }
        "messages.bashExecutionToText" => value(messages_api::bash_execution_to_text(&typed(
            &raw(input, "message")?,
        )?)),
        "messages.construct" => {
            let timestamp: JsString = field(input, "timestamp")?;
            let message = match required(input, "operation")?.as_str() {
                Some("branch") => messages_api::create_branch_summary_message(
                    field(input, "summary")?,
                    field(input, "fromId")?,
                    &timestamp,
                ),
                Some("compaction") => messages_api::create_compaction_summary_message_raw_tokens(
                    field(input, "summary")?,
                    input.get("tokensBefore").cloned(),
                    &timestamp,
                ),
                Some("custom") => messages_api::create_custom_message(
                    field(input, "customType")?,
                    required(input, "content")?.clone(),
                    field(input, "display")?,
                    input.get("details").cloned(),
                    &timestamp,
                ),
                _ => return Err("Unknown message construction operation".into()),
            };
            value(message)
        }
        "usage.totals" => {
            let usages: Vec<Usage> = field(input, "usages")?;
            let mut totals = usage_totals::create_usage_totals();
            for usage in &usages {
                usage_totals::add_usage_to_totals(&mut totals, usage);
            }
            let entries: Vec<SessionEntry> = typed(&raw(input, "entries")?)?;
            let mut output = JsObject::from([
                ("totals", encoded(&totals)?),
                (
                    "breakdown",
                    encoded(&usage_totals::get_usage_cost_breakdown(&entries))?,
                ),
            ]);
            if usages.len() == 2 {
                output.insert(
                    "combined",
                    encoded(&usage_totals::combine_usage(&usages[0], &usages[1]))?,
                );
            }
            value(output)
        }
        "session.cwdFormatting" => {
            let issue: session_cwd::SessionCwdIssue = field(input, "issue")?;
            let error = session_cwd::MissingSessionCwdError {
                issue: issue.clone(),
            };
            value(object([
                (
                    "error",
                    session_cwd::format_missing_session_cwd_error(&issue).into(),
                ),
                (
                    "prompt",
                    session_cwd::format_missing_session_cwd_prompt(&issue).into(),
                ),
                (
                    "exception",
                    object([
                        ("name", error.name().into()),
                        ("message", error.to_string().into()),
                        ("issue", encoded(&error.issue)?),
                    ]),
                ),
            ]))
        }
        "text.bom" => {
            let input: JsString = field(input, "text")?;
            let (bom, body) = text::split_bom(&input);
            value(object([
                (
                    "split",
                    object([("bom", bom.into()), ("text", body.into())]),
                ),
                ("stripped", text::strip_bom(&input).into()),
            ]))
        }
        _ => Err(format!("Unsupported session function {id}")),
    }
}
