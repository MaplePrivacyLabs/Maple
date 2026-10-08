#![allow(dead_code)]

use std::sync::Arc;

use pi_ai::types::{JsValue, Model};
use pi_testkit::VirtualEnv;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub fn env() -> Arc<VirtualEnv> {
    Arc::new(VirtualEnv::new(1_767_225_600_000))
}

pub fn js(value: Value) -> JsValue {
    JsValue::try_from(value).expect("test JSON retains exact JavaScript values")
}

pub fn json<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("upstream fixture matches the Rust contract")
}

pub fn fixture_model(provider: &str, id: &str) -> Model {
    let models: Value = serde_json::from_str(include_str!("../fixtures/chat-completions.json"))
        .expect("pinned model fixtures parse");
    let key = format!("{provider}/{id}");
    json(
        models
            .get(&key)
            .unwrap_or_else(|| panic!("missing pinned model fixture: {key}"))
            .clone(),
    )
}
