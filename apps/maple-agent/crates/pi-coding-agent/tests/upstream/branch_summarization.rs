use super::compaction_common::*;
use pi_ai::env::CancellationToken;
use pi_ai::types::{AssistantMessage, DoneReason, Model};
use pi_coding_agent::core::compaction::branch_summarization::{
    GenerateBranchSummaryOptions, generate_branch_summary,
};
use pi_coding_agent::core::session_manager::SessionEntry;
use serde_json::{Value, json as j};

fn model() -> Model {
    let mut model = summary_model(false, 8192.0, None);
    model.id = "test-model".into();
    model.name = "Test Model".into();
    model
}
fn entries() -> Vec<SessionEntry> {
    json(
        j!([{"type":"message","id":"branch-user","parentId":null,"timestamp":"1970-01-01T00:00:00.001Z",
        "message":{"role":"user","content":"Abandoned request","timestamp":1}}]),
    )
}
fn response(content: Value) -> AssistantMessage {
    let mut response = assistant("", Some(usage(0.0, 0.0, 0.0, 0.0)));
    response["model"] = j!("test-model");
    response["content"] = content;
    json(response)
}
fn options(model: Model) -> GenerateBranchSummaryOptions {
    GenerateBranchSummaryOptions {
        model,
        api_key: None,
        headers: None,
        env: None,
        signal: CancellationToken::new(),
        custom_instructions: None,
        replace_instructions: false,
        reserve_tokens: None,
        retry: None,
    }
}

#[allow(clippy::module_inception)] // Retain the exact upstream describe group.
mod branch_summarization {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn does_not_override_tool_choice_for_branch_summaries() {
        let mock = SummaryMock::new();
        mock.once(
            response(j!([{"type":"text","text":"summary"}])),
            DoneReason::Stop,
        );
        generate_branch_summary(&entries(), &options(model()), &mock.runtime(), None)
            .await
            .unwrap();
        let calls = mock.calls();
        assert_eq!(calls[0].options["maxTokens"], 4096);
        assert!(calls[0].options.get("toolChoice").is_none());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn clamps_the_branch_summary_output_cap_to_the_model_limit() {
        let mock = SummaryMock::new();
        mock.once(
            response(j!([{"type":"text","text":"summary"}])),
            DoneReason::Stop,
        );
        let mut model = model();
        model.max_tokens = 1024.0;
        generate_branch_summary(&entries(), &options(model), &mock.runtime(), None)
            .await
            .unwrap();
        assert_eq!(mock.calls()[0].options["maxTokens"], 1024);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rejects_tool_calls_from_branch_summaries() {
        let mock = SummaryMock::new();
        // Upstream changes the done event reason to toolUse but keeps faux's
        // response.stopReason="stop"; tool content itself must be rejected.
        mock.once(response(j!([{"type":"toolCall","id":"tool-call-1","name":"read","arguments":{"path":"README.md"}}])),DoneReason::ToolUse);
        let result = generate_branch_summary(&entries(), &options(model()), &mock.runtime(), None)
            .await
            .unwrap();
        assert_eq!(
            observed(&result.error),
            "Branch summarization attempted to call a tool"
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rejects_length_limited_branch_summaries() {
        let mock = SummaryMock::new();
        let mut response = observed(&response(j!([{"type":"text","text":"partial"}])));
        response["stopReason"] = j!("length");
        mock.once(json(response), DoneReason::Length);
        let result = generate_branch_summary(&entries(), &options(model()), &mock.runtime(), None)
            .await
            .unwrap();
        assert_eq!(
            observed(&result.error),
            "Branch summarization failed: generation hit the token cap and the summary is incomplete"
        );
    }
}
