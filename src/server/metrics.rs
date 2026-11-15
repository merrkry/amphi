//! Prometheus metrics that show where builds spend their time, so that a
//! slow run can be told apart from a slow scheduler, slow uploads or slow
//! derivation copies, and how often builds, evaluations and GitHub fail.

use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use sqlx::PgPool;

use super::builds::Timing;
use super::model::Run;
use crate::api::Inputs;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct WorkerLabel {
    pub worker: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StateLabel {
    state: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabel {
    worker: String,
    inputs: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct AttemptLabel {
    worker: String,
    result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ReleaseLabel {
    worker: String,
    reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EvaluationLabel {
    repo: String,
    jobset: String,
    result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct GithubLabel {
    operation: &'static str,
    result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OperationLabel {
    operation: &'static str,
}

type Histograms<L = WorkerLabel> = Family<L, Histogram, fn() -> Histogram>;

fn seconds() -> Histogram {
    // 50 ms to about 3.6 hours.
    Histogram::new(exponential_buckets(0.05, 2.0, 19))
}

fn request_seconds() -> Histogram {
    // 50 ms to about 100 seconds.
    Histogram::new(exponential_buckets(0.05, 2.0, 12))
}

/// Why builds went back in the queue without counting an attempt.
#[derive(Clone, Copy)]
pub enum Release {
    /// The worker started again, so its running builds are gone.
    WorkerRestarted,
    /// The worker never received the build, e.g. because the server
    /// restarted before answering its claim.
    NotReceived,
    /// No active run needs the build anymore.
    Unneeded,
}

impl Release {
    fn as_str(self) -> &'static str {
        match self {
            Release::WorkerRestarted => "worker_restarted",
            Release::NotReceived => "not_received",
            Release::Unneeded => "unneeded",
        }
    }
}

/// Requests to the GitHub API, shared with the GitHub clients.
#[derive(Clone)]
pub struct GithubMetrics {
    requests: Family<GithubLabel, Counter>,
    seconds: Histograms<OperationLabel>,
}

impl GithubMetrics {
    /// Records a request, whose result is `ok`, the HTTP status GitHub
    /// answered with, or `transport` if no answer arrived.
    pub fn request(&self, operation: &'static str, result: String, duration: Duration) {
        self.requests
            .get_or_create(&GithubLabel { operation, result })
            .inc();
        self.seconds
            .get_or_create(&OperationLabel { operation })
            .observe(duration.as_secs_f64());
    }
}

pub struct Metrics {
    registry: Registry,
    build_wait: Histograms,
    build: Histograms<BuildLabel>,
    upload: Histograms,
    stage: Histograms,
    claim_query: Histogram,
    evaluation: Histograms<EvaluationLabel>,
    attempts: Family<AttemptLabel, Counter>,
    released: Family<ReleaseLabel, Counter>,
    pub github: GithubMetrics,
    builds: Family<StateLabel, Gauge>,
    ready_builds: Gauge,
    slots: Family<WorkerLabel, Gauge>,
    running: Family<WorkerLabel, Gauge>,
    uploading: Family<WorkerLabel, Gauge>,
}

impl Default for Metrics {
    fn default() -> Self {
        let mut registry = Registry::with_prefix("amphi");
        let metrics = Self {
            build_wait: Histograms::new_with_constructor(seconds),
            build: Histograms::new_with_constructor(seconds),
            upload: Histograms::new_with_constructor(seconds),
            stage: Histograms::new_with_constructor(seconds),
            claim_query: seconds(),
            evaluation: Histograms::new_with_constructor(seconds),
            attempts: Family::default(),
            released: Family::default(),
            github: GithubMetrics {
                requests: Family::default(),
                seconds: Histograms::new_with_constructor(request_seconds),
            },
            builds: Family::default(),
            ready_builds: Gauge::default(),
            slots: Family::default(),
            running: Family::default(),
            uploading: Family::default(),
            registry: Registry::default(),
        };
        registry.register(
            "build_wait_seconds",
            "From a build becoming ready on a worker to the worker starting it",
            metrics.build_wait.clone(),
        );
        registry.register(
            "build_seconds",
            "Running nix build, by worker and whether the inputs had to be substituted",
            metrics.build.clone(),
        );
        registry.register(
            "build_attempts",
            "Attempts at builds that ended, by worker and result: succeeded, failed, or the \
             cause of a transient failure, which counts against max-attempts",
            metrics.attempts.clone(),
        );
        registry.register(
            "builds_released",
            "Builds put back in the queue without counting an attempt, by worker and reason",
            metrics.released.clone(),
        );
        registry.register(
            "upload_seconds",
            "From a build finishing to its outputs being in the cache",
            metrics.upload.clone(),
        );
        registry.register(
            "stage_seconds",
            "Copying the derivations of a run to a remote worker",
            metrics.stage.clone(),
        );
        registry.register(
            "claim_query_seconds",
            "Finding the next build for a worker in the database",
            metrics.claim_query.clone(),
        );
        registry.register(
            "evaluation_seconds",
            "Evaluating a jobset, by repository, jobset and result",
            metrics.evaluation.clone(),
        );
        registry.register(
            "github_requests",
            "Requests to the GitHub API, by operation and result: ok, the HTTP status of an \
             error, or transport if GitHub did not answer",
            metrics.github.requests.clone(),
        );
        registry.register(
            "github_request_seconds",
            "Requests to the GitHub API, by operation",
            metrics.github.seconds.clone(),
        );
        registry.register(
            "builds",
            "Builds needed by active runs, by state",
            metrics.builds.clone(),
        );
        registry.register(
            "ready_builds",
            "Queued builds whose dependencies are all in the cache",
            metrics.ready_builds.clone(),
        );
        registry.register(
            "worker_slots",
            "Builds a worker runs at once at most",
            metrics.slots.clone(),
        );
        registry.register(
            "worker_running_builds",
            "Builds a worker is running",
            metrics.running.clone(),
        );
        registry.register(
            "worker_uploading_builds",
            "Builds whose outputs a worker is uploading",
            metrics.uploading.clone(),
        );
        Self {
            registry,
            ..metrics
        }
    }
}

impl Metrics {
    pub fn claimed(&self, timing: &Timing, query_seconds: f64) {
        self.claim_query.observe(query_seconds);
        if let (Some(worker), Some(ready), Some(started)) =
            (&timing.worker, timing.ready_at, timing.started_at)
        {
            let label = WorkerLabel {
                worker: worker.clone(),
            };
            observe(&self.build_wait, &label, ready, started);
        }
    }

    /// Records a build that finished running, successfully or not.
    pub fn built(&self, timing: &Timing, inputs: Inputs) {
        if let (Some(worker), Some(started), Some(built)) =
            (&timing.worker, timing.started_at, timing.built_at)
        {
            let label = BuildLabel {
                worker: worker.clone(),
                inputs: inputs.as_str(),
            };
            observe(&self.build, &label, started, built);
        }
    }

    pub fn uploaded(&self, timing: &Timing) {
        if let (Some(worker), Some(built), Some(finished)) =
            (&timing.worker, timing.built_at, timing.finished_at)
        {
            let label = WorkerLabel {
                worker: worker.clone(),
            };
            observe(&self.upload, &label, built, finished);
        }
    }

    /// Records an attempt at a build that ended with `result`: `succeeded`,
    /// `failed`, or the cause of a transient failure.
    pub fn attempt(&self, worker: &str, result: &'static str) {
        self.attempts
            .get_or_create(&AttemptLabel {
                worker: worker.to_owned(),
                result,
            })
            .inc();
    }

    pub fn released(&self, worker: &str, reason: Release, builds: u64) {
        self.released
            .get_or_create(&ReleaseLabel {
                worker: worker.to_owned(),
                reason: reason.as_str(),
            })
            .inc_by(builds);
    }

    pub fn staged(&self, worker: &str, seconds: f64) {
        self.stage
            .get_or_create(&WorkerLabel {
                worker: worker.to_owned(),
            })
            .observe(seconds);
    }

    /// Records an evaluation, whose result is `succeeded`, `failed`, or
    /// `superseded` if the run was superseded meanwhile.
    pub fn evaluated(&self, run: &Run, result: &'static str, duration: Duration) {
        self.evaluation
            .get_or_create(&EvaluationLabel {
                repo: run.repo.clone(),
                jobset: run.jobset.clone(),
                result,
            })
            .observe(duration.as_secs_f64());
    }

    /// Refreshes the gauges from the database and renders every metric.
    pub async fn render(&self, db: &PgPool) -> anyhow::Result<String> {
        let states: Vec<(String, i64)> = sqlx::query_as(
            "SELECT b.state::text, count(*) FROM builds b WHERE EXISTS ( \
               SELECT 1 FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
               WHERE rb.drv_path = b.drv_path AND r.state = 'building' \
             ) GROUP BY b.state",
        )
        .fetch_all(db)
        .await?;
        self.builds.clear();
        for (state, count) in states {
            self.builds.get_or_create(&StateLabel { state }).set(count);
        }
        let ready: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM builds b WHERE b.state = 'queued' AND NOT EXISTS ( \
               SELECT 1 FROM build_deps d JOIN builds dep ON dep.drv_path = d.dep \
               WHERE d.drv_path = b.drv_path AND dep.state <> 'succeeded' \
             ) AND EXISTS ( \
               SELECT 1 FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
               WHERE rb.drv_path = b.drv_path AND r.state = 'building' \
             )",
        )
        .fetch_one(db)
        .await?;
        self.ready_builds.set(ready);

        let workers: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            "SELECT name, (status->>'max_jobs')::bigint, \
             jsonb_array_length(status->'running')::bigint, \
             jsonb_array_length(status->'uploading')::bigint \
             FROM workers WHERE last_seen > now() - interval '5 minutes'",
        )
        .fetch_all(db)
        .await?;
        self.slots.clear();
        self.running.clear();
        self.uploading.clear();
        for (worker, slots, running, uploading) in workers {
            let label = WorkerLabel { worker };
            self.slots.get_or_create(&label).set(slots);
            self.running.get_or_create(&label).set(running);
            self.uploading.get_or_create(&label).set(uploading);
        }

        let mut text = String::new();
        encode(&mut text, &self.registry)?;
        Ok(text)
    }
}

fn observe<L: Clone + std::hash::Hash + Eq>(
    histograms: &Histograms<L>,
    label: &L,
    from: jiff_sqlx::Timestamp,
    to: jiff_sqlx::Timestamp,
) {
    let seconds = (to.to_jiff() - from.to_jiff()).total(jiff::Unit::Second);
    if let Ok(seconds) = seconds {
        histograms.get_or_create(label).observe(seconds.max(0.0));
    }
}
