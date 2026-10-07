//! System One: typed decisions with calibrated probabilities (`POST /v1/systemone`).
//!
//! A request carries a JSON `state` and named questions of three types. Each
//! question is answered with one masked single-token decision on Continuum's
//! GLM-5.3-Flash, so the response holds probabilities rather than text:
//!
//! - `noul`: the probability that the statement is true;
//! - `choice`: a distribution over the options, in the order they were given;
//! - `score`: the expected level on an ordered scale, with the per-level
//!   distribution and a legend keyed by level number.
//!
//! Choice options are numbered in the order the caller lists them, so the SDK
//! keeps that order ([`OrderedEntries`]) instead of sorting keys as a map would.

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;

/// A JSON object whose entries keep the order they were given in.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedEntries<V>(pub Vec<(String, V)>);

impl<V> OrderedEntries<V> {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(&mut self, key: impl Into<String>, value: V) {
        self.0.push((key.into(), value));
    }

    /// The first value stored under `key`.
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<V> Default for OrderedEntries<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> From<Vec<(String, V)>> for OrderedEntries<V> {
    fn from(entries: Vec<(String, V)>) -> Self {
        Self(entries)
    }
}

impl<K: Into<String>, V> FromIterator<(K, V)> for OrderedEntries<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl<V: Serialize> Serialize for OrderedEntries<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedEntries<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for EntriesVisitor<V> {
            type Value = OrderedEntries<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(entry) = map.next_entry::<String, V>()? {
                    entries.push(entry);
                }
                Ok(OrderedEntries(entries))
            }
        }

        deserializer.deserialize_map(EntriesVisitor(std::marker::PhantomData))
    }
}

/// What `true` and `false` mean for a noul question, when the words alone are
/// not enough.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemOneNoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub when_true: Option<Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub when_false: Option<Value>,
}

/// One question about the request's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SystemOneQuestion {
    /// A yes/no statement; answered with the probability that it is true.
    Noul {
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<SystemOneNoulCriteria>,
    },
    /// One of 2 to 255 options, numbered in the order given. Each value
    /// describes its option (`null` for no description).
    Choice {
        instructions: Value,
        criteria: OrderedEntries<Value>,
    },
    /// A level on an ordered scale of 2 to 10 entries, lowest first.
    Score {
        instructions: Value,
        criteria: Vec<Value>,
    },
}

impl SystemOneQuestion {
    pub fn noul(instructions: impl Into<String>) -> Self {
        Self::Noul {
            instructions: Value::String(instructions.into()),
            criteria: None,
        }
    }

    pub fn choice<K, D>(
        instructions: impl Into<String>,
        options: impl IntoIterator<Item = (K, D)>,
    ) -> Self
    where
        K: Into<String>,
        D: Into<Value>,
    {
        Self::Choice {
            instructions: Value::String(instructions.into()),
            criteria: options
                .into_iter()
                .map(|(label, description)| (label, description.into()))
                .collect(),
        }
    }

    pub fn score<L: Into<Value>>(
        instructions: impl Into<String>,
        levels: impl IntoIterator<Item = L>,
    ) -> Self {
        Self::Score {
            instructions: Value::String(instructions.into()),
            criteria: levels.into_iter().map(Into::into).collect(),
        }
    }
}

/// A System One request. Up to 64 questions; see [`SystemOneQuestion`] for the
/// per-type limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneRequest {
    /// Defaults to the only supported model, `glm-5-3-flash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Any JSON; embedded in the prompt as given.
    pub state: Value,
    pub questions: OrderedEntries<SystemOneQuestion>,
    /// Up to 4 `data:` image URLs, 4 MiB each and 8 MiB in total, shown with the state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Calibration temperature override; `1` returns the raw probabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

impl SystemOneRequest {
    pub fn new(state: Value) -> Self {
        Self {
            model: None,
            state,
            questions: OrderedEntries::new(),
            images: Vec::new(),
            temperature: None,
        }
    }

