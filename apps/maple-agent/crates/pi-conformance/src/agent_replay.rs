//! Replay the agent DSL through the actual Rust Agent and faux provider.
//!
//! Records are snapshots of public callbacks; this interpreter never reads a
//! golden while running and never repairs an observed value to match one.

use pi_agent_core::{
    agent::{Agent, AgentInitialState, AgentOptions},
    types::*,
};
use pi_ai::{
    providers::faux::*,
    types::{Model, Tool},
    utils::js_value::{from_js_value, to_js_value},
};
use pi_testkit::env::VirtualEnv;
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Notify;

pub type ReplayResult<T> = Result<T, String>;
#[derive(Clone, Default, Debug)]
pub struct AgentRecords {
    pub events: Vec<JsValue>,
    pub requests: Vec<JsValue>,
    pub final_record: JsValue,
}
fn object(fields: impl IntoIterator<Item = (impl Into<JsString>, JsValue)>) -> JsValue {
    JsValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
fn snapshot<T: Serialize + ?Sized>(value: &T) -> JsValue {
    to_js_value(value).expect("agent record serialization")
}
fn decode<T: DeserializeOwned>(value: JsValue) -> AgentResult<T> {
    from_js_value(value).map_err(|error| AgentError::new(error.to_string()))
}
fn string(value: &JsValue) -> AgentResult<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| AgentError::new("Expected a script string"))
}
fn entries(value: &JsValue) -> AgentResult<&Vec<JsValue>> {
    value
        .as_array()
        .ok_or_else(|| AgentError::new("Expected a script array"))
}
fn member(value: &JsValue, key: &str) -> AgentResult<JsValue> {
    value
        .get(key)
        .cloned()
        .ok_or_else(|| AgentError::new(format!("Missing script field {key}")))
}
fn merge(target: &mut JsValue, source: JsValue) -> AgentResult<()> {
    let source = source
        .as_object()
        .ok_or_else(|| AgentError::new("Expected an object to merge"))?;
    let target = target
        .as_object_mut()
        .ok_or_else(|| AgentError::new("Expected an object merge target"))?;
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
    Ok(())
}
fn interpolate(value: &JsValue, bindings: &JsValue) -> AgentResult<JsValue> {
    fn lookup(bindings: &JsValue, field: &str) -> AgentResult<JsValue> {
        let mut value = bindings;
        for key in field.split('.') {
            value = value
                .get(key)
                .ok_or_else(|| AgentError::new(format!("Unknown script interpolation {field}")))?;
        }
        Ok(value.clone())
    }
    match value {
        JsValue::String(text) => {
            let units = text.as_utf16();
            let mut result = Vec::new();
            let mut cursor = 0;
            while let Some(offset) = units[cursor..]
                .windows(2)
                .position(|pair| pair == [36, 123])
            {
                let begin = cursor + offset;
                result.extend_from_slice(&units[cursor..begin]);
                let tail = begin + 2;
                let Some(offset) = units[tail..].iter().position(|unit| *unit == 125) else {
                    result.extend_from_slice(&units[begin..]);
                    cursor = units.len();
                    break;
                };
                let end = tail + offset;
                let field = &units[tail..end];
                if field.is_empty()
                    || !field.iter().all(|unit| {
                        *unit <= 127
                            && ((*unit as u8).is_ascii_alphanumeric() || *unit == 95 || *unit == 46)
                    })
                {
                    result.extend_from_slice(&[36, 123]);
                    cursor = tail;
                    continue;
                }
                let field = String::from_utf16(field).expect("ASCII interpolation field");
                let replacement = lookup(bindings, &field)?;
                if begin == 0 && end + 1 == units.len() {
                    return Ok(replacement);
                }
                match replacement {
                    JsValue::String(value) => result.extend(value.units()),
                    value => result.extend(pi_ai::utils::js_json::stringify(&value).encode_utf16()),
                }
                cursor = end + 1;
            }
            result.extend_from_slice(&units[cursor..]);
            Ok(JsString::from_utf16(result).into())
        }
        JsValue::Array(values) => values
            .iter()
            .map(|value| interpolate(value, bindings))
            .collect::<AgentResult<Vec<_>>>()
            .map(JsValue::Array),
        JsValue::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), interpolate(value, bindings)?)))
            .collect::<AgentResult<JsObject>>()
            .map(JsValue::Object),
        _ => Ok(value.clone()),
    }
}

