//! Dormant primitives for the second version of the attested transport.
//!
//! This module deliberately contains no HTTP routing, application route table,
//! authentication state machine, or memory scheduler. The active gateway is a
//! separate stacked change.

pub(crate) mod crypto;
pub(crate) mod diagnostics;
pub(crate) mod envelope;
pub(crate) mod framing;
pub(crate) mod gateway;
pub(crate) mod session;
