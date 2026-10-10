//! The ACP request handler: the caller-facing dispatch surface that turns
//! each incoming method into a call on the connection context.

use super::convert::client_supports_form_elicitation;
use super::session::{AcpConnectionContext, BridgeHelloNotification};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    DeleteSessionRequest, DeleteSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionDeleteCapabilities, SessionListCapabilities,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse,
};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Client, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, JsonRpcResponse, Responder,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Answer a request from a task of the connection, so the dispatcher goes
/// on to the next message, a `session/cancel` among them, while `work` runs.
async fn respond_in_background<T, F>(
    context: &AcpConnectionContext,
    responder: Responder<T>,
    work: F,
) -> Result<(), agent_client_protocol::Error>
where
    T: JsonRpcResponse,
    F: std::future::Future<Output = Result<T, agent_client_protocol::Error>> + Send + 'static,
{
    let mut tasks = context.background_tasks.lock().await;
    while tasks.try_join_next().is_some() {}
    if context.closed.load(Ordering::SeqCst) {
        responder.respond_with_error(
            agent_client_protocol::Error::internal_error()
                .data("The Maple ACP connection is closing"),
        )?;
        return Ok(());
    }
    tasks.spawn(async move {
        let _ = responder.respond_with_result(work.await);
    });
    Ok(())
}

#[derive(Clone)]
pub(super) struct MapleAcpHandler {
    pub(super) context: Arc<AcpConnectionContext>,
}

/// `session/new` fields that Buzz adds beyond the ACP 1 schema: a top-level
/// `systemPrompt` (its protocol 2 system role) and `_meta.sessionTitle`.
/// They are read from the raw request before typed dispatch drops them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct AcpCallerSessionFields {
    pub(super) system_prompt: Option<String>,
    pub(super) session_title: Option<String>,
}

impl AcpCallerSessionFields {
    pub(super) fn from_dispatch(message: &Dispatch) -> Self {
        let Dispatch::Request(request, _) = message else {
            return Self::default();
        };
        if request.method() != "session/new" {
            return Self::default();
        }
        Self::from_params(request.params())
    }

    pub(super) fn from_params(params: &serde_json::Value) -> Self {
        let text = |value: Option<&serde_json::Value>| {
            value
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Self {
            system_prompt: text(params.get("systemPrompt")),
            session_title: text(params.pointer("/_meta/sessionTitle")),
        }
    }
}

impl HandleDispatchFrom<Client> for MapleAcpHandler {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "maple-acp"
    }

    fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Client>,
    ) -> impl std::future::Future<Output = Result<Handled<Dispatch>, agent_client_protocol::Error>> + Send
    {
        let context = Arc::clone(&self.context);
        let caller_fields = AcpCallerSessionFields::from_dispatch(&message);
        Box::pin(async move {
            MatchDispatchFrom::new(message, &cx)
                .if_notification({
                    let context = Arc::clone(&context);
                    |notification: BridgeHelloNotification| async move {
                        context.set_bridge_environment(notification.environment).await;
                        Ok(())
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    move |request: InitializeRequest, responder: Responder<InitializeResponse>| async move {
                        context.client_supports_form_elicitation.store(
                            client_supports_form_elicitation(&request),
                            Ordering::SeqCst,
                        );
                        let capabilities = AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(
                                PromptCapabilities::new()
                                    .image(true)
                                    .audio(false)
                                    .embedded_context(false),
                            )
                            .mcp_capabilities(McpCapabilities::new().http(true))
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .close(SessionCloseCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            );
                        responder.respond(
                            // Protocol 2 tells Buzz that `session/new`
                            // accepts its `systemPrompt` field. The schema
                            // crate keeps `V2` behind an unstable feature,
                            // so build the value from the number.
                            InitializeResponse::new(
                                agent_client_protocol::schema::ProtocolVersion::from(2u16),
                            )
                            .agent_info(Implementation::new("maple", env!("CARGO_PKG_VERSION")))
                            .agent_capabilities(capabilities),
                        )
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    let cx = cx.clone();
                    |request: NewSessionRequest, responder: Responder<NewSessionResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.new_session(&cx, request, caller_fields).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    let cx = cx.clone();
                    |request: LoadSessionRequest, responder: Responder<LoadSessionResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.load_session(&cx, request).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    |request: ListSessionsRequest, responder: Responder<ListSessionsResponse>| async move {
                        responder.respond_with_result(context.list_sessions(request).await)
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    |request: CloseSessionRequest, responder: Responder<CloseSessionResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.close_session(request).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    |request: DeleteSessionRequest, responder: Responder<DeleteSessionResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.delete_session(request).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    |request: SetSessionConfigOptionRequest, responder: Responder<SetSessionConfigOptionResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.set_config_option(request).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    |request: SetSessionModeRequest, responder: Responder<SetSessionModeResponse>| async move {
                        let task_context = Arc::clone(&context);
                        respond_in_background(&context, responder, async move {
                            task_context.set_mode(request).await
                        })
                        .await
                    }
                })
                .await
                .if_request({
                    let context = Arc::clone(&context);
                    let cx = cx.clone();
                    |request: PromptRequest, responder: Responder<PromptResponse>| async move {
                        // Admitted here, in order, so a `session/cancel` the
                        // client sends next finds the prompt; the rest runs
                        // in the background.
                        let admission = match context.begin_prompt(&request).await {
                            Ok(admission) => admission,
                            Err(error) => {
                                responder.respond_with_error(error)?;
                                return Ok(());
                            }
                        };
                        let session_id = admission.session_id.clone();
                        let task_context = Arc::clone(&context);
                        let mut tasks = context.background_tasks.lock().await;
                        while tasks.try_join_next().is_some() {}
                        if context.closed.load(Ordering::SeqCst) {
                            context.prompt_states.lock().await.remove(&session_id);
                            responder.respond_with_error(
                                agent_client_protocol::Error::internal_error()
                                    .data("The Maple ACP connection is closing"),
                            )?;
                            return Ok(());
                        }
                        tasks.spawn(async move {
                            let _ = responder
                                .respond_with_result(task_context.prompt(&cx, admission).await);
                        });
                        Ok(())
                    }
                })
                .await
                .if_notification({
                    let context = Arc::clone(&context);
                    |notification: CancelNotification| async move {
                        context.cancel(notification).await
                    }
                })
                .await
                .otherwise({
                    let cx = cx.clone();
                    |message: Dispatch| async move {
                        match message {
                            Dispatch::Request(_, responder) => {
                                responder.respond_with_error(
                                    agent_client_protocol::Error::method_not_found(),
                                )?;
                            }
                            Dispatch::Response(result, router) => {
                                router.route_with_result(result)?;
                            }
                            Dispatch::Notification(_) => {}
                        }
                        let _ = cx;
                        Ok(())
                    }
                })
                .await
                .map(|()| Handled::Yes)
        })
    }
}
