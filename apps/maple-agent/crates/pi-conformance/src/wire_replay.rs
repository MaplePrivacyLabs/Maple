//! Recorded chat-completions scenarios replayed through the selected Pi provider.
//! SSE decoding, HTTP failures and header defaults here are an injected test
//! transport seam. Expected output is read only from the TypeScript corpus.
use crate::{
    CheckResult,
    compare::{self, CompareOptions, Concurrency},
};
use futures_util::stream as futures_stream;
use pi_ai::{
    api::openai_completions::{
        self, ChunkStream, CompletionsRequest, CompletionsResponse, CompletionsTransport,
        OpenAICompletionsOptions, ScriptedTransport,
    },
    env::{CancellationToken, PiEnv},
    types::{
        AssistantMessageEvent, BoxFuture, Context, JsObject, JsString, JsValue, Message, Model,
        ProviderResponse, RawMessage,
    },
    utils::{
        js_json::stringify,
        js_value::{from_js_value, to_js_value},
        json_parse::parse_json,
        transcript::normalize_context,
    },
};
use pi_testkit::VirtualEnv;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{sync::Notify, task::JoinHandle};

#[derive(Debug)]
pub struct WireObservation {
    pub events: Vec<JsValue>,
    pub requests: Vec<JsValue>,
    pub http: Vec<JsValue>,
    pub final_state: JsValue,
}
fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    JsValue::Object(entries.into_iter().collect())
}
fn observed(value: &impl Serialize) -> CheckResult<JsValue> {
    let value = to_js_value(value).map_err(|e| e.to_string())?;
    parse_json(&stringify(&value)).map_err(|e| e.to_string())
}
fn field<'a>(value: &'a JsValue, key: &str) -> CheckResult<&'a JsValue> {
    value
        .get(key)
        .ok_or_else(|| format!("Wire fixture missing {key}"))
}
fn text(value: &JsValue) -> CheckResult<&str> {
    value
        .as_str()
        .ok_or_else(|| "Wire fixture expected a host string".into())
}
fn array(value: &JsValue) -> CheckResult<&[JsValue]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "Wire fixture expected an array".into())
}
fn decode<T: serde::de::DeserializeOwned>(value: JsValue) -> CheckResult<T> {
    from_js_value(value).map_err(|e| e.to_string())
}
fn snapshot_messages(values: &[JsValue]) -> CheckResult<Vec<Message>> {
    values
        .iter()
        .map(|value| {
            value
                .as_object()
                .cloned()
                .map(|raw| Message::Raw(RawMessage::new(raw)))
                .ok_or_else(|| "Wire fixture message is not an object".into())
        })
        .collect()
}

