//! Messages between the server and its workers, exchanged as JSON over HTTP.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const START_PATH: &str = "/api/worker/start";
pub const CLAIM_PATH: &str = "/api/worker/claim";
pub const HEARTBEAT_PATH: &str = "/api/worker/heartbeat";
pub const REPORT_PATH: &str = "/api/worker/report";

/// What a worker reports about itself with every claim and heartbeat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub systems: Vec<String>,
    pub features: Vec<String>,
    /// Builds the worker runs at once at most.
    pub max_jobs: u32,
    /// Leases of the builds the worker is running.
    pub running: Vec<Uuid>,
    /// Leases of the builds whose outputs the worker is uploading.
    pub uploading: Vec<Uuid>,
    pub load: Load,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Load {
    pub cpus: u32,
    /// Idle CPU time over the last `window_secs`, in cores.
    pub idle_cores: f64,
    pub window_secs: u64,
    pub mem_available_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaimResponse {
    Job(Job),
    /// No build is ready. Ask again right away.
    Idle,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Job {
    pub lease: Uuid,
    pub drv_path: String,
}

/// What a worker resumes after it started.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StartResponse {
    /// Builds the worker built before it stopped, but whose outputs it had
    /// not finished uploading. The outputs are still in its store.
    pub uploads: Vec<Upload>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Upload {
    pub lease: Uuid,
    pub drv_path: String,
    pub outputs: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Leases the worker must stop building, because nothing needs them anymore.
    pub abort: Vec<Uuid>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub lease: Uuid,
    pub event: BuildEvent,
}

/// A step of a build. A build that succeeds reports `Built`, then `Uploaded`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildEvent {
    /// Built. The worker is uploading the outputs, and can build their
    /// dependents meanwhile.
    Built {
        outputs: BTreeMap<String, String>,
        inputs: Inputs,
    },
    /// Every output is in the binary cache.
    Uploaded,
    /// The derivation's build failed. Retrying it is not expected to help.
    Failed {
        error: String,
        log_tail: String,
        inputs: Inputs,
    },
    /// The build could not be attempted or published. The server retries it.
    Transient { cause: Transient, error: String },
}

/// Where the inputs of a build came from, which tells the time spent
/// fetching them apart from the build itself.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Inputs {
    /// Every input was in the worker's store already.
    Local,
    /// Nix could substitute missing inputs during the build.
    Substituted,
}

impl Inputs {
    pub fn as_str(self) -> &'static str {
        match self {
            Inputs::Local => "local",
            Inputs::Substituted => "substituted",
        }
    }
}

/// Why an attempt at a build ended without a result.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transient {
    /// The derivation was not in the worker's store.
    MissingDerivation,
    /// The worker could not run the build, e.g. `nix` did not start.
    WorkerError,
    /// Pushing the outputs to the cache failed.
    UploadFailed,
    /// The worker stopped sending heartbeats. Only the server reports this.
    WorkerLost,
}

impl Transient {
    pub fn as_str(self) -> &'static str {
        match self {
            Transient::MissingDerivation => "missing_derivation",
            Transient::WorkerError => "worker_error",
            Transient::UploadFailed => "upload_failed",
            Transient::WorkerLost => "worker_lost",
        }
    }
}
