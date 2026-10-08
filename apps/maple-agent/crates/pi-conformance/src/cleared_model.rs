//! Direct typed Agent replay for the input-only agent.clearedModel matrix.
use pi_agent_core::{
    agent::{Agent, AgentOptions},
    types::{AgentError, AgentFuture, AgentMessage, AgentResult, GetApiKey, Shared},
};
use pi_ai::{
    types::UserMessage,
    utils::js_value::{JsValue, from_js_value, to_js_value},
};
use pi_testkit::VirtualEnv;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    id: String,
    get_api_key: bool,
    prompt: UserMessage,
}
#[derive(Deserialize)]
struct Matrix {
    function: String,
    cases: Vec<Input>,
}

pub async fn replay(input: JsValue) -> Result<JsValue, String> {
    let matrix: Matrix = from_js_value(input).map_err(|error| error.to_string())?;
    if matrix.function != "agent.clearedModel" {
        return Err("Unexpected function matrix".into());
    }
    let mut records = Vec::new();
    for input in matrix.cases {
        let calls = Shared::new(0usize);
        let captured = calls.clone();
        let key_calls = Shared::new(0usize);
        let keys = key_calls.clone();
        let get_api_key: Option<GetApiKey> = input.get_api_key.then(|| {
            Arc::new(
                move |_: String| -> AgentFuture<AgentResult<Option<String>>> {
                    keys.update(|count| *count += 1);
                    Box::pin(async { Err(AgentError::new("unexpected api key call")) })
                },
            ) as GetApiKey
        });
        let agent = Agent::new(
            AgentOptions {
                stream_fn: Some(Arc::new(move |_, _, _| {
                    captured.update(|count| *count += 1);
                    Box::pin(async { Err(AgentError::new("unexpected typed provider call")) })
                })),
                get_api_key,
                ..Default::default()
            },
            Arc::new(VirtualEnv::new(input.prompt.timestamp as i64)),
        )
        .map_err(|error| error.to_string())?;
        let initial_model = agent
            .state()
            .model()
            .ok_or("missing default model")?
            .read(|model| model.id.clone());
        let events = Shared::new(Vec::<String>::new());
        let captured = events.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            captured.update(|events| events.push(event.kind().into()));
            Box::pin(async { Ok(()) })
        }));
        agent.state().set_model(None);
        let error = agent.prompt(AgentMessage::from(input.prompt)).await.err();
        agent.wait_for_idle().await;
        let mut state = serde_json::Map::new();
        state.insert(
            "modelMissing".into(),
            agent.state().model().is_none().into(),
        );
        state.insert("isStreaming".into(), agent.state().is_streaming().into());
        if let Some(error) = agent.state().error_message() {
            state.insert(
                "errorMessage".into(),
                serde_json::to_value(error).map_err(|error| error.to_string())?,
            );
        }
        state.insert(
            "messages".into(),
            serde_json::to_value(agent.state().messages().snapshot())
                .map_err(|error| error.to_string())?,
        );
        let mut row = serde_json::json!({ "id": input.id, "initialModel": initial_model,
            "events": events.snapshot(), "providerInvocations": calls.snapshot(), "apiKeyInvocations": key_calls.snapshot(), "state": state });
        if let Some(error) = error {
            row["error"] = serde_json::json!({ "name": error.name, "message": error.message });
        }
        records.push(to_js_value(&row).map_err(|error| error.to_string())?);
    }
    Ok(JsValue::Array(records))
}

fn compare_record(input: &JsValue, expected: &JsValue, actual: &JsValue) -> crate::CheckResult {
    let mut actual = actual.clone();
    let id = input["id"]
        .as_str()
        .ok_or("Cleared-model input needs an id")?;
    if expected["id"].as_str() != Some(id) || actual["id"].as_str() != Some(id) {
        return Err("Cleared-model case identity differs".into());
    }
    match (id, input["getApiKey"].as_bool()) {
        ("without-api-key", Some(false)) => {
            if expected["providerInvocations"] != JsValue::Number(1.0)
                || actual["providerInvocations"] != JsValue::Number(0.0)
            {
                return Err(
                    "Cleared-model rule requires exactly source 1 / Rust 0 provider calls".into(),
                );
            }
            actual["providerInvocations"] = JsValue::Number(1.0);
        }
        ("with-api-key", Some(true)) => {
            if expected["providerInvocations"] != JsValue::Number(0.0) {
                return Err("API-key fixture must fail before provider invocation".into());
            }
        }
        _ => return Err("Cleared-model rule does not support this input".into()),
    }
    crate::compare::compare_unmodified(expected, &actual)
}

pub async fn replay_recorded(root: &std::path::Path) -> crate::CheckResult {
    if !crate::selection::permits_cleared_model(root)? {
        return Err("Missing owner authorization for the typed cleared-model boundary".into());
    }
    for golden in
        crate::functions::read_goldens(&root.join("corpus/functions/agent.clearedModel.jsonl"))?
    {
        let expected = golden
            .result
            .map_err(|error| format!("Unexpected recorder error: {error:?}"))?;
        let matrix = pi_ai::types::JsObject::from_iter([
            ("function", JsValue::from("agent.clearedModel")),
            ("cases", JsValue::Array(vec![golden.input.clone()])),
        ]);
        let actual = replay(matrix.into()).await?;
        let actual = actual
            .as_array()
            .and_then(|values| values.first())
            .ok_or("Missing cleared-model result")?;
        compare_record(&golden.input, &expected, actual)
            .map_err(|error| format!("{}: {error}", golden.case))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::utils::json_parse::parse_json;

    #[test]
    fn cleared_model_rule_rejects_unrelated_record_changes() {
        let input = parse_json(r#"{"id":"without-api-key","getApiKey":false}"#).unwrap();
        let expected = parse_json(r#"{"id":"without-api-key","providerInvocations":1,"events":["agent_start"],"state":{"isStreaming":false}}"#).unwrap();
        let mut actual = expected.clone();
        actual["providerInvocations"] = 0.0.into();
        assert!(compare_record(&input, &expected, &actual).is_ok());
        for (field, value) in [
            ("providerInvocations", 2.0.into()),
            ("events", JsValue::Array(vec![])),
            ("id", "other".into()),
            ("state", JsValue::Null),
        ] {
            let mut changed = actual.clone();
            changed[field] = value;
            assert!(
                compare_record(&input, &expected, &changed).is_err(),
                "{field}"
            );
        }
    }
}