#[derive(Clone, Default)]
struct Gates(Arc<Mutex<HashMap<String, CancellationToken>>>);
impl Gates {
    fn gate(&self, name: &str) -> CancellationToken {
        self.0
            .lock()
            .unwrap()
            .entry(name.into())
            .or_default()
            .clone()
    }
    fn open(&self, name: &str) {
        self.gate(name).cancel();
    }
}
enum Frame {
    Gate(String),
    Data(JsValue),
    Error(JsString),
    Done,
}
fn adapter_error(fixture: &JsValue) -> CheckResult<JsString> {
    field(fixture, "transportError")?
        .as_js_str()
        .cloned()
        .ok_or_else(|| "transportError must be a string".into())
}
fn decoded_frame(payload: &str, fixture: &JsValue) -> CheckResult<Frame> {
    if payload == "[DONE]" {
        return Ok(Frame::Done);
    }
    let value = parse_json(payload).map_err(|e| format!("Fixture SSE JSON: {e}"))?;
    if value.get("error").is_some_and(|value| !value.is_null()) {
        Ok(Frame::Error(adapter_error(fixture)?))
    } else {
        Ok(Frame::Data(value))
    }
}
fn frames(fixture: &JsValue) -> CheckResult<VecDeque<Frame>> {
    let mut frames = VecDeque::new();
    let mut pending = String::new();
    for chunk in array(field(fixture, "sse")?)? {
        if let Some(gate) = chunk.get("gate") {
            frames.push_back(Frame::Gate(text(gate)?.into()));
            continue;
        }
        if chunk.get("disconnect").is_some() {
            frames.push_back(Frame::Error(adapter_error(fixture)?));
            continue;
        }
        let raw = if let Some(raw) = chunk.get("raw") {
            raw.as_js_str()
                .ok_or_else(|| "raw SSE fixture must be a string".to_owned())?
                .to_string_lossy()
        } else if let Some(comment) = chunk.get("comment") {
            format!(": {}\n\n", text(comment)?)
        } else {
            let data = field(chunk, "data")?;
            format!(
                "data: {}\n\n",
                data.as_js_str()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_else(|| stringify(data))
            )
        };
        pending.push_str(&raw.replace("\r\n", "\n").replace('\r', "\n"));
        while let Some(end) = pending.find("\n\n") {
            let event = pending[..end].to_owned();
            pending.drain(..end + 2);
            let data = event
                .lines()
                .filter_map(|line| {
                    if line == "data" {
                        Some("")
                    } else {
                        line.strip_prefix("data:")
                            .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    }
                })
                .collect::<Vec<_>>();
            if !data.is_empty() {
                frames.push_back(decoded_frame(&data.join("\n"), fixture)?);
            }
        }
    }
    if !pending.is_empty() {
        return Err("Wire fixture ends in an unterminated SSE event".into());
    }
    Ok(frames)
}
fn chunks(frames: VecDeque<Frame>, gates: Gates, signal: Option<CancellationToken>) -> ChunkStream {
    Box::pin(futures_stream::unfold(
        (frames, gates, signal, false),
        |(mut frames, gates, signal, done)| async move {
            if done {
                return None;
            }
            loop {
                match frames.pop_front()? {
                    Frame::Gate(name) => {
                        let gate = gates.gate(&name);
                        if let Some(signal) = &signal {
                            if signal.is_cancelled() {
                                return None;
                            }
                            tokio::select! {_ = gate.cancelled()=>{},_ = signal.cancelled()=>return None}
                        } else {
                            gate.cancelled().await;
                        }
                    }
                    Frame::Data(value) => return Some((Ok(value), (frames, gates, signal, false))),
                    Frame::Error(error) => {
                        return Some((Err(error), (frames, gates, signal, true)));
                    }
                    Frame::Done => return None,
                }
            }
        },
    ))
}
#[derive(Clone)]
struct FixtureTransport {
    scripted: ScriptedTransport,
    fixtures: Arc<Mutex<VecDeque<JsValue>>>,
    gates: Gates,
}
impl CompletionsTransport for FixtureTransport {
    fn send(
        &self,
        request: CompletionsRequest,
    ) -> BoxFuture<Result<CompletionsResponse, JsString>> {
        let fixture = self.fixtures.lock().unwrap().pop_front();
        let prepared = (|| -> CheckResult<()> {
            let fixture =
                fixture.ok_or_else(|| "Unexpected scripted fetch invocation".to_owned())?;
            let status = field(&fixture, "status")?
                .as_u64()
                .ok_or_else(|| "Wire status must be an integer".to_owned())?;
            if fixture.get("networkError").is_some() || !(200..300).contains(&status) {
                self.scripted.push_error(adapter_error(&fixture)?);
                return Ok(());
            }
            let mut response = ProviderResponse {
                status: status.try_into().map_err(|_| "Wire status exceeds u16")?,
                ..Default::default()
            };
            response
                .headers
                .insert("content-type".into(), "text/event-stream".into());
            if let Some(headers) = fixture.get("headers").and_then(JsValue::as_object) {
                for (key, value) in headers.iter() {
                    response
                        .headers
                        .insert(key.to_string_lossy(), text(value)?.into());
                }
            }
            self.scripted.push_stream_response(
                response,
                chunks(
                    frames(&fixture)?,
                    self.gates.clone(),
                    request.signal.clone(),
                ),
            );
            Ok(())
        })();
        if let Err(error) = prepared {
            return Box::pin(async move { Err(error.into()) });
        }
        self.scripted.send(request)
    }
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Default)]
struct ObservationState {
    messages: Vec<JsValue>,
    results: Vec<JsValue>,
    events: Vec<JsValue>,
    retained: Vec<AssistantMessageEvent>,
    active: bool,
    failure: Option<String>,
}
impl ObservationState {
    fn emit(&mut self, kind: &str, data: JsValue) {
        self.events.push(object([
            ("seq", (self.events.len() as f64).into()),
            ("type", kind.into()),
            ("entries", 0.0.into()),
            ("data", data),
        ]));
    }
}
async fn await_idle(pending: &mut Option<JoinHandle<CheckResult>>) -> CheckResult {
    if let Some(task) = pending.take() {
        task.await
            .map_err(|e| format!("Wire consumer failed: {e}"))??;
    }
    Ok(())
}
fn request_http(request: &CompletionsRequest, call: usize) -> CheckResult<JsValue> {
    let mut headers = BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]);
    for (key, value) in &request.headers {
        let key = key.to_ascii_lowercase();
        if [
            "content-type",
            "x-session-id",
            "x-client-request-id",
            "anthropic-beta",
        ]
        .contains(&key.as_str())
        {
            if let Some(value) = value {
                headers.insert(key, value.clone());
            } else {
                headers.remove(&key);
            }
        }
    }
    let base = url::Url::parse(&request.model.base_url).map_err(|e| e.to_string())?;
    let path = format!("{}/chat/completions", base.path().trim_end_matches('/'));
    Ok(object([
        ("call", (call as f64).into()),
        ("method", "POST".into()),
        ("path", path.into()),
        (
            "headers",
            JsValue::Object(headers.into_iter().map(|(k, v)| (k, v.into())).collect()),
        ),
        ("body", observed(&request.params)?),
        ("bodyRaw", stringify(&request.params).into()),
    ]))
}

