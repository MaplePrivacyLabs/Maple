//! Tinfoil's decision models behind `/v1/systemone`: `decision-small`, `decision-medium` and
//! `decision-large` (vLLM-SR d3 checkpoints in Tinfoil's attested enclaves, served as OpenAI's
//! `POST /v1/decisions`). The System One request and response keep their shape; this module
//! translates both ways. One upstream request answers every question, with no prompt, prefill,
//! token table or calibration of ours: d3 lays out its own prompt and reports the
//! probabilities of a trained classification head.
//!
//! Where the two shapes differ, the translation follows d3's own reading of a System One
//! question (d3_format.py and vLLM's `D3Strategy`):
//! - a `noul` without criteria is a `predicate`; one with criteria is a boolean `choice`
//!   listing `false` first, each option described by its criterion or by d3's default
//!   (`No / false`, `Yes / true`), as d3 lays out a described noul;
//! - a `choice` keeps its labels as values and its descriptions; a `score` sends its level
//!   texts as labels, in order;
//! - a non-string state, instruction or description is sent as JSON text with Python's
//!   separators, the way d3 renders one; a null instruction is sent empty so d3 asks its
//!   default question, and an empty state is sent as d3's `(empty)`.
//!
//! The answers then take the same shapes as GLM-5.3-Flash's: probabilities in option order,
//! the `temperature` (1 unless the request sets one, applied as `softmax(ln p / T)`),
//! TypeSafe's confidence, and an `option_mass` of 1 (a classification head puts all of its
//! probability on the offered options; vLLM reports the same `label_mass`).

use super::{
    answer_body, calibrated_probabilities, local_failure, render_description, to_python_json,
    Answer, Entries, Lowered, Question, SystemOneRequest, Upstream, UsageTotals,
};
use crate::inference::{AttemptFailureKind, AttemptStage, AttemptTerminal, CompletionEvidence};
use crate::web::openai::CompletionUsage;
use crate::ApiError;
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use tracing::{debug, error};

const DECISIONS_PATH: &str = "/v1/decisions";

/// d3's texts for the options of a `noul` whose criteria do not describe them.
const NOUL_FALSE: &str = "No / false";
const NOUL_TRUE: &str = "Yes / true";
/// d3's text for an empty state.
const EMPTY_STATE: &str = "(empty)";

/// How a question was asked upstream, to read its answer back in `Lowered` option order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// A `noul` without criteria: one probability that it is true.
    Predicate,
    /// A `noul` with criteria: a boolean choice, `false` first.
    Noul,
    Choice,
    Score,
}

/// What one decision-model request shares.
pub(super) struct DecisionCall<'a> {
    pub(super) upstream: Upstream<'a>,
    pub(super) provider_model_id: &'a str,
    pub(super) temperature: Option<f64>,
}

impl DecisionCall<'_> {
    /// Answers every question of `request` (lowered and validated as `questions`) with one
    /// upstream request. Usage is billed once the provider answers, even if the answers
    /// cannot be read; no partial answers are returned.
    pub(super) async fn answer_all(
        &self,
        request: &SystemOneRequest,
        questions: Vec<Lowered>,
    ) -> Result<(Entries<Answer>, UsageTotals), ApiError> {
        let (body, asked) = build_request(self.provider_model_id, request).map_err(|e| {
            error!(
                "Failed to render system one decisions request: {}",
                crate::log_redaction::JsonErrorSummary(&e)
            );
            ApiError::InternalServerError
        })?;
        let mut totals = UsageTotals::default();
        let outcome = self
            .upstream
            .send(DECISIONS_PATH, body, 0, &mut totals, decision_usage)
            .await?;
        let distributions = match read_answers(&outcome.body, &asked, &questions) {
            Ok(distributions) => {
                self.upstream.observe(
                    AttemptTerminal::Completed {
                        attempt: outcome.attempt,
                        evidence: CompletionEvidence::NonStreamingResponse,
                    },
                    outcome.probe,
                );
                distributions
            }
            Err(read_error) => {
                error!("system one decisions answer unusable: {read_error:?}");
                let (failure, api_error) = local_failure(
                    AttemptFailureKind::InvalidResponse,
                    AttemptStage::ResponseBody,
                );
                self.upstream.observe(
                    AttemptTerminal::Failed {
                        attempt: outcome.attempt,
                        failure,
                    },
                    outcome.probe,
                );
                return Err(api_error);
            }
        };
        let temperature = self.temperature.unwrap_or(1.0);
        let answers = questions
            .iter()
            .zip(distributions)
            .map(|(question, probabilities)| {
                (
                    question.name.clone(),
                    answer(question, &probabilities, temperature),
                )
            })
            .collect();
        debug!(
            "system one answered {} questions with {}",
            questions.len(),
            self.provider_model_id
        );
        Ok((Entries(answers), totals))
    }
}

