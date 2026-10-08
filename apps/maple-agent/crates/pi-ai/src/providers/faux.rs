//! Scripted provider from Pi's `providers/faux.ts`.
//!
//! The legacy provider registry and `createProvider` packaging are omitted.
//! Explicit handles expose the same stream and deferred-response operations.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::env::{CancellationToken, PiEnv};
use crate::types::*;
use crate::utils::event_stream::{
    AssistantMessageEventStream, AssistantMessageEventStreamWriter,
    create_assistant_message_event_stream,
};
use crate::utils::js_json::{stringify, utf16_len};
use crate::utils::js_value::to_js_value;
use crate::utils::raw_message;
use crate::utils::text::get_system_message_text;

const DEFAULT_API: &str = "faux";
const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxModelDefinition {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<InputModality>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostRates>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
}

pub type FauxContentBlock = AssistantContent;

pub fn faux_text(text: impl Into<JsString>) -> TextContent {
    TextContent::new(text)
}
pub fn faux_thinking(thinking: impl Into<JsString>) -> ThinkingContent {
    ThinkingContent::new(thinking)
}
pub fn faux_tool_call(
    env: &dyn PiEnv,
    name: impl Into<JsString>,
    arguments: impl Into<JsValue>,
    id: Option<JsString>,
) -> ToolCall {
    ToolCall::new(
        id.unwrap_or_else(|| random_id(env, "tool").into()),
        name,
        arguments,
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FauxAssistantContent {
    Text(JsString),
    Block(FauxContentBlock),
    Blocks(Vec<FauxContentBlock>),
}
impl From<String> for FauxAssistantContent {
    fn from(value: String) -> Self {
        Self::Text(value.into())
    }
}
impl From<&str> for FauxAssistantContent {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
impl From<JsString> for FauxAssistantContent {
    fn from(value: JsString) -> Self {
        Self::Text(value)
    }
}
impl From<&JsString> for FauxAssistantContent {
    fn from(value: &JsString) -> Self {
        Self::Text(value.clone())
    }
}
impl From<FauxContentBlock> for FauxAssistantContent {
    fn from(value: FauxContentBlock) -> Self {
        Self::Block(value)
    }
}
impl From<Vec<FauxContentBlock>> for FauxAssistantContent {
    fn from(value: Vec<FauxContentBlock>) -> Self {
        Self::Blocks(value)
    }
}

impl From<TextContent> for FauxAssistantContent {
    fn from(value: TextContent) -> Self {
        Self::Block(value.into())
    }
}
impl From<ThinkingContent> for FauxAssistantContent {
    fn from(value: ThinkingContent) -> Self {
        Self::Block(value.into())
    }
}
impl From<ToolCall> for FauxAssistantContent {
    fn from(value: ToolCall) -> Self {
        Self::Block(value.into())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxAssistantOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<f64>,
}

pub fn faux_assistant_message(
    env: &dyn PiEnv,
    content: impl Into<FauxAssistantContent>,
    options: FauxAssistantOptions,
) -> AssistantMessage {
    AssistantMessage {
        content: match content.into() {
            FauxAssistantContent::Text(text) => vec![faux_text(text).into()],
            FauxAssistantContent::Block(block) => vec![block],
            FauxAssistantContent::Blocks(blocks) => blocks,
        },
        api: DEFAULT_API.to_owned(),
        provider: DEFAULT_PROVIDER.to_owned(),
        model: DEFAULT_MODEL_ID.to_owned(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        deferred: options.deferred,
        error_message: options.error_message,
        response_id: options.response_id,
        timestamp: options.timestamp.unwrap_or_else(|| env.now_ms() as f64),
        ..AssistantMessage::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxProviderState {
    pub call_count: usize,
    pub deferred_fetch_count: usize,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

/// A live reference matching the state object passed to TypeScript factories.
#[derive(Clone, Default)]
pub struct SharedFauxProviderState(Arc<Mutex<FauxProviderState>>);
impl SharedFauxProviderState {
    pub fn snapshot(&self) -> FauxProviderState {
        self.0.lock().expect("faux state poisoned").clone()
    }
    pub fn read<T>(&self, read: impl FnOnce(&FauxProviderState) -> T) -> T {
        read(&self.0.lock().expect("faux state poisoned"))
    }
    fn update<T>(&self, update: impl FnOnce(&mut FauxProviderState) -> T) -> T {
        update(&mut self.0.lock().expect("faux state poisoned"))
    }
}

pub type FauxResponseFactory = Arc<
    dyn Fn(
            TranscriptContext,
            Option<SimpleStreamOptions>,
            SharedFauxProviderState,
            Model,
        ) -> BoxFuture<Result<AssistantMessage, JsString>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(FauxResponseFactory),
}
impl From<AssistantMessage> for FauxResponseStep {
    fn from(value: AssistantMessage) -> Self {
        Self::Message(Box::new(value))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxDeferredOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_fetches: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxTokenSize {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterFauxProviderOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<FauxModelDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<FauxDeferredOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_size: Option<FauxTokenSize>,
}

struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: TranscriptContext,
    options: Option<SimpleStreamOptions>,
    model: Model,
    pending_fetches: f64,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

struct Inner {
    env: Arc<dyn PiEnv>,
    min_token_size: f64,
    max_token_size: f64,
    tokens_per_second: Option<f64>,
    deferred_options: FauxDeferredOptions,
    pending_responses: Mutex<VecDeque<FauxResponseStep>>,
    prompt_cache: Mutex<HashMap<String, JsString>>,
    deferred_responses: Mutex<HashMap<JsString, Arc<Mutex<DeferredEntry>>>>,
}

#[derive(Clone)]
pub struct FauxProviderHandle {
    pub api: String,
    pub provider: String,
    pub models: Vec<Model>,
    pub state: SharedFauxProviderState,
    inner: Arc<Inner>,
}

pub fn create_faux_core(
    env: Arc<dyn PiEnv>,
    options: RegisterFauxProviderOptions,
) -> FauxProviderHandle {
    let api = options
        .api
        .unwrap_or_else(|| random_id(env.as_ref(), DEFAULT_API));
    let provider = options
        .provider
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
    let token_size = options.token_size.unwrap_or_default();
    let min_token_size = js_max(
        1.0,
        js_min(token_size.min.unwrap_or(3.0), token_size.max.unwrap_or(5.0)),
    );
    let max_token_size = js_max(min_token_size, token_size.max.unwrap_or(5.0));
    let definitions = options
        .models
        .filter(|models| !models.is_empty())
        .unwrap_or_else(|| {
            vec![FauxModelDefinition {
                id: DEFAULT_MODEL_ID.to_owned(),
                name: Some("Faux Model".to_owned()),
                reasoning: Some(false),
                input: Some(vec![InputModality::Text, InputModality::Image]),
                cost: Some(ModelCostRates::default()),
                context_window: Some(128_000.0),
                max_tokens: Some(16_384.0),
                ..FauxModelDefinition::default()
            }]
        });
    let models = definitions
        .into_iter()
        .map(|definition| Model {
            name: definition.name.unwrap_or_else(|| definition.id.clone()),
            id: definition.id,
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: definition.reasoning.unwrap_or(false),
            input: definition
                .input
                .unwrap_or_else(|| vec![InputModality::Text, InputModality::Image]),
            input_limits: definition.input_limits,
            cost: ModelCost {
                rates: definition.cost.unwrap_or_default(),
                tiers: None,
            },
            context_window: definition.context_window.unwrap_or(128_000.0),
            max_tokens: definition.max_tokens.unwrap_or(16_384.0),
            ..Model::default()
        })
        .collect();
    FauxProviderHandle {
        api,
        provider,
        models,
        state: SharedFauxProviderState::default(),
        inner: Arc::new(Inner {
            env,
            min_token_size,
            max_token_size,
            tokens_per_second: options.tokens_per_second,
            deferred_options: options.deferred.unwrap_or_default(),
            pending_responses: Mutex::new(VecDeque::new()),
            prompt_cache: Mutex::new(HashMap::new()),
            deferred_responses: Mutex::new(HashMap::new()),
        }),
    }
}

pub fn faux_provider(
    env: Arc<dyn PiEnv>,
    options: RegisterFauxProviderOptions,
) -> FauxProviderHandle {
    create_faux_core(env, options)
}

impl FauxProviderHandle {
    pub fn get_model(&self) -> &Model {
        &self.models[0]
    }
    pub fn get_model_by_id(&self, id: &str) -> Option<&Model> {
        if id.is_empty() {
            Some(self.get_model())
        } else {
            self.models.iter().find(|model| model.id == id)
        }
    }
    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        *self
            .inner
            .pending_responses
            .lock()
            .expect("faux queue poisoned") = responses.into();
    }
    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.inner
            .pending_responses
            .lock()
            .expect("faux queue poisoned")
            .extend(responses);
    }
    pub fn get_pending_response_count(&self) -> usize {
        self.inner
            .pending_responses
            .lock()
            .expect("faux queue poisoned")
            .len()
    }

    async fn resolve_response(
        &self,
        step: FauxResponseStep,
        context: TranscriptContext,
        options: Option<SimpleStreamOptions>,
        model: Model,
    ) -> Result<AssistantMessage, JsString> {
        let resolved = match step {
            FauxResponseStep::Message(message) => Ok(*message),
            FauxResponseStep::Factory(factory) => {
                let result = factory(
                    context.clone(),
                    options.clone(),
                    self.state.clone(),
                    model.clone(),
                )
                .await;
                // JavaScript await always yields, even for an already-ready
                // factory result. Keep that boundary before estimating usage.
                microtask().await;
                result
            }
        };
        let result = resolved.and_then(|resolved| {
            let message = AssistantMessage {
                api: self.api.clone(),
                provider: self.provider.clone(),
                model: model.id,
                ..resolved
            };
            with_usage_estimate(
                message,
                &context,
                options.as_ref().map(|options| &options.stream),
                &self.inner.prompt_cache,
            )
        });
        // The caller also awaits this async function. Without this boundary,
        // concurrent deferred fetches incorrectly share the first cached result
        // instead of both resolving their response as Pi does.
        microtask().await;
        result
    }

    pub fn stream(
        &self,
        model: Model,
        context: TranscriptContext,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let outer = create_assistant_message_event_stream();
        let writer = outer.writer();
        let step = self
            .inner
            .pending_responses
            .lock()
            .expect("faux queue poisoned")
            .pop_front();
        self.state.update(|state| state.call_count += 1);
        let this = self.clone();
        outer.set_producer(async move {
            let result = async {
                notify_response(
                    options
                        .as_ref()
                        .and_then(|options| options.on_response.as_ref()),
                    &model,
                )
                .await?;
                let Some(step) = step else {
                    let message =
                        this.error_message("No more faux responses queued".into(), &model);
                    let message = with_usage_estimate(
                        message,
                        &context,
                        options.as_ref().map(|options| &options.stream),
                        &this.inner.prompt_cache,
                    )?;
                    finish_error(&writer, message, ErrorReason::Error);
                    return Ok(());
                };
                if options
                    .as_ref()
                    .and_then(|options| options.deferred.as_ref())
                    .is_some_and(|deferred| !matches!(deferred, DeferredRequest::Enabled(false)))
                {
                    let handle = DeferredHandle {
                        provider: model.provider.clone(),
                        model_id: model.id.clone(),
                        api: model.api.clone(),
                        id: random_id(this.inner.env.as_ref(), "deferred").into(),
                        poll_after_ms: this.inner.deferred_options.poll_after_ms,
                        ..DeferredHandle::default()
                    };
                    this.inner
                        .deferred_responses
                        .lock()
                        .expect("faux deferred map poisoned")
                        .insert(
                            handle.id.clone(),
                            Arc::new(Mutex::new(DeferredEntry {
                                handle: handle.clone(),
                                step,
                                context,
                                options: options.clone(),
                                model: model.clone(),
                                pending_fetches: js_max(
                                    0.0,
                                    this.inner
                                        .deferred_options
                                        .pending_fetches
                                        .unwrap_or(0.0)
                                        .floor(),
                                ),
                                cancelled: false,
                                final_message: None,
                            })),
                        );
                    this.stream_with_deltas(
                        &writer,
                        this.deferred_message(&model, handle),
                        options.as_ref().and_then(|options| options.signal.as_ref()),
                    )
                    .await?;
                } else {
                    let message = this
                        .resolve_response(step, context, options.clone(), model.clone())
                        .await?;
                    this.stream_with_deltas(
                        &writer,
                        message,
                        options.as_ref().and_then(|options| options.signal.as_ref()),
                    )
                    .await?;
                }
                Ok::<(), JsString>(())
            }
            .await;
            if let Err(error) = result {
                finish_error(
                    &writer,
                    this.error_message(error, &model),
                    ErrorReason::Error,
                );
            }
        });
        outer
    }

    pub fn stream_simple(
        &self,
        model: Model,
        context: TranscriptContext,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, options)
    }
    pub fn stream_function(&self) -> StreamFunction<SimpleStreamOptions> {
        let this = self.clone();
        Arc::new(move |model, context, options| this.stream(model, context, options))
    }

    pub fn fetch_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: Option<DeferredFetchOptions>,
    ) -> AssistantMessageEventStream {
        let outer = create_assistant_message_event_stream();
        let writer = outer.writer();
        self.state.update(|state| state.deferred_fetch_count += 1);
        let this = self.clone();
        outer.set_producer(async move {
            let result = async {
                notify_response(
                    options
                        .as_ref()
                        .and_then(|options| options.request.on_response.as_ref()),
                    &model,
                )
                .await?;
                let entry = this
                    .inner
                    .deferred_responses
                    .lock()
                    .expect("faux deferred map poisoned")
                    .get(&handle.id)
                    .cloned()
                    .ok_or_else(|| prefixed("Unknown faux deferred response: ", &handle.id))?;
                let pending = {
                    let mut entry = entry.lock().expect("faux deferred entry poisoned");
                    if entry.handle.provider != handle.provider
                        || entry.handle.model_id != handle.model_id
                        || entry.handle.api != handle.api
                    {
                        return Err(prefixed("Unknown faux deferred response: ", &handle.id));
                    }
                    if entry.cancelled {
                        return Err(prefixed(
                            "Faux deferred response was cancelled: ",
                            &handle.id,
                        ));
                    }
                    if entry.pending_fetches > 0.0 {
                        entry.pending_fetches -= 1.0;
                        Some(entry.handle.clone())
                    } else {
                        None
                    }
                };
                let signal = options
                    .as_ref()
                    .and_then(|options| options.request.signal.as_ref());
                if let Some(pending) = pending {
                    return this
                        .stream_with_deltas(&writer, this.deferred_message(&model, pending), signal)
                        .await;
                }
                let final_message = entry
                    .lock()
                    .expect("faux deferred entry poisoned")
                    .final_message
                    .clone();
                let message = match final_message {
                    Some(message) => message,
                    None => {
                        let (step, context, mut submission_options, submission_model) = {
                            let entry = entry.lock().expect("faux deferred entry poisoned");
                            (
                                entry.step.clone(),
                                entry.context.clone(),
                                entry.options.clone(),
                                entry.model.clone(),
                            )
                        };
                        if let Some(options) = &mut submission_options {
                            options.deferred = None;
                            options.signal = None;
                            options.on_response = None;
                        }
                        let message = this
                            .resolve_response(
                                step,
                                context,
                                submission_options,
                                submission_model.clone(),
                            )
                            .await
                            .unwrap_or_else(|error| this.error_message(error, &submission_model));
                        entry
                            .lock()
                            .expect("faux deferred entry poisoned")
                            .final_message = Some(message.clone());
                        message
                    }
                };
                this.stream_with_deltas(&writer, message, signal).await
            }
            .await;
            if let Err(error) = result {
                finish_error(
                    &writer,
                    this.error_message(error, &model),
                    ErrorReason::Error,
                );
            }
        });
        outer
    }

    pub fn cancel_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: Option<DeferredCancelOptions>,
    ) -> BoxFuture<Result<(), JsString>> {
        self.state
            .update(|state| state.cancelled_deferred.push(handle.clone()));
        if let Some(entry) = self
            .inner
            .deferred_responses
            .lock()
            .expect("faux deferred map poisoned")
            .get(&handle.id)
        {
            entry
                .lock()
                .expect("faux deferred entry poisoned")
                .cancelled = true;
        }
        // Calling the response hook is eager, like evaluating the operand of
        // `await` inside Pi's async function; awaiting its result stays async.
        let response = options
            .and_then(|options| options.on_response)
            .map(|callback| {
                callback(
                    ProviderResponse {
                        status: 200,
                        headers: Default::default(),
                    },
                    model,
                )
            });
        Box::pin(async move {
            if let Some(response) = response {
                response
                    .await
                    .map_err(|error| JsString::from(error.to_string()))?;
            }
            Ok(())
        })
    }

    fn deferred_message(&self, model: &Model, handle: DeferredHandle) -> AssistantMessage {
        AssistantMessage {
            stop_reason: StopReason::Deferred,
            deferred: Some(handle),
            ..AssistantMessage::new(model, self.inner.env.now_ms() as f64)
        }
    }
    fn error_message(&self, error: JsString, model: &Model) -> AssistantMessage {
        AssistantMessage {
            api: self.api.clone(),
            provider: self.provider.clone(),
            model: model.id.clone(),
            stop_reason: StopReason::Error,
            error_message: Some(error),
            timestamp: self.inner.env.now_ms() as f64,
            ..AssistantMessage::default()
        }
    }
    fn abort(
        &self,
        writer: &AssistantMessageEventStreamWriter,
        partial: &SharedAssistantMessage,
        signal: Option<&CancellationToken>,
    ) -> bool {
        if !signal.is_some_and(CancellationToken::is_cancelled) {
            return false;
        }
        let aborted = AssistantMessage {
            stop_reason: StopReason::Aborted,
            error_message: Some("Request was aborted".into()),
            timestamp: self.inner.env.now_ms() as f64,
            ..partial.snapshot()
        };
        finish_error(writer, aborted, ErrorReason::Aborted);
        true
    }

    async fn stream_with_deltas(
        &self,
        writer: &AssistantMessageEventStreamWriter,
        message: AssistantMessage,
        signal: Option<&CancellationToken>,
    ) -> Result<(), JsString> {
        let partial = ShallowPartials::new(AssistantMessage {
            content: Vec::new(),
            stop_reason: StopReason::Pending,
            ..message.clone()
        });
        if self.abort(writer, &partial.message, signal) {
            return Ok(());
        }
        writer.push(AssistantMessageEvent::Start {
            partial: partial.emit(),
        });
        for (index, block) in message.content.iter().enumerate() {
            if self.abort(writer, &partial.message, signal) {
                return Ok(());
            }
            match block {
                AssistantContent::Thinking(block) => {
                    partial
                        .message
                        .append_content_copy(ThinkingContent::new("").into());
                    writer.push(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: partial.emit(),
                    });
                    for chunk in split_string_by_token_size(
                        &block.thinking,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                        self.inner.env.as_ref(),
                    ) {
                        self.schedule_chunk(&chunk).await;
                        if self.abort(writer, &partial.message, signal) {
                            return Ok(());
                        }
                        partial.update_block(index, |block| {
                            if let AssistantContent::Thinking(block) = block {
                                block.thinking.push(&chunk);
                            }
                        });
                        writer.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.emit(),
                        });
                    }
                    writer.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: block.thinking.clone(),
                        partial: partial.emit(),
                    });
                }
                AssistantContent::Text(block) => {
                    partial
                        .message
                        .append_content_copy(TextContent::new("").into());
                    writer.push(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: partial.emit(),
                    });
                    for chunk in split_string_by_token_size(
                        &block.text,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                        self.inner.env.as_ref(),
                    ) {
                        self.schedule_chunk(&chunk).await;
                        if self.abort(writer, &partial.message, signal) {
                            return Ok(());
                        }
                        partial.update_block(index, |block| {
                            if let AssistantContent::Text(block) = block {
                                block.text.push(&chunk);
                            }
                        });
                        writer.push(AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.emit(),
                        });
                    }
                    writer.push(AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: block.text.clone(),
                        partial: partial.emit(),
                    });
                }
                AssistantContent::ToolCall(block) => {
                    partial.message.append_content_copy(
                        ToolCall::new(block.id.clone(), block.name.clone(), JsonObject::new())
                            .into(),
                    );
                    writer.push(AssistantMessageEvent::ToolcallStart {
                        content_index: index,
                        partial: partial.emit(),
                    });
                    let arguments = JsString::from(block.arguments.read(stringify));
                    for chunk in split_string_by_token_size(
                        &arguments,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                        self.inner.env.as_ref(),
                    ) {
                        self.schedule_chunk(&chunk).await;
                        if self.abort(writer, &partial.message, signal) {
                            return Ok(());
                        }
                        writer.push(AssistantMessageEvent::ToolcallDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.emit(),
                        });
                    }
                    partial.update_block(index, |partial| {
                        if let AssistantContent::ToolCall(partial) = partial {
                            partial.arguments.clone_from(&block.arguments);
                        }
                    });
                    writer.push(AssistantMessageEvent::ToolcallEnd {
                        content_index: index,
                        tool_call: block.clone(),
                        partial: partial.emit(),
                    });
                }
            }
        }
        let reason = match message.stop_reason {
            StopReason::Pending => {
                return Err("Faux response ended without a stop reason".into());
            }
            StopReason::Error => {
                finish_error(writer, message, ErrorReason::Error);
                return Ok(());
            }
            StopReason::Aborted => {
                finish_error(writer, message, ErrorReason::Aborted);
                return Ok(());
            }
            StopReason::Stop => DoneReason::Stop,
            StopReason::Length => DoneReason::Length,
            StopReason::ToolUse => DoneReason::ToolUse,
            StopReason::Deferred => DoneReason::Deferred,
        };
        writer.push(AssistantMessageEvent::Done {
            reason,
            message: message.clone(),
        });
        writer.end(Some(message));
        Ok(())
    }

    async fn schedule_chunk(&self, chunk: &JsString) {
        match self.inner.tokens_per_second {
            Some(rate) if rate > 0.0 => {
                let _ = self
                    .inner
                    .env
                    .sleep(estimate_tokens(chunk) / rate * 1000.0, None)
                    .await;
            }
            _ => microtask().await,
        }
    }
}

