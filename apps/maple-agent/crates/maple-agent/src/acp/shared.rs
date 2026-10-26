//! Projection of shared host tasks and the steering extension used by the
//! Codex and Claude ACP adapters. ACP v1 prompt responses still end with a turn.

use super::*;
use serde_json::{Value, json};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SteeringRequest {
    session_id: SessionId,
    prompt: Vec<agent_client_protocol::schema::v1::ContentBlock>,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

impl AcpConnectionContext {
    pub(super) async fn steer(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        params: Value,
    ) -> Result<Value, agent_client_protocol::Error> {
        let request: SteeringRequest = serde_json::from_value(params)
            .map_err(|_| agent_client_protocol::Error::invalid_params())?;
        let idle = request
            .meta
            .pointer("/steering/idleBehavior")
            .and_then(Value::as_str);
        if idle.is_some_and(|behavior| behavior != "promptRequired") {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("Unsupported steering idleBehavior"));
        }
        let session_id = canonical_session_id(&request.session_id)?;
        let (access, model) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(&session_id).ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(session_id.clone()))
            })?;
            (
                session
                    .lease
                    .as_ref()
                    .ok_or_else(agent_client_protocol::Error::invalid_request)?
                    .access(),
                session.model.clone(),
            )
        };
        let catalog = self
            .agent
            .model_catalog_entry(&model)
            .await
            .map_err(internal_acp_error)?;
        let attachments = self
            .agent
            .import_image_attachments(session_id.clone(), prompt_images(&request.prompt))
            .await
            .map_err(internal_acp_error)?
            .into_iter()
            .map(|attachment| attachment.id)
            .collect::<Vec<_>>();
        let injected = self
            .agent
            .steer_surface_message(
                &access,
                AgentSendMessageRequest {
                    session_id: session_id.clone(),
                    text: prompt_text(&request.prompt)?,
                    attachments: attachments.clone(),
                    model: Some(model),
                    context_limit: None,
                    vision_capable: catalog.and_then(|entry| entry.vision) == Some(true),
                    steer: true,
                    queue_id: None,
                },
            )
            .await
            .map_err(internal_acp_error)?;
        if injected {
            return Ok(json!({"outcome": "injected"}));
        }
        if idle == Some("promptRequired") {
            return Ok(json!({"outcome": "promptRequired", "reason": "noRunningTurn"}));
        }
        // Legacy adapters start a new turn when steering races with completion.
        // Clients opting into promptRequired retain ownership of that fallback.
        let prompt = PromptRequest::new(request.session_id, request.prompt);
        let mut admission = self.begin_prompt(&prompt).await?;
        admission.images = PromptAttachments::Committed(attachments);
        let context = Arc::clone(self);
        let cx = cx.clone();
        self.background_tasks.lock().await.spawn(async move {
            if let Err(error) = context.prompt(&cx, admission).await {
                log::debug!("ACP steering fallback ended: {error}");
            }
        });
        Ok(json!({"outcome": "startedNewTurn"}))
    }
}

impl AcpConnectionContext {
    pub(super) async fn elicit_question(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: String,
        request_id: String,
        questions: Vec<crate::agent::AgentQuestion>,
        cancellation: &CancellationToken,
    ) {
        use agent_client_protocol::schema::v1::{
            CreateElicitationRequest, ElicitationFormMode, ElicitationSchema,
            ElicitationSessionScope, StringPropertySchema,
        };
        let mut resolved = self.agent.subscribe();
        if !self.agent.pending_questions().await.iter().any(|event| matches!(event, crate::agent::AgentServiceEvent::Question { request_id: id, .. } if id == &request_id)) { return; }
        let message = questions
            .iter()
            .map(|question| {
                let options = question
                    .options
                    .iter()
                    .map(|option| format!("- {}: {}", option.label, option.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{}\n{}", question.question, options)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let schema = ElicitationSchema::new().property(
            "answer",
            StringPropertySchema::new().title("Your answer"),
            true,
        );
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(ElicitationSessionScope::new(session_id), schema),
            message,
        );
        let sent = cx.send_request(request);
        let response = sent.block_task();
        tokio::pin!(response);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                event = resolved.recv() => match event {
                    Ok(crate::agent::AgentServiceEvent::QuestionResolved { request_id: id }) if id == request_id => return,
                    Err(_) => return,
                    _ => {},
                },
                result = &mut response => {
                    if let Ok(result) = result {
                        match result.action {
                            ElicitationAction::Accept(action) => {
                                if let Some(ElicitationContentValue::String(answer)) = action.content.as_ref().and_then(|content| content.get("answer")) {
                                    let _ = self.agent.answer_question_via_handle(&request_id, answer.clone()).await;
                                }
                            }
                            ElicitationAction::Decline => { let _ = self.agent.answer_question_via_handle(&request_id, "The user declined to answer.".into()).await; }
                            _ => {}, // Closing a form is not a denial on other interfaces.
                        }
                    }
                    return;
                }
            }
        }
    }
}
