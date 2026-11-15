//! A build machine: claims builds from the server, builds them with the local
//! Nix daemon and publishes the outputs to niks3.

mod load;
mod uploader;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use uuid::Uuid;

use self::load::LoadSampler;
use self::uploader::Uploader;
use crate::api::{
    self, BuildEvent, ClaimResponse, HeartbeatResponse, Inputs, Job, Report, StartResponse,
    Transient, Upload, WorkerStatus,
};
use crate::drv;
use crate::text::read_tail;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const LOG_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub server_url: String,
    pub token_file: PathBuf,
    /// Systems this machine builds for, e.g. `x86_64-linux`.
    pub systems: Vec<String>,
    /// Nix system features this machine supports, e.g. `kvm`.
    #[serde(default)]
    pub features: Vec<String>,
    /// Builds to run at once at most. Defaults to the number of CPUs.
    pub max_jobs: Option<u32>,
    /// Arguments for `niks3 push`, such as `--server-url` and credentials.
    #[serde(default)]
    pub niks3_push_args: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

struct Worker {
    config: Config,
    max_jobs: u32,
    http: reqwest::Client,
    token: String,
    load: LoadSampler,
    uploader: Uploader,
    /// Builds in progress. Each takes a slot.
    running: Mutex<HashMap<Uuid, AbortHandle>>,
    /// Built builds whose outputs are uploading, kept until the server
    /// acknowledged the upload.
    uploading: Mutex<HashSet<Uuid>>,
    /// Signals a free slot to the claim loop, keeping the signal until the
    /// loop waits for it, so that one sent while it checks the slots counts.
    finished: Notify,
}

impl Worker {
    fn status(&self) -> WorkerStatus {
        WorkerStatus {
            systems: self.config.systems.clone(),
            features: self.config.features.clone(),
            max_jobs: self.max_jobs,
            running: self
                .running
                .lock()
                .expect("not poisoned")
                .keys()
                .copied()
                .collect(),
            uploading: self
                .uploading
                .lock()
                .expect("not poisoned")
                .iter()
                .copied()
                .collect(),
            load: self.load.current(),
        }
    }