/// The System One answer for one question from the provider's probabilities (in `Lowered`
/// option order), with the temperature applied as `softmax(ln p / T)`: `T = 1` renormalizes
/// the provider's distribution and leaves it as it is.
fn answer(question: &Lowered, probabilities: &[f64], temperature: f64) -> Answer {
    let log_probabilities: Vec<f64> = probabilities.iter().map(|p| p.ln()).collect();
    let probabilities = calibrated_probabilities(&log_probabilities, temperature);
    answer_body(question, &probabilities, temperature, 1.0)
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// The `/v1/decisions` body for `request`, whose questions `prepare_request` has validated,
/// and how each question was asked.
fn build_request(
    provider_model_id: &str,
    request: &SystemOneRequest,
) -> Result<(Map<String, Value>, Vec<Asked>), serde_json::Error> {
    let mut questions = Vec::with_capacity(request.questions.0.len());
    let mut asked = Vec::with_capacity(request.questions.0.len());
    for (name, question) in &request.questions.0 {
        let (value, how) = decision_question(name, question)?;
        questions.push(value);
        asked.push(how);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(provider_model_id));
    body.insert("input".into(), json!(state_input(&request.state)));
    body.insert("questions".into(), Value::Array(questions));
    Ok((body, asked))
}

fn decision_question(name: &str, question: &Question) -> Result<(Value, Asked), serde_json::Error> {
    Ok(match question {
        Question::Noul {
            instructions,
            criteria,
        } => {
            let criteria = criteria.as_ref();
            let yes = criterion(criteria.and_then(|c| c.yes.as_ref()))?;
            let no = criterion(criteria.and_then(|c| c.no.as_ref()))?;
            if yes.is_none() && no.is_none() {
                (
                    json!({
                        "type": "predicate",
                        "name": name,
                        "instructions": instructions_text(instructions)?,
                    }),
                    Asked::Predicate,
                )
            } else {
                (
                    json!({
                        "type": "choice",
                        "name": name,
                        "instructions": instructions_text(instructions)?,
                        "choices": [
                            {"value": false, "description": no.as_deref().unwrap_or(NOUL_FALSE)},
                            {"value": true, "description": yes.as_deref().unwrap_or(NOUL_TRUE)},
                        ],
                    }),
                    Asked::Noul,
                )
            }
        }
        Question::Choice {
            instructions,
            criteria,
        } => {
            let choices = criteria
                .0
                .iter()
                .map(|(label, description)| {
                    let mut choice = json!({ "value": label });
                    if let Some(description) = criterion(Some(description))? {
                        choice["description"] = Value::String(description);
                    }
                    Ok(choice)
                })
                .collect::<Result<Vec<_>, serde_json::Error>>()?;
            (
                json!({
                    "type": "choice",
                    "name": name,
                    "instructions": instructions_text(instructions)?,
                    "choices": choices,
                }),
                Asked::Choice,
            )
        }
        Question::Score {
            instructions,
            criteria,
        } => {
            let levels = criteria
                .iter()
                .map(|level| describe(level).map(|label| json!({ "label": label })))
                .collect::<Result<Vec<_>, serde_json::Error>>()?;
            (
                json!({
                    "type": "score",
                    "name": name,
                    "instructions": instructions_text(instructions)?,
                    "levels": levels,
                }),
                Asked::Score,
            )
        }
    })
}

/// d3's text for a value: a string as it is, anything else as JSON with Python's separators.
fn describe(value: &Value) -> Result<String, serde_json::Error> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => to_python_json(other),
    }
}

