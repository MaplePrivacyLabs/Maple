//! Task-lived, bundled CPython execution. This crate owns processes, not host authority.
mod output;
mod process;
mod protocol;
mod resources;
mod worker;

pub use output::{BackgroundChunk, BackgroundOutput};
pub use resources::{PackagedPython, RuntimeManifest};
pub use worker::{Execution, Runtime, TaskHandle};

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

pub const MAX_WORKERS: usize = 4;
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
pub const OUTPUT_QUEUE_BYTES: usize = 256 * 1024;
pub const FOREGROUND_BYTES: usize = 64 * 1024;
pub const FINAL_BYTES: usize = 16 * 1024;
pub const BACKGROUND_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    pub startup_timeout: Duration,
    pub retirement_grace: Duration,
    pub cleanup_timeout: Duration,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(10),
            retirement_grace: Duration::from_secs(2),
            cleanup_timeout: Duration::from_secs(5),
        }
    }
}

/// Fully prepared, immutable launch configuration. No PATH discovery happens here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub python: PackagedPython,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    Empty,
    Starting,
    Idle,
    Executing,
    Retiring,
    CleanupPending,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStatus {
    pub generation: Option<u64>,
    pub phase: WorkerPhase,
    pub closed: bool,
    pub state_loss_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Holder {
    pub key: String,
    pub generation: u64,
    pub phase: WorkerPhase,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapacitySnapshot {
    pub limit: usize,
    pub holders: Vec<Holder>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetOutcome {
    pub retired_generation: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub implementation: String,
    pub version: String,
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub distribution: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Ok,
    Error,
    Cancelled,
    WorkerLost,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub generation: u64,
    pub execution_id: u64,
    pub status: OutcomeStatus,
    pub stdout: String,
    pub stderr: String,
    pub value: Option<String>,
    pub traceback: Option<String>,
    pub elapsed_ms: u64,
    pub dropped_stdout_bytes: u64,
    pub dropped_stderr_bytes: u64,
    pub background: BackgroundOutput,
    pub state_loss_reason: Option<String>,
    pub runtime: Option<RuntimeIdentity>,
    pub cleanup_pending: bool,
}

#[derive(Clone, Debug)]
pub enum Error {
    Busy,
    Cancelled,
    Retired,
    CleanupPending,
    Capacity { holders: Vec<Holder> },
    InvalidInput(String),
    LaunchMismatch,
    Unavailable(String),
    WorkerLost(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("Python is already executing a cell for this task"),
            Self::Cancelled => f.write_str("Python execution was cancelled before admission"),
            Self::Retired => f.write_str("This Python task binding has been retired"),
            Self::CleanupPending => {
                f.write_str("Python cleanup is pending; this worker still occupies capacity")
            }
            Self::Capacity { holders } => write!(
                f,
                "All {MAX_WORKERS} Python worker slots are occupied ({} retained); reset Python or close an owning session",
                holders.len()
            ),
            Self::InvalidInput(message)
            | Self::Unavailable(message)
            | Self::WorkerLost(message) => f.write_str(message),
            Self::LaunchMismatch => f.write_str(
                "The task's Python launch configuration changed; retire its old binding first",
            ),
        }
    }
}
impl std::error::Error for Error {}
