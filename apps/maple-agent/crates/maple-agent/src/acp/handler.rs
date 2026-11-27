//! The ACP request handler: the caller-facing dispatch surface that turns
//! each incoming method into a call on the connection context.

use super::convert::client_supports_form_elicitation;
use super::session::AcpConnectionContext;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    DeleteSessionRequest, DeleteSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, McpCapabilities,
    PromptCapabilities, PromptRequest, PromptResponse, SessionCapabilities,
    SessionDeleteCapabilities, SessionListCapabilities, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
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
        Box::pin(async move {
            // session/new and session/load answer as raw JSON: their responses
            // carry the v1 unstable `models` extension beside the typed
            // fields, which the typed response structs do not model.
            let message = match message {
                Dispatch::Request(request, responder)
                    if matches!(request.method(), "session/new" | "session/load") =>
                {
                    let (method, params) = request.into_parts();
                    let task_context = Arc::clone(&context);
                    let cx = cx.clone();
                    respond_in_background(&context, responder, async move {
                        task_context.session_setup(&method, &params, &cx).await
                    })
                    .await?;
                    return Ok(Handled::Yes);
                }
                other => other,
            };
            MatchDispatchFrom::new(message, &cx)
                .if_request({
                    let context = Arc::clone(&context);
                    let observer_connection = cx.clone();
                    move |request: InitializeRequest, responder: Responder<InitializeResponse>| async move {
                        context.client_supports_form_elicitation.store(
                            client_supports_form_elicitation(&request),
                            Ordering::SeqCst,
                        );
                        context.start_observing(observer_connection).await;
                        let capabilities = AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(
                                PromptCapabilities::new()
                                    .image(true)
                                    .audio(false)
                                    .embedded_context(false),
                            )
                            .mcp_capabilities(
                                McpCapabilities::new().http(true).acp(true),
                            )
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            );
                        responder.respond(
                            InitializeResponse::new(ProtocolVersion::LATEST)
                                .meta(serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(serde_json::json!({"steering": {"supported": true}})).expect("literal metadata"))
                                .agent_info(Implementation::new(
                                    "maple",
                                    env!("CARGO_PKG_VERSION"),
                                ))
                                .agent_capabilities(capabilities),
                        )
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
                            let _ = (task_context, request);
                            Err(agent_client_protocol::Error::method_not_found())
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
                    let context = Arc::clone(&context);
                    |message: Dispatch| async move {
                        match message {
                            Dispatch::Request(request, responder) if request.method == "_session/steering" => {
                                let task_context = Arc::clone(&context);
                                respond_in_background(&context, responder, async move {
                                    task_context.steer(&cx, request.params).await
                                }).await?;
                            }
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
                        Ok(())
                    }
                })
                .await
                .map(|()| Handled::Yes)
        })
    }
}
