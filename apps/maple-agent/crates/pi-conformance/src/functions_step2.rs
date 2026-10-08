//! Direct dispatch for the selected Chat Completions function matrices.
use pi_ai::{
    api::{
        openai_completions::{OpenAICompletionsOptions, build_params, convert_messages},
        transform_messages::{NormalizeToolCallId, transform_messages_raw},
    },
    env::PiEnv,
    types::*,
    utils::{
        js_value::{from_js_value, to_js_value},
        transcript::normalize_context,
    },
};
fn decode<T: serde::de::DeserializeOwned>(value: &JsValue) -> Result<T, JsString> {
    from_js_value(value.clone()).map_err(|error| error.to_string().into())
}
pub fn evaluate(id: &str, input: &JsValue, env: &dyn PiEnv) -> Result<JsValue, JsString> {
    let model: Model = decode(&input["model"])?;
    match id {
        "api.transformMessages" => {
            let prefix = |id: &JsString, _: &Model, _: &Message| {
                let mut result = JsString::from("normalized:");
                result.push(id);
                result
            };
            let normalize = (input.get("normalizeToolCallId").and_then(JsValue::as_str)
                == Some("prefix"))
            .then_some(&prefix as &NormalizeToolCallId<'_>);
            let result = transform_messages_raw(&input["messages"], &model, normalize, env)
                .map_err(|error| JsString::from(error.to_string()))?;
            to_js_value(&result).map_err(|error| error.to_string().into())
        }
        "api.convertMessages" => {
            let context = normalize_context(decode::<Context>(&input["context"])?);
            let compat: OpenAICompletionsCompat = decode(&input["compat"])?;
            convert_messages(&model, &context, &compat, None, env).map(JsValue::Array)
        }
        "api.buildParams" => {
            let context = normalize_context(decode::<Context>(&input["context"])?);
            let options: OpenAICompletionsOptions = decode(&input["options"])?;
            build_params(&model, &context, Some(&options), env)
        }
        _ => Err(format!("Unsupported completions function: {id}").into()),
    }
}

pub fn replay(root: &std::path::Path, id: &str) -> crate::CheckResult {
    let input =
        std::fs::read_to_string(root.join("scenarios/functions").join(format!("{id}.json")))
            .map_err(|error| error.to_string())?;
    let input = pi_ai::utils::json_parse::parse_json(&input).map_err(|error| error.to_string())?;
    let epoch = input["clock"]["epochMs"]
        .as_f64()
        .ok_or("matrix clock is missing")? as i64;
    let env = pi_testkit::VirtualEnv::new(epoch);
    let mut failures = Vec::new();
    for golden in
        crate::functions::read_goldens(&root.join("corpus/functions").join(format!("{id}.jsonl")))?
    {
        let result = match (golden.result, evaluate(id, &golden.input, &env)) {
            (Ok(expected), Ok(actual)) => {
                let actual = pi_ai::utils::json_parse::parse_json(
                    &pi_ai::utils::js_json::stringify(&actual),
                )
                .map_err(|error| error.to_string())?;
                crate::compare::compare_unmodified(&expected, &actual)
            }
            (Err(expected), Err(actual)) if expected == actual => Ok(()),
            (expected, actual) => Err(format!(
                "output/error differs: expected {expected:?}, actual {actual:?}"
            )),
        };
        if let Err(error) = result {
            failures.push(format!("{}: {error}", golden.case));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}
