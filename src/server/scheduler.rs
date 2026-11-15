//! Hands ready builds to workers with free slots.
//!
//! A worker with a free slot always gets a ready build, so the fleet never
//! builds less than its slots allow. Load only decides who goes first: a
//! worker whose CPUs are busy lets a waiting worker with idle CPUs take the
//! next build, if that worker can take one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;

use super::builds;
use super::config::WorkerConfig;
use super::{Nix, Server};
use crate::api::{BuildEvent, ClaimResponse, Job, Transient, WorkerStatus};

/// How long a claim waits for a build to become ready.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a busy worker leaves a ready build to idle workers.
pub(super) const YIELD: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct Scheduler {
    /// Signals that builds may have become ready.
    pub ready: Notify,
    /// When builds were last handed to each worker.
    starts: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// Workers waiting in a claim.
    waiting: Mutex<HashMap<String, Waiter>>,
    /// Numbers the claims.
    claims: AtomicU64,
    staging: Mutex<HashMap<String, Staging>>,
}

/// A worker waiting in a claim, and what it can build.
#[derive(Clone)]
struct Waiter {
    claim: u64,
    /// Updated by the worker's heartbeats while it waits.
    busy: bool,
    systems: Vec<String>,
    features: Vec<String>,
    /// The runs whose builds it can take, unless it can take any.
    runs: Option<Vec<i64>>,
}

/// Runs whose derivations were copied to a remote worker.
#[derive(Default)]
struct Staging {
    done: HashSet<i64>,
    running: HashSet<i64>,
}

impl Scheduler {
    /// Forgets which runs' derivations are on the worker, so that they are
    /// copied again before it gets another build, e.g. after it reported a
    /// derivation missing because its store collected it.
    pub fn forget_staged(&self, worker: &str) {
        if let Some(staging) = self.staging.lock().expect("not poisoned").get_mut(worker) {
            staging.done.clear();
        }
    }

    /// Builds started within the window are not yet reflected in the idle
    /// time the worker measured, so each of them is assumed to take a core.
    fn recent_starts(&self, worker: &str, window: Duration) -> usize {
        let mut starts = self.starts.lock().expect("not poisoned");
        let starts = starts.entry(worker.to_owned()).or_default();
        while starts.front().is_some_and(|start| start.elapsed() > window) {
            starts.pop_front();
        }
        starts.len()
    }

    fn is_busy(&self, worker: &str, status: &WorkerStatus, min_free_memory: u64) -> bool {
        let window = Duration::from_secs(status.load.window_secs);
        is_busy(status, self.recent_starts(worker, window), min_free_memory)
    }

    /// Takes the load of a heartbeat, which is newer than the one the
    /// worker's waiting claim started with.
    pub fn update_load(&self, worker: &str, status: &WorkerStatus, min_free_memory: u64) {
        let busy = self.is_busy(worker, status, min_free_memory);
        if let Some(waiter) = self.waiting.lock().expect("not poisoned").get_mut(worker) {
            waiter.busy = busy;
        }
    }

    /// Ends the claim the worker waits in, e.g. one the server still holds
    /// from before the worker restarted.
    pub fn end_claim(&self, worker: &str) {
        self.waiting.lock().expect("not poisoned").remove(worker);
    }

    fn record_start(&self, worker: &str) {
        let mut starts = self.starts.lock().expect("not poisoned");
        starts
            .entry(worker.to_owned())
            .or_default()
            .push_back(Instant::now());
    }

    /// Whether another worker with idle CPUs waits for a build it can take.
    async fn idle_worker_can_claim(&self, db: &PgPool, except: &str) -> sqlx::Result<bool> {
        let idle: Vec<(String, Waiter)> = self
            .waiting
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|(worker, waiter)| *worker != except && !waiter.busy)
            .map(|(worker, waiter)| (worker.clone(), waiter.clone()))
            .collect();
        for (worker, waiter) in idle {
            let runs = waiter.runs.as_deref();
            if builds::can_claim(db, &worker, &waiter.systems, &waiter.features, runs).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Registers a claim among the waiting workers until it ends. A worker
/// waits in one claim at a time, so a newer claim of the worker replaces the
/// older one, whose client is gone.
struct Waiting<'a> {
    scheduler: &'a Scheduler,
    worker: &'a str,
    claim: u64,
}

impl<'a> Waiting<'a> {
    fn new(scheduler: &'a Scheduler, worker: &'a str, waiter: Waiter) -> Self {
        let claim = waiter.claim;
        scheduler
            .waiting
            .lock()
            .expect("not poisoned")
            .insert(worker.to_owned(), waiter);
        Self {
            scheduler,
            worker,
            claim,
        }
    }