pub async fn observe(input: &JsValue) -> CheckResult<WireObservation> {
    if field(input, "layer")?.as_str() != Some("wire")
        || field(field(input, "provider")?, "kind")?.as_str() != Some("wire")
    {
        return Err("Unsupported wire scenario".into());
    }
    for key in ["tools", "variants"] {
        if input
            .get(key)
            .and_then(JsValue::as_array)
            .is_some_and(|items| !items.is_empty())
        {
            return Err("Wire-only scenarios require explicit transcript tool declarations".into());
        }
    }
    let epoch = field(field(input, "clock")?, "epochMs")?
        .as_i64()
        .ok_or_else(|| "Wire clock epoch must fit i64".to_owned())?;
    let env = Arc::new(VirtualEnv::new(epoch));
    let gates = Gates::default();
    let transport = Arc::new(FixtureTransport {
        scripted: ScriptedTransport::default(),
        fixtures: Arc::new(Mutex::new(
            array(field(field(input, "provider")?, "responses")?)?
                .to_vec()
                .into(),
        )),
        gates: gates.clone(),
    });
    let mut model = field(field(input, "model")?, "value")?.clone();
    let initial = input
        .get("initialMessages")
        .map(array)
        .transpose()?
        .unwrap_or_default()
        .to_vec();
    let state = Arc::new(Mutex::new(ObservationState {
        messages: initial,
        ..Default::default()
    }));
    let changed = Arc::new(Notify::new());
    let mut pending = None;
    let mut controller = None;
    let mut requests = vec![];
    for step in array(field(input, "steps")?)? {
        state.lock().unwrap().emit("$step", observed(step)?);
        if let Some(prompt) = step.get("prompt") {
            if state.lock().unwrap().active {
                return Err("Prompt while wire stream is active".into());
            }
            await_idle(&mut pending).await?;
            let messages = {
                let mut state = state.lock().unwrap();
                if prompt.is_string() {
                    state.messages.push(object([
                        ("role", "user".into()),
                        ("content", prompt.clone()),
                        ("timestamp", (env.now_ms() as f64).into()),
                    ]));
                } else if let Some(values) = prompt.as_array() {
                    state.messages.extend(values.clone());
                } else {
                    state.messages.push(prompt.clone());
                }
                snapshot_messages(&state.messages)?
            };
            let context = normalize_context(Context {
                messages,
                system_prompt: input
                    .get("systemPrompt")
                    .and_then(JsValue::as_js_str)
                    .cloned(),
                tools: None,
            });
            let mut options = JsObject::new();
            options.insert("maxRetries", 0.0.into());
            if let Some(overrides) = input.get("options").and_then(JsValue::as_object) {
                options.extend(overrides.clone());
            }
            options.insert("apiKey", "<apiKey>".into());
            requests.push(object([
                ("call", (requests.len() as f64).into()),
                ("model", model.clone()),
                ("context", observed(&context)?),
                ("options", JsValue::Object(options.clone())),
            ]));
            options.insert("apiKey", "fixture-key".into());
            let mut options: OpenAICompletionsOptions = decode(options.into())?;
            let signal = CancellationToken::new();
            options.signal = Some(signal.clone());
            controller = Some(CancelOnDrop(signal));
            let mut output = openai_completions::stream(
                decode::<Model>(model.clone())?,
                context,
                Some(options),
                transport.clone(),
                env.clone(),
            )
            .map_err(|e| e.to_string_lossy())?;
            state.lock().unwrap().active = true;
            let state = state.clone();
            let changed = changed.clone();
            pending = Some(tokio::spawn(async move {
                let result: CheckResult = async {
                    while let Some(event) = output.next().await {
                        let snapshot = observed(&event)?;
                        let kind = text(field(&snapshot, "type")?)?.to_owned();
                        let mut state = state.lock().unwrap();
                        state.retained.push(event);
                        state.emit(&kind, snapshot);
                        drop(state);
                        changed.notify_one();
                    }
                    let result = observed(&output.result().await)?;
                    let mut state = state.lock().unwrap();
                    state.messages.push(result.clone());
                    state.results.push(result);
                    Ok(())
                }
                .await;
                let mut state = state.lock().unwrap();
                state.active = false;
                if let Err(error) = &result {
                    state.failure = Some(error.clone());
                }
                drop(state);
                changed.notify_one();
                result
            }));
        } else if let Some(next) = step.get("setModel") {
            if state.lock().unwrap().active {
                return Err("Wire model changes require an idle stream".into());
            }
            model = field(next, "value")?.clone();
        } else if let Some(wait) = step.get("waitFor") {
            let kind = text(field(wait, "type")?)?;
            let count = wait.get("count").and_then(JsValue::as_u64).unwrap_or(1) as usize;
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let state = state.lock().unwrap();
                    if let Some(error) = &state.failure {
                        return Err(error.clone());
                    }
                    if state
                        .events
                        .iter()
                        .filter(|e| e.get("type").and_then(JsValue::as_str) == Some(kind))
                        .count()
                        >= count
                    {
                        break;
                    }
                    if !state.active {
                        return Err(format!(
                            "Wire stream ended before waitFor {}",
                            stringify(wait)
                        ));
                    }
                }
                notified.await;
            }
        } else if let Some(name) = step.get("open") {
            gates.open(text(name)?);
        } else if step.get("awaitIdle").is_some() {
            await_idle(&mut pending).await?;
        } else if step.get("abort").is_some() {
            if let Some(controller) = &controller {
                controller.0.cancel();
            }
        } else if let Some(ms) = step.get("advanceClock") {
            env.advance(
                ms.as_u64()
                    .ok_or_else(|| "Wire advanceClock must be u64".to_owned())?,
            )
            .await;
        } else {
            return Err(format!("Unsupported wire step {}", stringify(step)));
        }
    }
    await_idle(&mut pending).await?;
    if !transport.fixtures.lock().unwrap().is_empty() {
        return Err("Unconsumed wire responses".into());
    }
    let state = state.lock().unwrap();
    let final_state = object([
        (
            "state",
            object([
                ("messages", state.messages.clone().into()),
                ("results", state.results.clone().into()),
                ("retainedEvents", observed(&state.retained)?),
            ]),
        ),
        ("queues", JsObject::new().into()),
        ("errors", Vec::<JsValue>::new().into()),
    ]);
    let http = transport
        .scripted
        .requests()
        .iter()
        .enumerate()
        .map(|(i, request)| request_http(request, i))
        .collect::<CheckResult<Vec<_>>>()?;
    Ok(WireObservation {
        events: state.events.clone(),
        requests,
        http,
        final_state: observed(&final_state)?,
    })
}
fn read_json(path: &Path) -> CheckResult<JsValue> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_json(&source).map_err(|e| format!("{}: {e}", path.display()))
}
fn read_jsonl(path: &Path) -> CheckResult<Vec<JsValue>> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    source
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| parse_json(line).map_err(|e| format!("{}: {e}", path.display())))
        .collect()
}
pub async fn replay(root: &Path, id: &str) -> CheckResult {
    let directory = root.join("corpus/scenarios").join(id);
    let input = read_json(&directory.join("scenario.json"))?;
    if !crate::selection::permits_gated_live_partials(root, id)? {
        return Err(format!("{id}: missing approved gated-observation rule"));
    }
    validate_observation_gates(&input)?;
    let actual = observe(&input).await?;
    let options = CompareOptions::default();
    compare::compare_events(
        &read_jsonl(&directory.join("events.jsonl"))?,
        &actual.events,
        &options,
        Concurrency::Gated,
    )
    .map_err(|e| format!("{id}/events.jsonl: {e}"))?;
    for (name, values) in [("requests", actual.requests), ("http", actual.http)] {
        compare::compare(
            &read_jsonl(&directory.join(format!("{name}.jsonl")))?.into(),
            &values.into(),
            &options,
        )
        .map_err(|e| format!("{id}/{name}.jsonl: {e}"))?;
    }
    compare::compare(
        &read_json(&directory.join("final.json"))?,
        &actual.final_state,
        &options,
    )
    .map_err(|e| format!("{id}/final.json: {e}"))
}