/// Queue this continuation behind already-ready work. Tokio's `yield_now`
/// deferred-wake list reverses sibling ready continuations on its current-thread
/// executor; a normal self-wake retains Pi's FIFO promise-microtask order.
async fn microtask() {
    let mut queued = false;
    std::future::poll_fn(move |context| {
        if queued {
            std::task::Poll::Ready(())
        } else {
            queued = true;
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

async fn notify_response(callback: Option<&OnResponse>, model: &Model) -> Result<(), JsString> {
    let result = if let Some(callback) = callback {
        callback(
            ProviderResponse {
                status: 200,
                headers: Default::default(),
            },
            model.clone(),
        )
        .await
        .map_err(|error| JsString::from(error.to_string()))
    } else {
        Ok(())
    };
    microtask().await;
    result
}

fn finish_error(
    writer: &AssistantMessageEventStreamWriter,
    message: AssistantMessage,
    reason: ErrorReason,
) {
    writer.push(AssistantMessageEvent::Error {
        reason,
        error: message.clone(),
    });
    writer.end(Some(message));
}

/// Spread copies share their existing content blocks and array; appending a
/// block replaces only the producer's array, exactly like Pi's array spread.
struct ShallowPartials {
    message: SharedAssistantMessage,
}
impl ShallowPartials {
    fn new(message: AssistantMessage) -> Self {
        Self {
            message: SharedAssistantMessage::new(message),
        }
    }
    fn emit(&self) -> SharedAssistantMessage {
        self.message.shallow_clone()
    }
    fn update_block(&self, index: usize, update: impl FnOnce(&mut AssistantContent)) {
        self.message
            .update_block(index, update)
            .expect("partial content index exists");
    }
}

fn estimate_tokens(text: &JsString) -> f64 {
    (text.utf16_len() as f64 / 4.0).ceil()
}
fn json(value: &impl serde::Serialize) -> String {
    stringify(&to_js_value(value).expect("faux serializable content"))
}
fn join_text(parts: impl IntoIterator<Item = JsString>, separator: &str) -> JsString {
    let mut text = JsString::default();
    for (index, part) in parts.into_iter().enumerate() {
        if index != 0 {
            text.push_str(separator);
        }
        text.push(&part);
    }
    text
}
fn prefixed(prefix: &str, text: &JsString) -> JsString {
    let mut result = JsString::from(prefix);
    result.push(text);
    result
}
fn content_to_text(content: &[UserContent]) -> JsString {
    join_text(
        content.iter().map(|block| match block {
            UserContent::Text(block) => block.text.clone(),
            UserContent::Image(block) => {
                format!("[image:{}:{}]", block.mime_type, utf16_len(&block.data)).into()
            }
        }),
        "\n",
    )
}
fn assistant_content_to_text(content: &[AssistantContent]) -> JsString {
    join_text(
        content.iter().map(|block| match block {
            AssistantContent::Text(block) => block.text.clone(),
            AssistantContent::Thinking(block) => block.thinking.clone(),
            AssistantContent::ToolCall(block) => {
                let mut text = block.name.clone();
                text.push_str(":");
                text.push_str(&json(&block.arguments));
                text
            }
        }),
        "\n",
    )
}
fn message_to_text(message: &Message) -> Result<JsString, JsString> {
    Ok(match message {
        Message::System(message) => {
            let mut parts = vec![get_system_message_text(message)];
            parts.extend(
                message
                    .tools_removed
                    .iter()
                    .flatten()
                    .map(|tool| format!("tool-:{}", json(tool)).into()),
            );
            parts.extend(
                message
                    .tools_added
                    .iter()
                    .flatten()
                    .map(|tool| format!("tool+:{}", json(tool)).into()),
            );
            join_text(parts.into_iter().filter(|part| !part.is_empty()), "\n")
        }
        Message::User(message) => match &message.content {
            UserMessageContent::Text(text) => text.clone(),
            UserMessageContent::Blocks(blocks) => content_to_text(blocks),
        },
        Message::Assistant(message) => assistant_content_to_text(&message.content),
        Message::ToolResult(message) => {
            let mut parts = vec![message.tool_name.clone()];
            parts.extend(
                message
                    .content
                    .iter()
                    .map(|block| content_to_text(std::slice::from_ref(block))),
            );
            join_text(parts, "\n")
        }
        Message::Raw(message) => message.read(raw_message_to_text)?,
    })
}
fn raw_content_to_text(content: Option<&JsValue>) -> Result<JsString, JsString> {
    if let Some(JsValue::String(text)) = content {
        return Ok(text.clone());
    }
    let blocks = raw_message::array(content, "content", "map")?;
    let mut parts = Vec::new();
    for block in blocks {
        if raw_message::property(Some(block), "type")?.and_then(JsValue::as_str) == Some("text") {
            parts.push(block.get("text").cloned());
        } else {
            let length = raw_message::length(block.get("data"))?;
            let length = if length.is_nan() {
                "undefined".to_owned()
            } else {
                crate::utils::js_json::number_to_string(length)
            };
            let mut image = JsString::from("[image:");
            image.push(&raw_message::string(block.get("mimeType")));
            image.push_str(&format!(":{length}]"));
            parts.push(Some(JsValue::String(image)));
        }
    }
    Ok(raw_message::join_values(
        parts.iter().map(Option::as_ref),
        "\n",
    ))
}
fn raw_message_to_text(message: &JsObject) -> Result<JsString, JsString> {
    match message.get("role").and_then(JsValue::as_str) {
        Some("system") => {
            let mut parts = vec![raw_message::system_text(message)?];
            for (key, prefix) in [("toolsRemoved", "tool-:"), ("toolsAdded", "tool+:")] {
                if let Some(value) = message.get(key).filter(|value| !value.is_null()) {
                    let values = value.as_array().ok_or_else(|| {
                        JsString::from(format!("message.{key}?.map is not a function"))
                    })?;
                    parts.extend(
                        values
                            .iter()
                            .map(|tool| JsString::from(format!("{prefix}{}", stringify(tool)))),
                    );
                }
            }
            Ok(join_text(
                parts.into_iter().filter(|part| !part.is_empty()),
                "\n",
            ))
        }
        Some("user") => raw_content_to_text(message.get("content")),
        Some("assistant") => {
            let blocks = raw_message::array(message.get("content"), "content", "map")?;
            let mut parts = Vec::new();
            for block in blocks {
                let part =
                    match raw_message::property(Some(block), "type")?.and_then(JsValue::as_str) {
                        Some("text") => block.get("text").cloned(),
                        Some("thinking") => block.get("thinking").cloned(),
                        _ => {
                            let mut text = raw_message::string(block.get("name"));
                            text.push_str(":");
                            text.push_str(
                                &block
                                    .get("arguments")
                                    .map_or_else(|| "undefined".to_owned(), stringify),
                            );
                            Some(JsValue::String(text))
                        }
                    };
                parts.push(part);
            }
            Ok(raw_message::join_values(
                parts.iter().map(Option::as_ref),
                "\n",
            ))
        }
        _ => {
            let blocks = raw_message::array(message.get("content"), "message.content", "map")?;
            let mut parts = vec![message.get("toolName").cloned()];
            for block in blocks {
                parts.push(Some(JsValue::String(raw_content_to_text(Some(
                    &JsValue::Array(vec![block.clone()]),
                ))?)));
            }
            Ok(raw_message::join_values(
                parts.iter().map(Option::as_ref),
                "\n",
            ))
        }
    }
}
fn serialize_context(context: &TranscriptContext) -> Result<JsString, JsString> {
    let mut parts = Vec::new();
    for message in &context.messages {
        let mut part = match message {
            Message::Raw(raw) => raw.read(|object| raw_message::string(object.get("role"))),
            _ => message.role().into(),
        };
        part.push_str(":");
        part.push(&message_to_text(message)?);
        parts.push(part);
    }
    Ok(join_text(parts, "\n\n"))
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    context: &TranscriptContext,
    options: Option<&StreamOptions>,
    prompt_cache: &Mutex<HashMap<String, JsString>>,
) -> Result<AssistantMessage, JsString> {
    let prompt_text = serialize_context(context)?;
    let prompt_tokens = estimate_tokens(&prompt_text);
    let output = estimate_tokens(&assistant_content_to_text(&message.content));
    let mut input = prompt_tokens;
    let mut cache_read = 0.0;
    let mut cache_write = 0.0;
    if let Some(options) = options
        && options.cache_retention != Some(CacheRetention::None)
        && let Some(session_id) = options.session_id.as_ref().filter(|id| !id.is_empty())
    {
        let mut cache = prompt_cache.lock().expect("faux prompt cache poisoned");
        if let Some(previous) = cache.get(session_id).filter(|prompt| !prompt.is_empty()) {
            let cached_chars = previous
                .units()
                .zip(prompt_text.units())
                .take_while(|(left, right)| left == right)
                .count();
            cache_read = (cached_chars as f64 / 4.0).ceil();
            cache_write = ((prompt_text.utf16_len() - cached_chars) as f64 / 4.0).ceil();
            input = (prompt_tokens - cache_read).max(0.0);
        } else {
            cache_write = prompt_tokens;
        }
        cache.insert(session_id.clone(), prompt_text);
    }
    message.usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    Ok(message)
}

fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

fn random_id(env: &dyn PiEnv, prefix: &str) -> String {
    format!(
        "{prefix}:{}:{}",
        env.now_ms(),
        random_radix36_fraction(env.math_random())
    )
}

fn random_radix36_fraction(mut value: f64) -> String {
    if value == 0.0 {
        return String::new();
    }
    let mut delta = (f64::from_bits(value.to_bits() + 1) - value) / 2.0;
    let mut digits = Vec::new();
    loop {
        value *= 36.0;
        delta *= 36.0;
        let digit = value.floor() as u8;
        digits.push(digit);
        value -= f64::from(digit);
        if (value > 0.5 || (value == 0.5 && !digit.is_multiple_of(2))) && value + delta > 1.0 {
            while let Some(last) = digits.pop() {
                if last < 35 {
                    digits.push(last + 1);
                    break;
                }
            }
            break;
        }
        if value == 0.0 || value < delta {
            break;
        }
    }
    digits
        .into_iter()
        .map(|digit| {
            char::from(if digit < 10 {
                b'0' + digit
            } else {
                b'a' + digit - 10
            })
        })
        .collect()
}

fn split_string_by_token_size(
    text: &JsString,
    min: f64,
    max: f64,
    env: &dyn PiEnv,
) -> Vec<JsString> {
    let units: Vec<_> = text.units().collect();
    let mut chunks = Vec::new();
    let mut index = 0.0;
    while index < units.len() as f64 {
        let token_size = min + (env.math_random() * (max - min + 1.0)).floor();
        let char_size = js_max(1.0, token_size * 4.0);
        let start = index as usize;
        let end = (index + char_size) as usize;
        chunks.push(JsString::from_utf16(
            units[start.min(units.len())..end.min(units.len())].to_vec(),
        ));
        index += char_size;
    }
    if chunks.is_empty() {
        chunks.push(JsString::default());
    }
    chunks
}