    /// Sets the runs the worker can take builds of, and returns whether it
    /// is busy, or `None` once the claim was ended or replaced.
    fn set_runs(&self, runs: Option<Vec<i64>>) -> Option<bool> {
        let mut waiting = self.scheduler.waiting.lock().expect("not poisoned");
        let waiter = waiting
            .get_mut(self.worker)
            .filter(|waiter| waiter.claim == self.claim)?;
        waiter.runs = runs;
        Some(waiter.busy)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut waiting = self.scheduler.waiting.lock().expect("not poisoned");
        if waiting
            .get(self.worker)
            .is_some_and(|waiter| waiter.claim == self.claim)
        {
            waiting.remove(self.worker);
        }
    }
}

/// Whether the worker's CPUs or memory are taken, so that an idle worker
/// should get the next build if one is waiting.
pub fn is_busy(status: &WorkerStatus, recent_starts: usize, min_free_memory: u64) -> bool {
    !status.running.is_empty()
        && (status.load.idle_cores - (recent_starts as f64) < 1.0
            || status.load.mem_available_bytes < min_free_memory)
}

pub async fn claim(
    server: &Arc<Server>,
    worker: &WorkerConfig,
    status: &WorkerStatus,
) -> anyhow::Result<ClaimResponse> {
    let scheduler = &server.scheduler;
    if status.running.len() >= status.max_jobs as usize {
        return Ok(ClaimResponse::Idle);
    }
    let waiting = Waiting::new(
        scheduler,
        &worker.name,
        Waiter {
            claim: scheduler.claims.fetch_add(1, Ordering::Relaxed),
            busy: scheduler.is_busy(
                &worker.name,
                status,
                server.config.scheduler.min_free_memory_bytes(),
            ),
            systems: status.systems.clone(),
            features: status.features.clone(),
            // Until it looks, a remote worker has nothing staged.
            runs: worker.store_uri.as_ref().map(|_| Vec::new()),
        },
    );
    let deadline = tokio::time::Instant::now() + CLAIM_TIMEOUT;
    loop {
        // Register for wakeups before looking, so none is missed in between.
        let ready = scheduler.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();

        let runs = match &worker.store_uri {
            Some(_) => Some(staged_runs(server, worker).await?),
            None => None,
        };
        let Some(busy) = waiting.set_runs(runs.clone()) else {
            return Ok(ClaimResponse::Idle);
        };
        if busy
            && scheduler
                .idle_worker_can_claim(&server.db, &worker.name)
                .await?
        {
            tokio::time::sleep(YIELD).await;
        }
        let started = Instant::now();
        let claimed = builds::claim(
            &server.db,
            &worker.name,
            &status.systems,
            &status.features,
            runs.as_deref(),
        )
        .await?;
        if let Some(claimed) = claimed {
            scheduler.record_start(&worker.name);
            server
                .metrics
                .claimed(&claimed.timing, started.elapsed().as_secs_f64());
            tracing::debug!(
                worker = worker.name,
                drv = claimed.drv_path,
                "dispatched build"
            );
            return Ok(ClaimResponse::Job(Job {
                lease: claimed.lease,
                drv_path: claimed.drv_path,
            }));
        }

        tokio::select! {
            () = &mut ready => {}
            () = tokio::time::sleep_until(deadline) => return Ok(ClaimResponse::Idle),
        }
    }
}

/// The building runs whose derivations are on the remote worker. Starts
/// copying the derivations of the others.
async fn staged_runs(server: &Arc<Server>, worker: &WorkerConfig) -> anyhow::Result<Vec<i64>> {
    let building: Vec<i64> = sqlx::query_scalar("SELECT id FROM runs WHERE state = 'building'")
        .fetch_all(&server.db)
        .await?;
    let mut staging = server.scheduler.staging.lock().expect("not poisoned");
    let staging = staging.entry(worker.name.clone()).or_default();
    staging.done.retain(|run| building.contains(run));
    for &run in &building {
        if !staging.done.contains(&run) && staging.running.insert(run) {
            tokio::spawn(stage(server.clone(), worker.clone(), run));
        }
    }
    Ok(staging.done.iter().copied().collect())
}

async fn stage(server: Arc<Server>, worker: WorkerConfig, run: i64) {
    let started = Instant::now();
    let result = copy_derivations(&server, &worker, run).await;
    let mut staging = server.scheduler.staging.lock().expect("not poisoned");
    let staging = staging.entry(worker.name.clone()).or_default();
    staging.running.remove(&run);
    match result {
        Ok(()) => {
            let seconds = started.elapsed().as_secs_f64();
            tracing::info!(worker = worker.name, run, seconds, "copied derivations");
            server.metrics.staged(&worker.name, seconds);
            staging.done.insert(run);
            server.scheduler.ready.notify_waiters();
        }
        Err(error) => {
            tracing::warn!(worker = worker.name, run, "copying derivations: {error:#}");
        }
    }
}

/// Copies the run's derivations, with everything needed to build them, to
/// the worker's store in one batch.
async fn copy_derivations(server: &Server, worker: &WorkerConfig, run: i64) -> anyhow::Result<()> {
    let store_uri = worker.store_uri.as_deref().expect("remote worker");
    let drv_paths: Vec<String> =
        sqlx::query_scalar("SELECT drv_path FROM run_builds WHERE run_id = $1")
            .bind(run)
            .fetch_all(&server.db)
            .await?;
    if drv_paths.is_empty() {
        return Ok(());
    }
    match &server.nix {
        Nix::Commands => nix_copy(store_uri, &drv_paths).await,
        #[cfg(test)]
        Nix::Fake(nix) => nix.copy(&worker.name, &drv_paths).await,
    }
}

async fn nix_copy(store_uri: &str, drv_paths: &[String]) -> anyhow::Result<()> {
    let mut child = Command::new("nix")
        .args([
            "copy",
            "--derivation",
            "--no-check-sigs",
            "--stdin",
            "--to",
            store_uri,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting nix copy")?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(drv_paths.join("\n").as_bytes()).await?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .context("waiting for nix copy")?;
    if !output.status.success() {
        bail!(
            "nix copy failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Requeues the builds of workers that stopped sending heartbeats.
pub async fn expire_orphans(server: &Server) -> anyhow::Result<()> {
    let timeout = server.config.scheduler.worker_timeout().as_secs() as i64;
    for lease in builds::orphaned_leases(&server.db, timeout).await? {
        tracing::warn!(%lease, "worker of build stopped responding");
        server
            .report(
                lease,
                BuildEvent::Transient {
                    cause: Transient::WorkerLost,
                    error: "the worker stopped responding".into(),
                },
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Load;

    fn status(running: usize, idle_cores: f64, mem_available_gib: u64) -> WorkerStatus {
        WorkerStatus {
            systems: vec!["x86_64-linux".into()],
            features: vec![],
            max_jobs: 4,
            running: (0..running).map(|_| uuid::Uuid::new_v4()).collect(),
            uploading: vec![],
            load: Load {
                cpus: 8,
                idle_cores,
                window_secs: 5,
                mem_available_bytes: mem_available_gib << 30,
            },
        }
    }

    const MIN_FREE: u64 = 2 << 30;

    #[test]
    fn worker_without_builds_is_never_busy() {
        assert!(!is_busy(&status(0, 0.0, 0), 3, MIN_FREE));
    }

    #[test]
    fn busy_after_counting_recent_starts_as_cores() {
        assert!(!is_busy(&status(1, 2.5, 8), 1, MIN_FREE));
        assert!(is_busy(&status(1, 2.5, 8), 2, MIN_FREE));
        assert!(is_busy(&status(1, 0.5, 8), 0, MIN_FREE));
    }

    #[test]
    fn busy_when_memory_is_short() {
        assert!(is_busy(&status(1, 8.0, 1), 0, MIN_FREE));
    }
}