/// A question's instructions; empty for null, which d3 reads as its default question.
fn instructions_text(value: &Value) -> Result<String, serde_json::Error> {
    match value {
        Value::Null => Ok(String::new()),
        other => describe(other),
    }
}

/// A criterion's text, or `None` when the caller left it out (null or blank), by the same
/// rule GLM-5.3-Flash's lowering uses.
fn criterion(value: Option<&Value>) -> Result<Option<String>, serde_json::Error> {
    match value {
        Some(value) if render_description(value).is_some() => describe(value).map(Some),
        _ => Ok(None),
    }
}

/// The state as d3 renders it: a JSON string as its text (`(empty)` when empty), anything
/// else as the caller's JSON in the caller's key order with Python's separators (`", "` and
/// `": "`) in place of insignificant whitespace.
fn state_input(state: &RawValue) -> String {
    let text = state.get().trim();
    if text.starts_with('"') {
        if let Ok(text) = serde_json::from_str::<String>(text) {
            return if text.is_empty() {
                EMPTY_STATE.to_string()
            } else {
                text
            };
        }
    }
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => out.push_str(", "),
            ':' => out.push_str(": "),
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ReadError {
    /// No `answers` array, or not one answer per question.
    Answers,
    /// The model declined the question at this position.
    Refused(usize),
    /// The answer at this position does not have the shape of what was asked.
    Shape(usize),
}

/// The probabilities of each answer, in `Lowered` option order (a `noul` is true, then
/// false). Answers are matched by position and must echo what was asked: the type, the
/// option values in order, and one finite non-negative probability per option.
fn read_answers(
    body: &Value,
    asked: &[Asked],
    questions: &[Lowered],
) -> Result<Vec<Vec<f64>>, ReadError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_array)
        .filter(|answers| answers.len() == asked.len() && answers.len() == questions.len())
        .ok_or(ReadError::Answers)?;
    answers
        .iter()
        .zip(asked)
        .zip(questions)
        .enumerate()
        .map(|(index, ((answer, how), question))| {
            read_answer(answer, *how, question).map_err(|refused| {
                if refused {
                    ReadError::Refused(index)
                } else {
                    ReadError::Shape(index)
                }
            })
        })
        .collect()
}

/// One answer's probabilities; `Err(true)` for a refusal, `Err(false)` for any other shape.
fn read_answer(answer: &Value, how: Asked, question: &Lowered) -> Result<Vec<f64>, bool> {
    let kind = answer.get("type").and_then(Value::as_str);
    if kind == Some("refusal") {
        return Err(true);
    }
    let expected = match how {
        Asked::Predicate => "predicate",
        Asked::Noul | Asked::Choice => "choice",
        Asked::Score => "score",
    };
    if kind != Some(expected) {
        return Err(false);
    }
    let probabilities = match how {
        Asked::Predicate => {
            let p = answer
                .get("probability")
                .and_then(Value::as_f64)
                .filter(|p| (0.0..=1.0).contains(p))
                .ok_or(false)?;
            vec![p, 1.0 - p]
        }
        Asked::Noul => {
            // Asked as [false, true]; the answer reads true first.
            let read = option_probabilities(answer, &[json!(false), json!(true)])?;
            vec![read[1], read[0]]
        }
        Asked::Choice => {
            let values: Vec<Value> = question
                .options
                .iter()
                .map(|(label, _)| json!(label))
                .collect();
            option_probabilities(answer, &values)?
        }
        Asked::Score => {
            let values: Vec<Value> = (0..question.options.len()).map(|i| json!(i)).collect();
            option_probabilities(answer, &values)?
        }
    };
    if probabilities.len() != question.options.len()
        || probabilities.iter().any(|p| !p.is_finite() || *p < 0.0)
        || probabilities.iter().sum::<f64>() <= 0.0
    {
        return Err(false);
    }
    Ok(probabilities)
}