#[derive(Default)]
struct Gate {
    open: std::sync::atomic::AtomicBool,
    notify: Notify,
}
#[derive(Clone, Default)]
struct Gates(Shared<BTreeMap<String, Arc<Gate>>>);
impl Gates {
    fn gate(&self, name: &str) -> Arc<Gate> {
        self.0
            .update(|gates| gates.entry(name.to_owned()).or_default().clone())
    }
    fn open(&self, name: &str) {
        let gate = self.gate(name);
        gate.open.store(true, std::sync::atomic::Ordering::SeqCst);
        gate.notify.notify_waiters();
    }
    fn open_all(&self) {
        for name in self
            .0
            .read(|gates| gates.keys().cloned().collect::<Vec<_>>())
        {
            self.open(&name);
        }
    }
    async fn wait(&self, name: &str, signal: Option<&CancellationToken>) -> AgentResult<()> {
        let gate = self.gate(name);
        loop {
            let notified = gate.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(AgentError::new("Operation aborted"));
            }
            if gate.open.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(signal) = signal {
                tokio::select! { biased; _ = signal.cancelled() => return Err(AgentError::new("Operation aborted")), _ = notified => {} }
            } else {
                notified.await;
            }
        }
    }
}
#[derive(Clone, Default)]
struct Recorder {
    records: Shared<AgentRecords>,
    gates: Gates,
    agent: Shared<Option<Agent>>,
    hook_calls: Shared<BTreeMap<String, usize>>,
}
impl Recorder {
    fn emit(&self, kind: &str, data: JsValue) {
        self.records.update(|records| {
            records.events.push(object([
                ("seq", (records.events.len() as f64).into()),
                ("type", kind.into()),
                ("entries", 0.into()),
                ("data", data),
            ]))
        });
    }
    async fn actions(
        &self,
        behavior: &[JsValue],
        mut context: JsValue,
        args: Option<SharedArgs>,
        signal: Option<CancellationToken>,
        update: Option<AgentToolUpdateCallback>,
    ) -> AgentResult<Option<JsValue>> {
        let mut bindings = context
            .get("args")
            .filter(|args| args.is_object())
            .cloned()
            .unwrap_or_else(|| object([] as [(&str, JsValue); 0]));
        merge(&mut bindings, context.clone())?;
        for action in behavior {
            if let Some(args) = &args {
                context
                    .as_object_mut()
                    .unwrap()
                    .insert("args", args.snapshot());
                bindings
                    .as_object_mut()
                    .unwrap()
                    .insert("args", args.snapshot());
            }
            if let Some(value) = action.get("wait") {
                self.gates
                    .wait(&string(&interpolate(value, &bindings)?)?, signal.as_ref())
                    .await?;
            } else if let Some(value) = action.get("update") {
                update
                    .as_ref()
                    .ok_or_else(|| AgentError::new("Updates require a tool invocation"))?(
                    decode(interpolate(value, &bindings)?)?,
                );
            } else if let Some(value) = action.get("mutateArgs") {
                let value = interpolate(value, &bindings)?;
                args.as_ref()
                    .ok_or_else(|| AgentError::new("No args to mutate"))?
                    .update(|args| merge(args, value))?;
            } else if action.get("abort").is_some() {
                self.agent
                    .read(|agent| agent.as_ref().expect("initialized agent").abort());
            } else if let Some(value) = action.get("throw") {
                return Err(AgentError::new(string(&interpolate(value, &bindings)?)?));
            } else if let Some(value) = action.get("return").or_else(|| action.get("result")) {
                return Ok(Some(interpolate(value, &bindings)?));
            } else {
                return Err(AgentError::new("Unsupported agent script action"));
            }
        }
        Ok(None)
    }
    async fn hook(
        &self,
        name: &str,
        rules: JsValue,
        context: JsValue,
        args: Option<SharedArgs>,
        signal: Option<CancellationToken>,
    ) -> AgentResult<Option<JsValue>> {
        let call = self.hook_calls.update(|calls| {
            let count = calls.entry(name.to_owned()).or_default();
            *count += 1;
            *count
        });
        self.emit(
            "$hook",
            object([
                ("name", name.into()),
                ("call", (call as f64).into()),
                ("context", context.clone()),
            ]),
        );
        let rule = entries(&rules)?.iter().find(|rule| {
            let when = &rule["when"];
            (when
                .get("call")
                .is_none_or(|value| value.as_u64() == Some(call as u64)))
                && (when
                    .get("toolCallId")
                    .is_none_or(|value| context["toolCall"]["id"] == *value))
        });
        match rule {
            Some(rule) => {
                self.actions(entries(&rule["behavior"])?, context, args, signal, None)
                    .await
            }
            None => Ok(None),
        }
    }
}
fn tool_record(tool: &AgentTool) -> JsValue {
    let mut value = snapshot(&tool.tool);
    value
        .as_object_mut()
        .unwrap()
        .insert("label", snapshot(&tool.label));
    if let Some(mode) = &tool.execution_mode {
        value
            .as_object_mut()
            .unwrap()
            .insert("executionMode", snapshot(mode));
    }
    value
}
fn tools_record(tools: &AgentTools) -> JsValue {
    JsValue::Array(tools.read(|tools| tools.iter().map(|tool| tool.read(tool_record)).collect()))
}
fn context_record(context: &AgentContext) -> JsValue {
    let mut value = object([("messages", snapshot(&context.messages))]);
    if let Some(tools) = &context.tools {
        value
            .as_object_mut()
            .unwrap()
            .insert("tools", tools_record(tools));
    }
    value
}
fn turn_record(context: &AgentTurnContext) -> JsValue {
    object([
        ("message", snapshot(&context.message)),
        ("toolResults", snapshot(&context.tool_results)),
        ("context", context_record(&context.context)),
        ("newMessages", snapshot(&context.new_messages)),
    ])
}
fn select_tools(all: &AgentTools, names: &JsValue) -> AgentResult<AgentTools> {
    Ok(Shared::new(
        entries(names)?
            .iter()
            .map(|name| {
                let name = string(name)?;
                all.read(|tools| {
                    tools
                        .iter()
                        .find(|tool| tool.read(|tool| tool.name == name))
                        .cloned()
                })
                .ok_or_else(|| AgentError::new(format!("Unknown scripted tool {name}")))
            })
            .collect::<AgentResult<Vec<_>>>()?,
    ))
}
fn replacement_model(model: &JsValue, base: &Model) -> AgentResult<Shared<Model>> {
    let mut model = model.clone();
    if model["ref"] != JsValue::from("faux-default") {
        return Err(AgentError::new("Unknown scripted model reference"));
    }
    model.as_object_mut().unwrap().remove("ref");
    let mut result = snapshot(base);
    merge(&mut result, model)?;
    decode(result).map(Shared::new)
}
fn replacement_context(value: &JsValue, tools: &AgentTools) -> AgentResult<AgentContext> {
    Ok(AgentContext {
        messages: decode(member(value, "messages")?)?,
        tools: value
            .get("tools")
            .map(|names| select_tools(tools, names))
            .transpose()?,
    })
}
fn update(value: JsValue, tools: &AgentTools, model: &Model) -> AgentResult<AgentLoopTurnUpdate> {
    Ok(AgentLoopTurnUpdate {
        context: value
            .get("context")
            .map(|context| replacement_context(context, tools))
            .transpose()?,
        messages: value
            .get("messages")
            .map(|value| decode(value.clone()))
            .transpose()?,
        model: value
            .get("model")
            .map(|value| replacement_model(value, model))
            .transpose()?,
        thinking_level: value
            .get("thinkingLevel")
            .map(|value| decode(value.clone()))
            .transpose()?,
    })
}
fn matches(kind: &str, data: &JsValue, selector: &JsValue) -> bool {
    selector["type"].as_str() == Some(kind)
        && selector
            .get("toolCallId")
            .is_none_or(|value| *value == data["toolCallId"])
        && selector
            .get("role")
            .is_none_or(|value| *value == data["message"]["role"])
        && selector
            .get("name")
            .is_none_or(|value| *value == data["name"])
        && selector
            .get("assistantMessageEventType")
            .is_none_or(|value| *value == data["assistantMessageEvent"]["type"])
}
fn event_data(event: &AgentEvent) -> JsValue {
    let mut data = snapshot(event);
    data.as_object_mut().unwrap().remove("type");
    data
}
fn record_event(recorder: &Recorder, event: &AgentEvent) {
    let mut data = event_data(event);
    if matches!(event, AgentEvent::MessageUpdate { .. }) {
        let inner = data["assistantMessageEvent"].as_object_mut().unwrap();
        let partial = inner.remove("partial");
        let index = inner.get("contentIndex").and_then(JsValue::as_u64);
        if let (Some(partial), Some(index)) = (partial, index)
            && let Some(block) = partial["content"]
                .as_array()
                .and_then(|content| content.get(index as usize))
        {
            data.as_object_mut().unwrap().insert("block", block.clone());
        }
    }
    recorder.emit(event.kind(), data);
}

