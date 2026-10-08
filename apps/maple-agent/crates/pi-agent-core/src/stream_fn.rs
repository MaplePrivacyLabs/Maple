//! The optional default stream-function slot from `stream-fn.ts`.
use crate::types::{AgentError, AgentResult, StreamFn};
use std::sync::{OnceLock, RwLock};
fn slot() -> &'static RwLock<Option<StreamFn>> {
    static SLOT: OnceLock<RwLock<Option<StreamFn>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}
pub fn set_default_stream_fn(stream: Option<StreamFn>) {
    *slot().write().expect("default stream function poisoned") = stream;
}
pub fn get_default_stream_fn() -> AgentResult<StreamFn> {
    slot().read().expect("default stream function poisoned").clone().ok_or_else(||AgentError::new("No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn()."))
}