/// The `probabilities` of a choice or score answer, which must list exactly `values` in order.
fn option_probabilities(answer: &Value, values: &[Value]) -> Result<Vec<f64>, bool> {
    let entries = answer
        .get("probabilities")
        .and_then(Value::as_array)
        .filter(|entries| entries.len() == values.len())
        .ok_or(false)?;
    entries
        .iter()
        .zip(values)
        .map(|(entry, value)| {
            if entry.get("value") != Some(value) {
                return Err(false);
            }
            entry
                .get("probability")
                .and_then(Value::as_f64)
                .ok_or(false)
        })
        .collect()
}

/// Usage as Tinfoil reports it: every question's prompt counted (each one re-reads the
/// state) and no output tokens.
fn decision_usage(body: &Value) -> Option<CompletionUsage> {
    let usage = body.get("usage")?;
    let tokens = |value: Option<&Value>| {
        value
            .and_then(Value::as_i64)
            .filter(|tokens| *tokens >= 0)
            .map(|tokens| tokens.min(i64::from(i32::MAX)) as i32)
    };
    let prompt_tokens = tokens(usage.get("input_tokens"))?;
    Some(CompletionUsage {
        prompt_tokens,
        completion_tokens: tokens(usage.get("output_tokens")).unwrap_or(0),
        cached_prompt_tokens: tokens(
            usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens")),
        )
        .map(|cached| cached.min(prompt_tokens)),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{lower_questions, parse_request, prepare_request, Kind};
    use super::*;
    use crate::provider_client::{ProviderClient, ProviderRequest};
    use crate::proxy_config::ProxyConfig;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use reqwest::Method;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn request(json: Value) -> SystemOneRequest {
        parse_request(&json.to_string()).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A request from JSON text, keeping its key order (`json!` would sort the keys).
    fn request_text(text: &str) -> SystemOneRequest {
        parse_request(text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn lowered(request: &SystemOneRequest) -> Vec<Lowered> {
        lower_questions(&request.questions, 26).unwrap()
    }

    fn answer_json(answer: &Answer) -> Value {
        serde_json::to_value(answer).unwrap()
    }

    #[test]
    fn questions_are_asked_as_d3_reads_them() {
        let request = request_text(
            r#"{
            "model": "decision-medium",
            "state": {"ticket": "Charged twice", "meta": {"b": 1, "a": [1, 2]}},
            "questions": {
                "urgent": {"type": "noul", "instructions": "Is it urgent?"},
                "churn": {
                    "type": "noul",
                    "instructions": "Will they leave?",
                    "criteria": {"true": "they say they will cancel", "false": ""}
                },
                "intent": {
                    "type": "choice",
                    "instructions": {"ask": "What do they want?"},
                    "criteria": {"refund": "money back", "question": null, "praise": {"tone": "thanks"}}
                },
                "severity": {"type": "score", "instructions": null, "criteria": ["low", {"level": "high"}]}
            }
        }"#,
        );
        let (body, asked) = build_request("decision-medium", &request).unwrap();
        assert_eq!(
            asked,
            [Asked::Predicate, Asked::Noul, Asked::Choice, Asked::Score]
        );
        assert_eq!(
            Value::Object(body),
            json!({
                "model": "decision-medium",
                "input": "{\"ticket\": \"Charged twice\", \"meta\": {\"b\": 1, \"a\": [1, 2]}}",
                "questions": [
                    {"type": "predicate", "name": "urgent", "instructions": "Is it urgent?"},
                    {"type": "choice", "name": "churn", "instructions": "Will they leave?",
                     "choices": [
                         {"value": false, "description": "No / false"},
                         {"value": true, "description": "they say they will cancel"}
                     ]},
                    {"type": "choice", "name": "intent", "instructions": "{\"ask\": \"What do they want?\"}",
                     "choices": [
                         {"value": "refund", "description": "money back"},
                         {"value": "question"},
                         {"value": "praise", "description": "{\"tone\": \"thanks\"}"}
                     ]},
                    {"type": "score", "name": "severity", "instructions": "",
                     "levels": [{"label": "low"}, {"label": "{\"level\": \"high\"}"}]}
                ]
            })
        );
    }

    #[test]
    fn state_keeps_caller_order_and_takes_python_separators() {
        let raw = |text: &str| RawValue::from_string(text.to_string()).unwrap();
        assert_eq!(
            state_input(&raw(
                "{\n  \"z\" : 1,\n  \"a\":[true ,null],\"s\":\"x, y: z\\\" ,:\"\n}"
            )),
            "{\"z\": 1, \"a\": [true, null], \"s\": \"x, y: z\\\" ,:\"}"
        );
        assert_eq!(
            state_input(&raw("\"plain text\\nline\"")),
            "plain text\nline"
        );
        assert_eq!(state_input(&raw("\"\"")), "(empty)");
        assert_eq!(state_input(&raw("[ ]")), "[]");
        assert_eq!(state_input(&raw("42")), "42");
    }

    /// Four questions, one of each way of asking, in this order.
    const FOUR_QUESTIONS: &str = r#"{
            "model": "decision-small",
            "state": "x",
            "questions": {
                "plain": {"type": "noul", "instructions": "a?"},
                "described": {"type": "noul", "instructions": "b?", "criteria": {"true": "yes it is"}},
                "pick": {"type": "choice", "instructions": "c?", "criteria": {"x": null, "y": null, "z": null}},
                "rate": {"type": "score", "instructions": "d?", "criteria": ["lo", "mid", "hi"]}
            }
        }"#;

    fn noul_answers() -> (Vec<Asked>, Vec<Lowered>) {
        let request = request_text(FOUR_QUESTIONS);
        let (_, asked) = build_request("decision-small", &request).unwrap();
        (asked, lowered(&request))
    }

    fn provider_answers() -> Value {
        json!({
            "model": "decision-small",
            "answers": [
                {"type": "predicate", "name": "plain", "probability": 0.75},
                {"type": "choice", "name": "described", "choice": true, "confidence": 0.9,
                 "probabilities": [{"value": false, "probability": 0.1}, {"value": true, "probability": 0.9}]},
                {"type": "choice", "name": "pick", "choice": "y", "confidence": 0.5,
                 "probabilities": [{"value": "x", "probability": 0.2}, {"value": "y", "probability": 0.5},
                                   {"value": "z", "probability": 0.3}]},
                {"type": "score", "name": "rate", "score": 1.4, "confidence": 0.5,
                 "probabilities": [{"value": 0, "label": "lo", "probability": 0.1},
                                   {"value": 1, "label": "mid", "probability": 0.4},
                                   {"value": 2, "label": "hi", "probability": 0.5}]}
            ],
            "usage": {"input_tokens": 452, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                      "output_tokens": 0, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 452}
        })
    }

    #[test]
    fn answers_take_the_system_one_shapes() {
        let (asked, questions) = noul_answers();
        let distributions = read_answers(&provider_answers(), &asked, &questions).unwrap();
        assert_eq!(
            distributions,
            [
                vec![0.75, 0.25],
                vec![0.9, 0.1],
                vec![0.2, 0.5, 0.3],
                vec![0.1, 0.4, 0.5]
            ]
        );
        let answers: Vec<Value> = questions
            .iter()
            .zip(&distributions)
            .map(|(question, probabilities)| answer_json(&answer(question, probabilities, 1.0)))
            .collect();
        assert_eq!(
            answers[0],
            json!({"type": "noul", "noul": 0.75, "temperature": 1.0, "option_mass": 1.0})
        );
        assert_eq!(answers[1]["noul"], 0.9);
        assert_eq!(
            answers[2],
            json!({"type": "choice", "choice": "y", "probabilities": {"x": 0.2, "y": 0.5, "z": 0.3},
                   "confidence": 0.25, "temperature": 1.0, "option_mass": 1.0})
        );
        assert_eq!(answers[3]["score"], 1.4);
        assert_eq!(
            answers[3]["legend"],
            json!({"0": "lo", "1": "mid", "2": "hi"})
        );
        assert_eq!(
            answers[3]["probabilities"],
            json!({"0": 0.1, "1": 0.4, "2": 0.5})
        );
        // TypeSafe's score confidence, not the provider's top probability.
        assert_ne!(answers[3]["confidence"], 0.5);
    }

    #[test]
    fn a_requested_temperature_reshapes_without_changing_the_winner() {
        let (asked, questions) = noul_answers();
        let distributions = read_answers(&provider_answers(), &asked, &questions).unwrap();
        let sharp = answer_json(&answer(&questions[2], &distributions[2], 0.5));
        assert_eq!(sharp["choice"], "y");
        assert_eq!(sharp["temperature"], 0.5);
        // softmax(ln p / 0.5) = p^2 / sum p^2 = 0.25 / 0.38
        assert_eq!(sharp["probabilities"]["y"], 0.6579);
        let raw = answer_json(&answer(&questions[0], &[0.6, 0.4], 2.0));
        // sqrt(0.6) / (sqrt(0.6) + sqrt(0.4))
        assert_eq!(raw["noul"], 0.5505);
    }

    #[test]
    fn answers_must_echo_what_was_asked() {
        let (asked, questions) = noul_answers();
        let read = |body: Value| read_answers(&body, &asked, &questions);
        let mut body = provider_answers();
        body["answers"].as_array_mut().unwrap().pop();
        assert_eq!(read(body), Err(ReadError::Answers));
        assert_eq!(read(json!({"usage": {}})), Err(ReadError::Answers));

        let mut body = provider_answers();
        body["answers"][1] = json!({"type": "refusal", "name": "described"});
        assert_eq!(read(body), Err(ReadError::Refused(1)));

        let mut body = provider_answers();
        body["answers"][0]["type"] = json!("choice");
        assert_eq!(read(body), Err(ReadError::Shape(0)));

        let mut body = provider_answers();
        body["answers"][0]["probability"] = json!(1.5);
        assert_eq!(read(body), Err(ReadError::Shape(0)));

        let mut body = provider_answers();
        body["answers"][2]["probabilities"][1]["value"] = json!("z");
        assert_eq!(read(body), Err(ReadError::Shape(2)));

        let mut body = provider_answers();
        body["answers"][3]["probabilities"][0]["value"] = json!(2);
        assert_eq!(read(body), Err(ReadError::Shape(3)));

        let mut body = provider_answers();
        body["answers"][2]["probabilities"][0]["probability"] = json!(-0.1);
        assert_eq!(read(body), Err(ReadError::Shape(2)));

        let mut body = provider_answers();
        for entry in body["answers"][3]["probabilities"].as_array_mut().unwrap() {
            entry["probability"] = json!(0.0);
        }
        assert_eq!(read(body), Err(ReadError::Shape(3)));
    }

    #[test]
    fn usage_counts_every_questions_prompt() {
        let usage = |body: Value| {
            decision_usage(&body).map(|usage| {
                (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cached_prompt_tokens,
                )
            })
        };
        assert_eq!(usage(provider_answers()), Some((452, 0, Some(0))));
        assert_eq!(usage(json!({"answers": []})), None);
        assert_eq!(
            usage(
                json!({"usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 99}}})
            ),
            Some((10, 0, Some(10)))
        );
    }

    #[test]
    fn decision_models_take_text_and_at_most_26_options() {
        use super::super::SystemOneRequestError as E;
        let prepare = |json: Value| prepare_request(&request(json)).err();
        let options = |n: usize| -> Value {
            Value::Object((0..n).map(|i| (format!("o{i}"), json!(null))).collect())
        };
        let choice = |n: usize| json!({"q": {"type": "choice", "instructions": "?", "criteria": options(n)}});
        for model in ["decision-small", "decision-medium", "decision-large"] {
            assert_eq!(
                prepare(json!({"model": model, "state": "s", "questions": choice(26)})),
                None
            );
            assert_eq!(
                prepare(json!({"model": model, "state": "s", "questions": choice(27)})),
                Some(E::BadOptionCount(26))
            );
            assert_eq!(
                prepare(json!({"model": model, "state": "s", "questions": choice(2),
                               "images": ["data:image/png;base64,AAAA"]})),
                Some(E::ImagesUnsupported)
            );
            assert_eq!(
                prepare(json!({"model": model, "state": "s",
                               "questions": {"q": {"type": "score", "instructions": "?", "criteria": ["a", " "]}}})),
                Some(E::EmptyOptionLabel)
            );
            assert_eq!(
                prepare(json!({"model": model, "state": "s",
                               "questions": {"q": {"type": "score", "instructions": "?", "criteria": ["a", "a"]}}})),
                Some(E::DuplicateOption)
            );
            // A state that fits the 32,000-token state limit but not, with a question, the
            // model's 32,768-token context.
            let state = "word ".repeat(31_900);
            assert_eq!(
                prepare(json!({"model": model, "state": state, "questions": choice(2)})),
                Some(E::ContextExceeded(32_768))
            );
        }
        // The same requests stay valid on GLM-5.3-Flash.
        for questions in [
            choice(255),
            json!({"q": {"type": "score", "instructions": "?", "criteria": ["a", "a"]}}),
        ] {
            assert_eq!(prepare(json!({"state": "s", "questions": questions})), None);
        }
        assert_eq!(
            prepare(
                json!({"state": "s", "questions": choice(2), "images": ["data:image/png;base64,AAAA"]})
            ),
            None
        );
        assert_eq!(
            prepare(json!({"model": "decision-xl", "state": "s", "questions": choice(2)})),
            Some(E::UnsupportedModel)
        );
    }

    #[tokio::test]
    async fn sends_one_decisions_request_through_the_tinfoil_transport() {
        type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
        let seen: Seen = Arc::default();
        let recorded = Arc::clone(&seen);
        let router = Router::new().route(
            DECISIONS_PATH,
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    recorded.lock().unwrap().push((headers, body));
                    Json(provider_answers())
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = ProviderClient::for_test(base_url.clone()).unwrap();
        let proxy = ProxyConfig {
            base_url,
            api_key: None,
            provider_name: "tinfoil".to_owned(),
        };

        let request = request_text(FOUR_QUESTIONS);
        let (mut body, asked) = build_request("decision-small", &request).unwrap();
        // As `Upstream::send` does before every send.
        crate::web::openai::apply_provider_managed_request_fields(
            &mut body,
            &proxy.provider_name,
            uuid::Uuid::nil(),
            &crate::web::openai::CompletionCachePolicy::LegacyV1,
        );
        let response = client
            .send(
                &proxy,
                ProviderRequest::new(Method::POST, DECISIONS_PATH, Duration::from_secs(5))
                    .content_type("application/json")
                    .body(serde_json::to_vec(&Value::Object(body.clone())).unwrap()),
            )
            .await
            .unwrap();
        assert!(response.is_success());
        let answered: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            read_answers(&answered, &asked, &lowered(&request))
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            decision_usage(&answered).map(|usage| usage.prompt_tokens),
            Some(452)
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (headers, sent) = &seen[0];
        assert_eq!(headers["authorization"], "Bearer test-tinfoil-key");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(sent, &Value::Object(body));
        // Tinfoil's decision enclave rejects fields it does not know. Its router consumes
        // `user_cache_secret` (the account's cache namespace, as for chat) before forwarding;
        // nothing else is sent.
        let mut keys: Vec<&str> = sent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["input", "model", "questions", "user_cache_secret"]);
        task.abort();
    }

    #[test]
    fn noul_kinds_lower_in_the_order_answers_are_read() {
        let request = request(
            json!({"state": "x", "questions": {"n": {"type": "noul", "instructions": "?"}}}),
        );
        let questions = lowered(&request);
        assert_eq!(questions[0].kind, Kind::Noul);
        assert_eq!(questions[0].options[0].0, "true");
        assert_eq!(questions[0].options[1].0, "false");
    }
}