fn build_tools(scenario: &JsValue, recorder: &Recorder) -> AgentResult<AgentTools> {
    let mut tools = Vec::new();
    for definition in scenario
        .get("tools")
        .map(entries)
        .transpose()?
        .into_iter()
        .flatten()
    {
        let name = string(&definition["name"])?;
        let run = recorder.clone();
        let behavior = entries(&definition["behavior"])?.clone();
        let tool_name = name.clone();
        let execute: ToolExecute = Arc::new(move |id, args, signal, update| {
            let recorder = run.clone();
            let behavior = behavior.clone();
            let name = tool_name.clone();
            // Invocation is observable synchronously, before the async behavior.
            recorder.emit(
                "$tool_call",
                object([
                    ("toolCallId", snapshot(&id)),
                    ("toolName", name.clone().into()),
                    ("args", args.snapshot()),
                ]),
            );
            Box::pin(async move {
                let context = object([
                    ("args", args.snapshot()),
                    ("toolCallId", snapshot(&id)),
                    ("toolName", name.clone().into()),
                ]);
                let result = recorder
                    .actions(&behavior, context, Some(args), signal, update)
                    .await?
                    .ok_or_else(|| {
                        AgentError::new(format!("Tool {name} has no scripted result"))
                    })?;
                decode(result)
            })
        });
        let prepare_arguments = definition.get("prepareArguments").map(|prepared| {
            let prepared = prepared.clone();
            let recorder = recorder.clone();
            let name = name.clone();
            Arc::new(move |args: SharedArgs| {
                recorder.emit(
                    "$hook",
                    object([
                        ("name", "prepareArguments".into()),
                        ("toolName", name.clone().into()),
                        ("args", args.snapshot()),
                    ]),
                );
                let bindings = object([("args", args.snapshot())]);
                let result = if let Some(value) = prepared.get("return") {
                    interpolate(value, &bindings)?
                } else {
                    let mut result = args.snapshot();
                    merge(&mut result, interpolate(&prepared["set"], &bindings)?)?;
                    result
                };
                Ok(SharedArgs::new(result))
            }) as PrepareArguments
        });
        tools.push(Shared::new(AgentTool {
            tool: Tool {
                name: name.clone().into(),
                description: decode(
                    definition
                        .get("description")
                        .cloned()
                        .unwrap_or_else(|| "".into()),
                )?,
                parameters: decode(member(definition, "parameters")?)?,
                constrained_sampling: None,
            },
            label: decode(
                definition
                    .get("label")
                    .cloned()
                    .unwrap_or_else(|| name.into()),
            )?,
            execute,
            prepare_arguments,
            output_schema: None,
            replay: None,
            execution_mode: definition
                .get("executionMode")
                .map(|value| decode(value.clone()))
                .transpose()?,
        }));
    }
    Ok(Shared::new(tools))
}
fn hooks(
    options: &mut AgentOptions,
    scenario: &JsValue,
    recorder: &Recorder,
    tools: &AgentTools,
    model: &Model,
) {
    if let Some(rules) = scenario["hooks"].get("beforeToolCall") {
        let recorder = recorder.clone();
        let rules = rules.clone();
        options.before_tool_call = Some(Arc::new(move |context, signal| {
            let recorder = recorder.clone();
            let rules = rules.clone();
            let data = object([
                ("assistantMessage", snapshot(&context.assistant_message)),
                ("toolCall", snapshot(&context.tool_call)),
                ("args", context.args.snapshot()),
                ("context", context_record(&context.context)),
            ]);
            Box::pin(async move {
                recorder
                    .hook("beforeToolCall", rules, data, Some(context.args), signal)
                    .await?
                    .map(decode)
                    .transpose()
            })
        }));
    }
    if let Some(rules) = scenario["hooks"].get("afterToolCall") {
        let recorder = recorder.clone();
        let rules = rules.clone();
        options.after_tool_call = Some(Arc::new(move |context, signal| {
            let recorder = recorder.clone();
            let rules = rules.clone();
            let data = object([
                ("assistantMessage", snapshot(&context.assistant_message)),
                ("toolCall", snapshot(&context.tool_call)),
                ("args", context.args.snapshot()),
                ("result", snapshot(&context.result)),
                ("isError", context.is_error.into()),
                ("context", context_record(&context.context)),
            ]);
            Box::pin(async move {
                recorder
                    .hook("afterToolCall", rules, data, Some(context.args), signal)
                    .await?
                    .map(decode)
                    .transpose()
            })
        }));
    }
    if let Some(rules) = scenario["hooks"].get("finishTurn") {
        let recorder = recorder.clone();
        let rules = rules.clone();
        options.finish_turn = Some(Arc::new(move |context, signal| {
            let recorder = recorder.clone();
            let rules = rules.clone();
            let data = turn_record(&context);
            Box::pin(async move {
                recorder
                    .hook("finishTurn", rules, data, None, signal)
                    .await?
                    .map(decode)
                    .transpose()
            })
        }));
    }
    if let Some(rules) = scenario["hooks"].get("prepareRequest") {
        let recorder = recorder.clone();
        let rules = rules.clone();
        let tools = tools.clone();
        let model = model.clone();
        options.prepare_request = Some(Arc::new(move |context, signal| {
            let recorder = recorder.clone();
            let rules = rules.clone();
            let tools = tools.clone();
            let model = model.clone();
            let data = object([
                ("context", context_record(&context.context)),
                ("model", snapshot(&context.model)),
                ("thinkingLevel", snapshot(&context.thinking_level)),
            ]);
            Box::pin(async move {
                recorder
                    .hook("prepareRequest", rules, data, None, signal)
                    .await?
                    .map(|value| {
                        let update = update(value, &tools, &model)?;
                        Ok(AgentRequestUpdate {
                            context: update.context,
                            model: update.model,
                            thinking_level: update.thinking_level,
                        })
                    })
                    .transpose()
            })
        }));
    }
    if let Some(rules) = scenario["hooks"].get("prepareNextTurn") {
        let recorder = recorder.clone();
        let rules = rules.clone();
        let tools = tools.clone();
        let model = model.clone();
        options.prepare_next_turn_with_context = Some(Arc::new(move |context, signal| {
            let recorder = recorder.clone();
            let rules = rules.clone();
            let tools = tools.clone();
            let model = model.clone();
            let data = turn_record(&context);
            Box::pin(async move {
                recorder
                    .hook("prepareNextTurn", rules, data, None, signal)
                    .await?
                    .map(|value| update(value, &tools, &model))
                    .transpose()
            })
        }));
    }
}