fn validate_observation_gates(input: &JsValue) -> CheckResult {
    if field(input, "layer")?.as_str() != Some("wire")
        || field(input, "concurrency")?.as_str() != Some("gated")
    {
        return Err("Wire live partials require the approved gated observation schedule".into());
    }
    let responses = array(field(field(input, "provider")?, "responses")?)?;
    let mut streams = 0;
    for response in responses {
        // HTTP failures have no SSE body and hence no intermediate partials.
        let Some(frames) = response.get("sse") else {
            continue;
        };
        streams += 1;
        let frames = array(frames)?;
        let gates = frames
            .iter()
            .filter_map(|frame| frame.get("gate"))
            .map(text)
            .collect::<CheckResult<Vec<_>>>()?;
        if frames.first().and_then(|frame| frame.get("gate")).is_none()
            || gates.len() < 2
            || gates.iter().any(|gate| gate.is_empty())
        {
            return Err(
                "Each recorded SSE stream requires explicit initial and later input gates".into(),
            );
        }
    }
    if streams == 0 {
        return Err("Gated wire fixture has no SSE stream".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gated_partial_rule_rejects_ungated_inputs() {
        let fixture = |concurrency: &str, frames: &str| {
            parse_json(&format!(
                r#"{{"layer":"wire","concurrency":"{concurrency}","provider":{{"responses":[{{"sse":{frames}}}]}}}}"#
            ))
            .unwrap()
        };
        let gates = r#"[{"gate":"first"},{"data":{}},{"gate":"end"}]"#;
        assert!(validate_observation_gates(&fixture("gated", gates)).is_ok());
        assert!(validate_observation_gates(&fixture("barrier", gates)).is_err());
        assert!(validate_observation_gates(&fixture("gated", r#"[{"data":{}}]"#)).is_err());
        assert!(validate_observation_gates(&fixture("gated", r#"[{"gate":"first"}]"#)).is_err());
    }
}
