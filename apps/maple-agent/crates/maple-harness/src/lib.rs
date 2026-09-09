//! GPUI-free core types and validation for Maple's programmable harness.
//!
//! This crate deliberately contains no action executors and no UI objects. The
//! application owns those capabilities; this crate defines the wire contract,
//! policy, registry validation, semantic projection primitives, and bounded
//! audit/event stores shared by every transport.

#![forbid(unsafe_code)]

pub mod action;
pub mod audit;
pub mod catalog;
pub mod controller;
pub mod discovery;
pub mod host;
pub mod keymap;
pub mod policy;
pub mod registry;
pub mod semantic;

pub use action::*;
pub use audit::*;
pub use controller::*;
pub use discovery::*;
pub use host::*;
pub use keymap::*;
pub use policy::*;
pub use registry::*;
pub use semantic::*;

/// Initial wire schema version for Developer Preview contracts.
pub const SCHEMA_VERSION: u16 = 1;