/// Drive callbacks on the required current-thread runtime without advancing
/// time. The fixed empty scheduler turns are a deadlock bound, not time steps.
async fn settle(recorder: &Recorder) -> AgentResult<()> {
    // All callbacks owned by these fixtures are synchronous work, explicit
    // gates, or VirtualEnv timers. Drain scheduler work until records stop
    // progressing; none of these scheduler turns changes virtual time.
    for _ in 0..10000 {
        let previous = recorder
            .records
            .read(|records| (records.events.len(), records.requests.len()));
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        if previous
            == recorder
                .records
                .read(|records| (records.events.len(), records.requests.len()))
        {
            return Ok(());
        }
    }
    Err(AgentError::new(
        "Agent callback work exceeded the replay progress bound",
    ))
}
struct Driver {
    recorder: Recorder,
    agent: Agent,
    env: Arc<VirtualEnv>,
    all_tools: AgentTools,
    base_model: Model,
    pending: Option<AgentFuture<AgentResult<()>>>,
    errors: Vec<JsValue>,
}
impl Driver {
    async fn idle(&mut self, auto_advance: bool) -> AgentResult<()> {
        for _ in 0..10000 {
            settle(&self.recorder).await?;
            if !self.agent.state().is_streaming() {
                if let Some(pending) = self.pending.take() {
                    pending.await?;
                }
                self.agent.wait_for_idle().await;
                return Ok(());
            }
            if auto_advance && let Some(delay) = self.env.next_timer_delay_ms() {
                self.env.advance(delay).await;
                continue;
            }
            return Err(AgentError::new(
                "awaitIdle is blocked; open scripted gates or explicitly advance the clock",
            ));
        }
        Err(AgentError::new("Scripted agent did not become idle"))
    }
    async fn wait_for(&self, selector: &JsValue) -> AgentResult<()> {
        let count = selector["count"].as_u64().unwrap_or(1) as usize;
        for _ in 0..10000 {
            let matched = || {
                self.recorder.records.read(|records| {
                    records
                        .events
                        .iter()
                        .filter(|event| {
                            matches(event["type"].as_str().unwrap(), &event["data"], selector)
                        })
                        .count()
                        >= count
                })
            };
            if matched() {
                return Ok(());
            }
            let before = self.recorder.records.read(|records| records.events.len());
            settle(&self.recorder).await?;
            if matched() {
                return Ok(());
            }
            if before == self.recorder.records.read(|records| records.events.len()) {
                return Err(AgentError::new(format!(
                    "waitFor did not match {}; latest event {:?}; open a gate or advance time",
                    pi_ai::utils::js_json::stringify(selector),
                    self.recorder
                        .records
                        .read(|records| records.events.last().cloned())
                )));
            }
        }
        Err(AgentError::new("waitFor exceeded callback bound"))
    }
    fn step<'a>(
        &'a mut self,
        step: &'a JsValue,
        expect_completion: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AgentResult<()>> + 'a>> {
        Box::pin(async move {
            if let Some(prompt) = step.get("prompt") {
                let next = self.agent.prompt(decode::<JsString>(prompt.clone())?);
                if expect_completion {
                    next.await?;
                } else {
                    self.pending = Some(next);
                }
            } else if step.get("continue").is_some() {
                let next = self.agent.continue_run();
                if expect_completion {
                    next.await?;
                } else {
                    self.pending = Some(next);
                }
            } else if let Some(value) = step.get("steer") {
                self.agent.steer(decode(value.clone())?);
            } else if let Some(value) = step.get("followUp") {
                self.agent.follow_up(decode(value.clone())?);
            } else if step.get("abort").is_some() {
                self.agent.abort();
            } else if let Some(value) = step.get("open") {
                self.recorder.gates.open(&string(value)?);
            } else if let Some(value) = step.get("waitFor") {
                self.wait_for(value).await?;
            } else if let Some(value) = step.get("advanceClock") {
                settle(&self.recorder).await?;
                self.env
                    .advance(
                        value
                            .as_u64()
                            .ok_or_else(|| AgentError::new("Clock advance must be an integer"))?,
                    )
                    .await;
                settle(&self.recorder).await?;
            } else if let Some(value) = step.get("awaitIdle") {
                self.idle(value["autoAdvance"].as_bool() == Some(true))
                    .await?;
            } else if let Some(value) = step.get("setActiveTools") {
                self.agent
                    .state()
                    .set_tools(&select_tools(&self.all_tools, value)?);
            } else if let Some(value) = step.get("setThinkingLevel") {
                self.agent
                    .state()
                    .set_thinking_level(decode(value.clone())?);
            } else if let Some(value) = step.get("setModel") {
                self.agent
                    .state()
                    .set_model(replacement_model(value, &self.base_model)?);
            } else if let Some(value) = step.get("expectError") {
                let error = match self.step(&value["step"], true).await {
                    Err(error) => error,
                    Ok(()) => return Err(AgentError::new("Expected scripted API call to throw")),
                };
                if snapshot(&error.message) != value["message"] {
                    return Err(AgentError::new(format!(
                        "Expected API error {}, received {error}",
                        string(&value["message"])?
                    )));
                }
                let data = object([("message", snapshot(&error.message))]);
                self.errors.push(data.clone());
                self.recorder.emit("$api_error", data);
            } else {
                return Err(AgentError::new("Unsupported agent step"));
            }
            Ok(())
        })
    }
}