    fn running_count(&self) -> usize {
        self.running.lock().expect("not poisoned").len()
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> anyhow::Result<T> {
        let response = self
            .http
            .post(format!(
                "{}{path}",
                self.config.server_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let token = std::fs::read_to_string(&config.token_file)
        .with_context(|| format!("reading {}", config.token_file.display()))?
        .trim()
        .to_owned();
    let load = LoadSampler::start()?;
    let worker = Arc::new(Worker {
        max_jobs: config.max_jobs.unwrap_or(load.cpus()),
        // Claims wait up to 25 seconds for a build.
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?,
        token,
        load,
        uploader: Uploader::start(config.niks3_push_args.clone()),
        running: Mutex::default(),
        uploading: Mutex::default(),
        finished: Notify::new(),
        config,
    });
    resume(&worker).await;
    tokio::spawn(heartbeat(worker.clone()));

    loop {
        if worker.running_count() >= worker.max_jobs as usize {
            worker.finished.notified().await;
            continue;
        }
        match worker
            .post::<ClaimResponse>(api::CLAIM_PATH, &worker.status())
            .await
        {
            Ok(ClaimResponse::Job(job)) => start(&worker, job),
            Ok(ClaimResponse::Idle) => {}
            Err(error) => {
                tracing::warn!("claiming a build: {error:#}");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// Tells the server that the worker started, so that the builds of its
/// previous run are not left waiting for it, and resumes their uploads.
async fn resume(worker: &Arc<Worker>) {
    let response = loop {
        match worker
            .post::<StartResponse>(api::START_PATH, &worker.status())
            .await
        {
            Ok(response) => break response,
            Err(error) => {
                tracing::warn!("contacting the server: {error:#}");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };
    for Upload {
        lease,
        drv_path,
        outputs,
    } in response.uploads
    {
        tracing::info!(drv = drv_path, "resuming upload");
        worker.uploading.lock().expect("not poisoned").insert(lease);
        let worker = worker.clone();
        tokio::spawn(async move { upload(&worker, lease, &drv_path, outputs).await });
    }
}

async fn heartbeat(worker: Arc<Worker>) {
    loop {
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        match worker
            .post::<HeartbeatResponse>(api::HEARTBEAT_PATH, &worker.status())
            .await
        {
            Ok(response) => {
                let mut running = worker.running.lock().expect("not poisoned");
                for lease in response.abort {
                    if let Some(build) = running.remove(&lease) {
                        tracing::info!(%lease, "aborting build");
                        build.abort();
                    }
                }
                worker.finished.notify_one();
            }
            Err(error) => tracing::warn!("sending heartbeat: {error:#}"),
        }
    }
}

fn start(worker: &Arc<Worker>, job: Job) {
    tracing::info!(drv = job.drv_path, lease = %job.lease, "building");
    let build = tokio::spawn({
        let drv_path = job.drv_path.clone();
        async move { build(&drv_path).await }
    });
    worker
        .running
        .lock()
        .expect("not poisoned")
        .insert(job.lease, build.abort_handle());

    let worker = worker.clone();
    tokio::spawn(async move {
        // Aborted builds were released by the server already.
        let Ok(event) = build.await else {
            return;
        };
        let outputs = match &event {
            BuildEvent::Built { outputs, .. } => {
                tracing::info!(drv = job.drv_path, "built");
                Some(outputs.values().cloned().collect::<Vec<_>>())
            }
            BuildEvent::Failed { error, .. } => {
                tracing::warn!(drv = job.drv_path, "build failed: {error}");
                None
            }
            BuildEvent::Transient { cause, error } => {
                tracing::warn!(
                    drv = job.drv_path,
                    cause = cause.as_str(),
                    "could not build: {error}"
                );
                None
            }
            BuildEvent::Uploaded => unreachable!("builds end before uploads"),
        };
        // The server releases builds missing from heartbeats, so the lease
        // stays in them until the server has the result.
        let free_slot = || {
            worker
                .running
                .lock()
                .expect("not poisoned")
                .remove(&job.lease);
            worker.finished.notify_one();
        };
        if outputs.is_some() {
            // Uploading takes no slot.
            worker
                .uploading
                .lock()
                .expect("not poisoned")
                .insert(job.lease);
            free_slot();
        }

        let accepted = report(&worker, job.lease, event).await;
        match outputs {
            Some(paths) if accepted => upload(&worker, job.lease, &job.drv_path, paths).await,
            Some(_) => {
                worker
                    .uploading
                    .lock()
                    .expect("not poisoned")
                    .remove(&job.lease);
            }
            None => free_slot(),
        }
    });
}

/// Pushes the outputs of a build, which must be among the uploading ones,
/// and reports the result.
async fn upload(worker: &Worker, lease: Uuid, drv_path: &str, outputs: Vec<String>) {
    let event = match worker.uploader.upload(outputs).await {
        Ok(()) => BuildEvent::Uploaded,
        Err(error) => {
            tracing::warn!(drv = drv_path, "upload failed: {error}");
            BuildEvent::Transient {
                cause: Transient::UploadFailed,
                error: format!("uploading the outputs: {error}"),
            }
        }
    };
    report(worker, lease, event).await;
    worker
        .uploading
        .lock()
        .expect("not poisoned")
        .remove(&lease);
}

/// Reports a step of a build until the server takes it. Returns false if
/// the server no longer wants the build.
async fn report(worker: &Worker, lease: Uuid, event: BuildEvent) -> bool {
    let report = Report { lease, event };
    loop {
        let response = worker
            .http
            .post(format!(
                "{}{}",
                worker.config.server_url.trim_end_matches('/'),
                api::REPORT_PATH
            ))
            .bearer_auth(&worker.token)
            .json(&report)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => return true,
            Ok(response) if response.status() == reqwest::StatusCode::GONE => {
                tracing::warn!(%lease, "the server no longer wants this build");
                return false;
            }
            Ok(response) => tracing::warn!("reporting a build: {}", response.status()),
            Err(error) => tracing::warn!("reporting a build: {error}"),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn build(drv_path: &str) -> BuildEvent {
    let inputs = match check_inputs(drv_path).await {
        Ok(Some(inputs)) => inputs,
        // The server copied the derivation here. If it is gone, e.g. collected
        // since, building would only fail for reasons unrelated to the derivation.
        Ok(None) => {
            return BuildEvent::Transient {
                cause: Transient::MissingDerivation,
                error: format!("{drv_path} is not in the store"),
            };
        }
        Err(error) => {
            return BuildEvent::Transient {
                cause: Transient::WorkerError,
                error: format!("{error:#}"),
            };
        }
    };
    match nix_build(drv_path, inputs).await {
        Ok(Ok(outputs)) => BuildEvent::Built { outputs, inputs },
        Ok(Err((error, log_tail))) => BuildEvent::Failed {
            error,
            log_tail,
            inputs,
        },
        Err(error) => BuildEvent::Transient {
            cause: Transient::WorkerError,
            error: format!("{error:#}"),
        },
    }
}

/// Whether the derivation's inputs are all here or need substituting, or
/// `None` if the derivation is not in the store.
///
/// The server only hands out derivations whose outputs are in no cache, so
/// Nix need not look for them in the substituters, which costs a round trip
/// per substituter and dominates small builds. When every input is already
/// here, e.g. because this worker built it, the build runs without
/// substituters at all.
async fn check_inputs(drv_path: &str) -> anyhow::Result<Option<Inputs>> {
    let mut paths = vec![drv_path.to_owned()];
    let inputs = match tokio::fs::read_to_string(drv_path).await {
        Ok(text) => input_paths(&text).await?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {drv_path}")),
    };
    paths.extend(inputs.iter().flatten().cloned());
    let output = Command::new("nix-store")
        .args(["--check-validity", "--print-invalid"])
        .args(&paths)
        .stderr(Stdio::inherit())
        .output()
        .await
        .context("running nix-store")?;
    anyhow::ensure!(output.status.success(), "nix-store --check-validity failed");
    let invalid = String::from_utf8_lossy(&output.stdout);
    if invalid.lines().any(|path| path == drv_path) {
        return Ok(None);
    }
    Ok(Some(if inputs.is_none() || !invalid.trim().is_empty() {
        Inputs::Substituted
    } else {
        Inputs::Local
    }))
}

/// The store paths the derivation's build reads, or `None` if some are not
/// known in advance, e.g. content-addressed outputs.
async fn input_paths(text: &str) -> anyhow::Result<Option<Vec<String>>> {
    let derivation = drv::parse(text)?;
    let mut paths = derivation.input_srcs;
    for (input, outputs) in derivation.input_drvs {
        let text = tokio::fs::read_to_string(&input)
            .await
            .with_context(|| format!("reading {input}"))?;
        let input_outputs = drv::parse(&text)
            .with_context(|| format!("parsing {input}"))?
            .outputs;
        for name in outputs {
            match input_outputs.iter().find(|(output, _)| *output == name) {
                Some((_, path)) if !path.is_empty() => paths.push(path.clone()),
                _ => return Ok(None),
            }
        }
    }
    Ok(Some(paths))
}

/// Builds the derivation's outputs. The outer error means the build could
/// not be attempted; the inner one carries a build failure and its log.
async fn nix_build(
    drv_path: &str,
    inputs: Inputs,
) -> anyhow::Result<Result<BTreeMap<String, String>, (String, String)>> {
    let mut child = Command::new("nix")
        .args(["build", "--no-link", "--json", "--print-build-logs"])
        // Keep the build on this machine. Distributing builds is the server's job.
        .args(["--builders", ""])
        .args([
            "--option",
            "substitute",
            match inputs {
                Inputs::Local => "false",
                Inputs::Substituted => "true",
            },
        ])
        .arg(format!("{drv_path}^*"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting nix build")?;
    let stdout = tokio::spawn(read_tail(child.stdout.take().expect("piped"), 1 << 20));
    let log_tail = tokio::spawn(read_tail(
        child.stderr.take().expect("piped"),
        LOG_TAIL_BYTES,
    ));
    let status = child.wait().await.context("waiting for nix build")?;
    let stdout = stdout.await?;
    let log_tail = log_tail.await?;
    if !status.success() {
        return Ok(Err((format!("nix build failed with {status}"), log_tail)));
    }

    #[derive(Deserialize)]
    struct Built {
        outputs: BTreeMap<String, String>,
    }
    let built: Vec<Built> = serde_json::from_str(&stdout)
        .with_context(|| format!("unexpected nix build output: {stdout}"))?;
    let [built] = <[Built; 1]>::try_from(built)
        .map_err(|built| anyhow::anyhow!("nix build reported {} results", built.len()))?;
    if built.outputs.is_empty() {
        bail!("nix build reported no outputs");
    }
    Ok(Ok(built.outputs))
}