    pub fn with_question(mut self, name: impl Into<String>, question: SystemOneQuestion) -> Self {
        self.questions.push(name, question);
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_image(mut self, data_url: impl Into<String>) -> Self {
        self.images.push(data_url.into());
        self
    }

    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// The answer to one question. Every variant reports the calibration
/// `temperature` applied and `option_mass`, the share of the model's next-token
/// probability that fell on the offered options (low values mean the prompt did
/// not fit the question well).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SystemOneAnswer {
    Noul {
        /// Probability that the statement is true.
        noul: f64,
        temperature: f64,
        option_mass: f64,
    },
    Choice {
        /// The most probable option's label.
        choice: String,
        /// Probability per option label, in the order the options were given.
        probabilities: OrderedEntries<f64>,
        /// Peakedness of the distribution, from 0 (uniform) to 1 (certain).
        confidence: f64,
        temperature: f64,
        option_mass: f64,
    },
    Score {
        /// Expected level number (0-based), a probability-weighted average.
        score: f64,
        /// Level text keyed by level number.
        legend: OrderedEntries<Value>,
        /// Probability per level number.
        probabilities: OrderedEntries<f64>,
        confidence: f64,
        temperature: f64,
        option_mass: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Upstream inference requests made: one per question, more for wide choices.
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneResponse {
    pub id: String,
    pub model: String,
    /// One answer per question, keyed by the question's name, in question order.
    pub answers: OrderedEntries<SystemOneAnswer>,
    pub usage: SystemOneUsage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::json_inference_request;
    use serde_json::json;

    #[test]
    fn request_serializes_questions_and_options_in_caller_order() {
        let request = SystemOneRequest::new(json!({"ticket": "Charged twice"}))
            .with_question("zebra", SystemOneQuestion::noul("Is it urgent?"))
            .with_question(
                "intent",
                SystemOneQuestion::choice(
                    "What does the customer want?",
                    [("refund", "money back"), ("10", "ten"), ("2", "two")],
                ),
            )
            .with_question(
                "frustration",
                SystemOneQuestion::score("How frustrated?", ["Low", "High"]),
            )
            .with_temperature(1.0);

        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"state":{"ticket":"Charged twice"},"questions":{"zebra":{"type":"noul","instructions":"Is it urgent?"},"intent":{"type":"choice","instructions":"What does the customer want?","criteria":{"refund":"money back","10":"ten","2":"two"}},"frustration":{"type":"score","instructions":"How frustrated?","criteria":["Low","High"]}},"temperature":1.0}"#
        );

        let http =
            json_inference_request("/v1/systemone", http::Method::POST, Some(&request)).unwrap();
        assert_eq!(http.method(), http::Method::POST);
        assert_eq!(http.uri(), "/v1/systemone");
        assert_eq!(http.headers()["content-type"], "application/json");
        assert_eq!(http.body(), encoded.as_bytes());
    }

    #[test]
    fn noul_criteria_and_images_serialize_when_present() {
        let request = SystemOneRequest::new(json!("state"))
            .with_question(
                "ok",
                SystemOneQuestion::Noul {
                    instructions: json!("Is it fine?"),
                    criteria: Some(SystemOneNoulCriteria {
                        when_true: Some(json!("all good")),
                        when_false: None,
                    }),
                },
            )
            .with_image("data:image/png;base64,AAAA")
            .with_model("glm-5-3-flash");
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "model": "glm-5-3-flash",
                "state": "state",
                "questions": {"ok": {"type": "noul", "instructions": "Is it fine?", "criteria": {"true": "all good"}}},
                "images": ["data:image/png;base64,AAAA"]
            })
        );
    }

    #[test]
    fn response_parses_all_three_answer_shapes_in_order() {
        let response: SystemOneResponse = serde_json::from_str(
            r#"{"id":"so_1","model":"glm-5-3-flash","answers":{
                "is_urgent":{"type":"noul","noul":0.9318,"temperature":2.483,"option_mass":1.0},
                "intent":{"type":"choice","choice":"refund","probabilities":{"refund":0.8611,"10":0.1,"2":0.0389},"confidence":0.6,"temperature":2.407,"option_mass":0.9952},
                "frustration":{"type":"score","score":1.2,"legend":{"0":"Low","1":"High"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.03,"temperature":2.48,"option_mass":0.99}
            },"usage":{"input_tokens":388,"output_tokens":3,"cached_tokens":0,"requests":3}}"#,
        )
        .unwrap();

        let names: Vec<&str> = response.answers.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["is_urgent", "intent", "frustration"]);
        match response.answers.get("intent").unwrap() {
            SystemOneAnswer::Choice {
                choice,
                probabilities,
                confidence,
                ..
            } => {
                assert_eq!(choice, "refund");
                let labels: Vec<&str> = probabilities.iter().map(|(label, _)| label).collect();
                assert_eq!(
                    labels,
                    ["refund", "10", "2"],
                    "option order survives integer-like labels"
                );
                assert_eq!(*confidence, 0.6);
            }
            other => panic!("unexpected answer {other:?}"),
        }
        assert!(matches!(
            response.answers.get("is_urgent"),
            Some(SystemOneAnswer::Noul { noul, .. }) if *noul == 0.9318
        ));
        assert!(matches!(
            response.answers.get("frustration"),
            Some(SystemOneAnswer::Score { score, legend, .. }) if *score == 1.2 && legend.get("1") == Some(&json!("High"))
        ));
        assert_eq!(response.usage.requests, 3);
    }
}
