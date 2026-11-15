//! The coordinator: polls GitHub, evaluates, schedules builds on workers and
//! reports results.

mod auth;
mod builds;
pub mod config;
mod evaluator;
mod finalizer;
mod github;
mod http;
mod issues;
mod metrics;
mod model;
mod pins;
mod reconciler;
mod reporter;
mod runs;
mod scheduler;
mod summary;
#[cfg(test)]
mod tests;
mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use uuid::Uuid;

use self::auth::Auth;
use self::config::{Config, WorkerConfig, read_secret};
use self::github::Github;
use self::metrics::Metrics;
use self::pins::Niks3;
use self::scheduler::Scheduler;
use crate::api::{BuildEvent, Transient};

pub struct Server {
    config: Config,
    db: PgPool,
    github: Github,
    niks3: Option<Niks3>,
    auth: Option<Auth>,
    /// Workers by the SHA-256 of their token.
    workers: Vec<([u8; 32], WorkerConfig)>,
    scheduler: Scheduler,
    metrics: Metrics,
    wake: Wake,
    nix: Nix,
}

/// How the server runs Nix.
enum Nix {
    /// `nix-eval-jobs` and `nix` from `PATH`.
    Commands,
    #[cfg(test)]
    Fake(Arc<tests::FakeNix>),
}

/// Wakes background tasks early when there is work for them.
#[derive(Default)]
struct Wake {
    reconciler: Notify,
    evaluator: Notify,
    reporter: Notify,
    finalizer: Notify,
}

impl Server {
    fn worker_by_token(&self, token: &str) -> Option<&WorkerConfig> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.workers
            .iter()
            .find(|(known, _)| *known == digest)
            .map(|(_, worker)| worker)
    }

    /// Records a step of a build and wakes whatever it affects. Returns
    /// whether the build was in the state the step follows.
    async fn report(&self, lease: Uuid, event: BuildEvent) -> anyhow::Result<bool> {
        let reported =
            builds::report(&self.db, lease, &event, self.config.scheduler.max_attempts).await?;
        let Some(build) = &reported.build else {
            // A worker repeats a report whose response it lost. It uploads the
            // outputs only once `built` is accepted, so a repeated one is too.
            let repeated = matches!(event, BuildEvent::Built { .. })
                && builds::is_uploading(&self.db, lease).await?;
            return Ok(repeated);
        };
        let worker = build.timing.worker.as_deref().unwrap_or_default();
        match &event {
            BuildEvent::Built { inputs, .. } => self.metrics.built(&build.timing, *inputs),
            BuildEvent::Uploaded => {
                self.metrics.uploaded(&build.timing);
                self.metrics.attempt(worker, "succeeded");
            }
            BuildEvent::Failed { error, inputs, .. } => {
                self.metrics.built(&build.timing, *inputs);
                self.metrics.attempt(worker, "failed");
                tracing::info!(drv = build.drv_path, worker, "build failed: {error}");
            }
            BuildEvent::Transient { cause, error } => {
                self.metrics.attempt(worker, cause.as_str());
                if matches!(cause, Transient::MissingDerivation) {
                    self.scheduler.forget_staged(worker);
                }
                if build.state == "failed" {
                    tracing::error!(
                        drv = build.drv_path,
                        worker,
                        cause = cause.as_str(),
                        attempts = build.attempts,
                        "giving up on build: {error}"
                    );
                } else {
                    tracing::warn!(
                        drv = build.drv_path,
                        worker,
                        cause = cause.as_str(),
                        attempts = build.attempts,
                        "retrying build: {error}"
                    );
                }
            }
        }
        if reported.unblocked {
            self.scheduler.ready.notify_waiters();
        }
        self.wake.reporter.notify_one();
        if !reported.finished_runs.is_empty() {
            self.wake.finalizer.notify_one();
        }
        Ok(true)
    }
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let db = PgPool::connect(&config.database_url)
        .await
        .context("connecting to the database")?;
    sqlx::migrate!()
        .run(&db)
        .await
        .context("migrating the database")?;
    let server = Server::new(config, db, Nix::Commands)?;
    let _tasks = server.start().await?;

    if let Some(address) = server.config.metrics_listen {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("listening on {address}"))?;
        tokio::spawn(axum::serve(listener, http::metrics_router(server.clone())).into_future());
    }
    let listener = tokio::net::TcpListener::bind(server.config.listen)
        .await
        .with_context(|| format!("listening on {}", server.config.listen))?;
    tracing::info!(address = %server.config.listen, "listening");
    axum::serve(listener, http::router(server))
        .await
        .context("serving HTTP")
}

impl Server {
    fn new(config: Config, db: PgPool, nix: Nix) -> anyhow::Result<Arc<Self>> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        let workers = config
            .workers
            .iter()
            .map(|worker| {
                let token = read_secret(&worker.token_file)?;
                Ok((Sha256::digest(token.as_bytes()).into(), worker.clone()))
            })
            .collect::<anyhow::Result<_>>()?;
        let metrics = Metrics::default();
        Ok(Arc::new(Self {
            github: Github::new(&config.github, metrics.github.clone())?,
            niks3: config
                .niks3
                .as_ref()
                .map(|niks3| Niks3::new(niks3, http_client.clone()))
                .transpose()?,
            auth: config
                .web
                .as_ref()
                .map(|web| {
                    Auth::new(
                        web,
                        &config.github,
                        &config.public_url,
                        http_client.clone(),
                        metrics.github.clone(),
                    )
                })
                .transpose()?,
            workers,
            scheduler: Scheduler::default(),
            metrics,
            wake: Wake::default(),
            nix,
            db,
            config,
        }))
    }

    /// Recovers from the previous run of the server, then starts the
    /// background tasks, which stop when the returned set is dropped.
    async fn start(self: &Arc<Self>) -> anyhow::Result<JoinSet<()>> {
        let requeued = runs::requeue_interrupted(&self.db).await?;
        if requeued > 0 {
            tracing::info!(requeued, "requeued evaluations interrupted by a restart");
        }
        if let Err(error) = finalizer::remove_stale_gc_roots(self).await {
            tracing::warn!("removing stale GC roots: {error:#}");
        }

        let mut tasks = JoinSet::new();
        tasks.spawn(reconciler::run(self.clone()));
        tasks.spawn(evaluator::run(self.clone()));
        tasks.spawn(reporter::run(self.clone()));
        tasks.spawn(finalizer::run(self.clone()));
        tasks.spawn(expire_orphans(self.clone()));
        Ok(tasks)
    }
}

async fn expire_orphans(server: Arc<Server>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if let Err(error) = scheduler::expire_orphans(&server).await {
            tracing::warn!("expiring builds of lost workers: {error:#}");
        }
    }
}