pub async fn replay(scenario: &JsValue) -> ReplayResult<AgentRecords> {
    replay_inner(scenario)
        .await
        .map_err(|error| error.to_string())
}
async fn replay_inner(scenario: &JsValue) -> AgentResult<AgentRecords> {
    if scenario["layer"] != JsValue::from("agent")
        || scenario["provider"]["kind"] != JsValue::from("faux")
        || scenario["model"]["ref"] != JsValue::from("faux-default")
    {
        return Err(AgentError::new(
            "Agent replay requires the faux agent layer",
        ));
    }
    if scenario
        .get("variants")
        .is_some_and(|value| value.as_array().is_none_or(|array| !array.is_empty()))
    {
        return Err(AgentError::new(
            "Materialize variants as separate scenario inputs",
        ));
    }
    let env = Arc::new(VirtualEnv::new(
        scenario["clock"]["epochMs"]
            .as_i64()
            .ok_or_else(|| AgentError::new("Invalid clock epoch"))?,
    ));
    let recorder = Recorder::default();
    let provider = &scenario["provider"];
    let faux = create_faux_core(
        env.clone(),
        RegisterFauxProviderOptions {
            api: Some("faux".into()),
            provider: Some("faux".into()),
            token_size: Some(FauxTokenSize {
                min: provider["tokenSize"].as_f64(),
                max: provider["tokenSize"].as_f64(),
            }),
            tokens_per_second: provider["tokensPerSecond"].as_f64(),
            ..Default::default()
        },
    );
    let responses = entries(&provider["responses"])?;
    let mut steps = Vec::new();
    for response in responses {
        let recorder = recorder.clone();
        let env = env.clone();
        let response = response.clone();
        steps.push(FauxResponseStep::Factory(Arc::new(
            move |context, options, _state, model| {
                let mut option_record = options
                    .as_ref()
                    .map(snapshot)
                    .unwrap_or_else(|| object([] as [(&str, JsValue); 0]));
                if option_record.get("apiKey").is_some_and(JsValue::is_string) {
                    option_record
                        .as_object_mut()
                        .unwrap()
                        .insert("apiKey", "<apiKey>".into());
                }
                recorder.records.update(|records| {
                    records.requests.push(object([
                        ("call", (records.requests.len() as f64).into()),
                        ("model", snapshot(&model)),
                        ("context", snapshot(&context)),
                        ("options", option_record),
                    ]))
                });
                let response = response.clone();
                let env = env.clone();
                Box::pin(async move {
                    let content: FauxAssistantContent =
                        decode(response["content"].clone()).map_err(|error| error.message)?;
                    let options: FauxAssistantOptions =
                        decode(response).map_err(|error| error.message)?;
                    Ok(faux_assistant_message(env.as_ref(), content, options))
                })
            },
        )));
    }
    faux.set_responses(steps);
    let tools = build_tools(scenario, &recorder)?;
    let mut options = AgentOptions {
        initial_state: Some(AgentInitialState {
            model: Some(Shared::new(faux.get_model().clone())),
            system_prompt: scenario
                .get("systemPrompt")
                .map(|value| decode(value.clone()))
                .transpose()?,
            thinking_level: scenario
                .get("thinkingLevel")
                .map(|value| decode(value.clone()))
                .transpose()?,
            tools: Some(
                scenario
                    .get("activeTools")
                    .map(|names| select_tools(&tools, names))
                    .transpose()?
                    .unwrap_or_else(|| tools.clone()),
            ),
            ..Default::default()
        }),
        steering_mode: scenario
            .get("steeringMode")
            .map(|value| decode(value.clone()))
            .transpose()?,
        follow_up_mode: scenario
            .get("followUpMode")
            .map(|value| decode(value.clone()))
            .transpose()?,
        tool_execution: scenario
            .get("toolExecution")
            .map(|value| decode(value.clone()))
            .transpose()?,
        ..Default::default()
    };
    let provider = faux.clone();
    options.stream_fn = Some(Arc::new(move |model, context, options| {
        let stream = provider.stream_simple(model, context, options);
        Box::pin(async move { Ok(stream) })
    }));
    let transform = recorder.clone();
    options.transform_context = Some(Arc::new(move |messages, _| {
        transform.emit(
            "$hook",
            object([
                ("name", "transformContext".into()),
                ("messages", snapshot(&messages)),
            ]),
        );
        Box::pin(async move { Ok(messages) })
    }));
    let convert = recorder.clone();
    options.convert_to_llm = Some(Arc::new(move |messages| {
        convert.emit(
            "$hook",
            object([
                ("name", "convertToLlm".into()),
                ("messages", snapshot(&messages)),
            ]),
        );
        Box::pin(async move {
            Ok(messages.read(|messages| {
                messages
                    .iter()
                    .map(|message| match message.snapshot() {
                        AgentMessageValue::Custom(raw) => pi_ai::types::Message::Raw(raw),
                        _ => message.as_llm().expect("built-in agent message"),
                    })
                    .collect()
            }))
        })
    }));
    hooks(&mut options, scenario, &recorder, &tools, faux.get_model());
    let agent = Agent::new(options, env.clone())?;
    recorder.agent.update(|slot| *slot = Some(agent.clone()));
    let mut subscriptions = Vec::new();
    for subscriber in scenario
        .get("subscribers")
        .map(entries)
        .transpose()?
        .into_iter()
        .flatten()
    {
        let recorder = recorder.clone();
        let subscriber = subscriber.clone();
        let seen = Shared::new(0u64);
        subscriptions.push(agent.subscribe(Arc::new(move |event, signal| {
            let recorder = recorder.clone();
            let subscriber = subscriber.clone();
            let should_wait = matches(event.kind(), &event_data(&event), &subscriber["when"])
                && seen.update(|seen| {
                    *seen += 1;
                    *seen == subscriber["when"]["count"].as_u64().unwrap_or(1)
                });
            if should_wait {
                recorder.emit(
                    "$hook",
                    object([
                        ("name", "subscriber".into()),
                        ("event", snapshot(&event)),
                        ("gate", subscriber["wait"].clone()),
                    ]),
                );
            }
            Box::pin(async move {
                if should_wait {
                    recorder
                        .gates
                        .wait(&string(&subscriber["wait"])?, Some(&signal))
                        .await?;
                }
                Ok(())
            })
        })));
    }
    let observe = recorder.clone();
    subscriptions.push(agent.subscribe(Arc::new(move |event, _| {
        record_event(&observe, &event);
        Box::pin(async { Ok(()) })
    })));
    let mut driver = Driver {
        recorder: recorder.clone(),
        agent: agent.clone(),
        env: env.clone(),
        all_tools: tools,
        base_model: faux.get_model().clone(),
        pending: None,
        errors: Vec::new(),
    };
    let result = async {
        for step in entries(&scenario["steps"])? {
            recorder.emit("$step", step.clone());
            driver.step(step, false).await?;
        }
        driver.idle(false).await?;
        let call_count = faux.state.snapshot().call_count;
        if faux.get_pending_response_count() != 0
            || call_count != responses.len()
            || recorder.records.read(|records| records.requests.len()) != responses.len()
        {
            return Err(AgentError::new(
                "Scripted faux response count did not match actual requests",
            ));
        }
        let state = agent.state();
        let mut state_record = object([
            ("systemPrompt", snapshot(&state.system_prompt()?)),
            ("thinkingLevel", snapshot(&state.thinking_level())),
            ("tools", tools_record(&state.tools())),
            ("messages", snapshot(&state.messages())),
            ("isStreaming", state.is_streaming().into()),
            ("pendingToolCalls", snapshot(&state.pending_tool_calls())),
        ]);
        if let Some(model) = state.model() {
            state_record
                .as_object_mut()
                .unwrap()
                .insert("model", snapshot(&model));
        }
        if let Some(message) = state.streaming_message() {
            state_record
                .as_object_mut()
                .unwrap()
                .insert("streamingMessage", snapshot(&message));
        }
        if let Some(error) = state.error_message() {
            state_record
                .as_object_mut()
                .unwrap()
                .insert("errorMessage", snapshot(&error));
        }
        recorder.records.update(|records| {
            records.final_record = object([
                ("state", state_record),
                (
                    "queues",
                    object([
                        ("hasQueuedMessages", agent.has_queued_messages().into()),
                        ("next", snapshot(&agent.peek_queued_messages())),
                    ]),
                ),
                ("errors", JsValue::Array(driver.errors.clone())),
            ])
        });
        Ok(recorder.records.snapshot())
    }
    .await;
    agent.abort();
    recorder.gates.open_all();
    let _ = settle(&recorder).await;
    for _ in 0..10000 {
        if !agent.state().is_streaming() {
            break;
        }
        let Some(delay) = env.next_timer_delay_ms() else {
            break;
        };
        env.advance(delay).await;
        let _ = settle(&recorder).await;
    }
    if let Some(pending) = driver.pending.take() {
        let _ = pending.await;
    }
    // Break the callback/Agent ownership cycle after every success or failure.
    for unsubscribe in subscriptions {
        unsubscribe();
    }
    recorder.agent.update(|agent| *agent = None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_preserves_lone_surrogates_in_templates_and_bindings() {
        let bindings = object([
            ("n", 7.into()),
            ("s", JsString::from_utf16(vec![0xd800]).into()),
        ]);
        let template = JsString::from_utf16(vec![0xd800, 32, 36, 123, 110, 125]);
        assert_eq!(
            interpolate(&template.into(), &bindings).unwrap(),
            JsValue::String(JsString::from_utf16(vec![0xd800, 32, 55]))
        );
        assert_eq!(
            interpolate(&"prefix ${s}".into(), &bindings).unwrap(),
            JsValue::String(JsString::from_utf16(
                "prefix ".encode_utf16().chain([0xd800]).collect::<Vec<_>>()
            ))
        );
        assert_eq!(
            interpolate(&"${n}".into(), &bindings).unwrap(),
            JsValue::from(7)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_converter_preserves_custom_messages_in_requests() {
        let scenario = pi_ai::utils::js_value::from_json(r#"{
            "layer":"agent","model":{"ref":"faux-default"},
            "clock":{"epochMs":1767225600000},
            "provider":{"kind":"faux","tokenSize":1000,"responses":[{"content":"ok","stopReason":"stop"}]},
            "steps":[{"steer":{"role":"custom","data":{"retained":true}}},{"prompt":"Hello"},{"awaitIdle":{}}]
        }"#).unwrap();
        let records = replay(&scenario).await.unwrap();
        assert!(
            records.requests[0]["context"]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == JsValue::from("custom")
                    && message["data"]["retained"] == JsValue::from(true))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unexpected_api_success_is_a_replay_error_not_a_panic() {
        let scenario = pi_ai::utils::js_value::from_json(r#"{
            "layer":"agent","model":{"ref":"faux-default"},
            "clock":{"epochMs":1767225600000},
            "provider":{"kind":"faux","tokenSize":1000,"responses":[{"content":"ok","stopReason":"stop"}]},
            "steps":[{"expectError":{"step":{"prompt":"Hello"},"message":"deliberately impossible"}}]
        }"#).unwrap();
        let error = replay(&scenario).await.unwrap_err();
        assert_eq!(error, "Expected scripted API call to throw");
    }
}
