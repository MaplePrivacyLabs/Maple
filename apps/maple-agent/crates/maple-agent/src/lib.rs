//! Transport-neutral Maple agent runtime.
//!
//! This crate is the backend half of the Maple desktop agent flow. It runs
//! Maple's tasks on the Pi crates (`pi-ai`, `pi-agent-core`,
//! `pi-coding-agent`): the Maple provider over the OpenSecret SDK, Maple's
//! tools, and account-scoped session storage. It has no UI and no windowing
//! dependency; a caller composes [`agent::MapleAgentService`] with its own
//! [`agent::AgentEventSink`] and drives it through [`agent::AgentRuntimeHandle`]
//! method calls.

// Phase 2 moves Maple's features onto Pi one part at a time, and some of
// what the later parts use is already here. This goes once they all are.
#![allow(dead_code)]

#[cfg(feature = "acp")]
pub mod acp;
pub mod agent;
mod desktop_environment;
pub use desktop_environment::prepare_process_environment;
pub mod maple_api;
pub mod open_secret_config;
pub mod private_file;
