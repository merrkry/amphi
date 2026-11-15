use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::builds;
use super::config::WorkerConfig;
use super::metrics::Release;
use super::{Server, runs, scheduler, web};
use crate::api::{self, ClaimResponse, HeartbeatResponse, Report, StartResponse, WorkerStatus};

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route(api::START_PATH, post(start))
        .route(api::CLAIM_PATH, post(claim))
        .route(api::HEARTBEAT_PATH, post(heartbeat))
        .route(api::REPORT_PATH, post(report))
        .route("/api/github/webhook", post(webhook))
        .merge(web::router(&server))
        .with_state(server)
}

pub fn metrics_router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(server)
}

pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

/// The worker that authenticated the request with its bearer token.
struct Worker(WorkerConfig);

impl FromRequestParts<Arc<Server>> for Worker {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        server: &Arc<Server>,
    ) -> Result<Self, StatusCode> {
        parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| server.worker_by_token(token))
            .map(|worker| Self(worker.clone()))
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

async fn record_status(server: &Server, worker: &str, status: &WorkerStatus) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO workers (name, last_seen, status) VALUES ($1, now(), $2) \
         ON CONFLICT (name) DO UPDATE SET last_seen = now(), status = $2",
    )
    .bind(worker)
    .bind(sqlx::types::Json(status))
    .execute(&server.db)
    .await?;
    Ok(())
}

/// Recovers the builds of a worker that started, e.g. after a deployment or
/// a crash.
async fn start(
    State(server): State<Arc<Server>>,
    Worker(worker): Worker,
    Json(status): Json<WorkerStatus>,
) -> Result<Json<StartResponse>, AppError> {
    record_status(&server, &worker.name, &status).await?;
    // Before the builds go back in the queue, so that a claim of the
    // previous process does not take them.
    server.scheduler.end_claim(&worker.name);
    let released = builds::release_running(&server.db, &worker.name).await?;
    if released > 0 {
        server
            .metrics
            .released(&worker.name, Release::WorkerRestarted, released);
        server.scheduler.ready.notify_waiters();
    }
    let uploads = builds::uploads(&server.db, &worker.name).await?;
    tracing::info!(
        worker = worker.name,
        released,
        uploads = uploads.len(),
        "worker started"
    );
    Ok(Json(StartResponse { uploads }))
}

async fn claim(
    State(server): State<Arc<Server>>,
    Worker(worker): Worker,
    Json(status): Json<WorkerStatus>,
) -> Result<Json<ClaimResponse>, AppError> {
    record_status(&server, &worker.name, &status).await?;
    Ok(Json(scheduler::claim(&server, &worker, &status).await?))
}

async fn heartbeat(
    State(server): State<Arc<Server>>,
    Worker(worker): Worker,
    Json(status): Json<WorkerStatus>,
) -> Result<Json<HeartbeatResponse>, AppError> {
    record_status(&server, &worker.name, &status).await?;
    let min_free_memory = server.config.scheduler.min_free_memory_bytes();
    server
        .scheduler
        .update_load(&worker.name, &status, min_free_memory);
    let known: Vec<_> = status
        .running
        .iter()
        .chain(&status.uploading)
        .copied()
        .collect();
    // The worker never received these builds, e.g. because the server
    // restarted before answering its claim.
    let forgotten = builds::forgotten_leases(&server.db, &worker.name, &known).await?;
    if !forgotten.is_empty() {
        tracing::warn!(
            worker = worker.name,
            builds = forgotten.len(),
            "worker does not have its builds"
        );
        let released = builds::release(&server.db, &forgotten).await?;
        server
            .metrics
            .released(&worker.name, Release::NotReceived, released);
        server.scheduler.ready.notify_waiters();
    }

    // Builds the server no longer assigns to the worker, e.g. after it expired them.
    let held = builds::held_leases(&server.db, &worker.name, &status.running).await?;
    let mut abort: Vec<_> = status
        .running
        .iter()
        .filter(|lease| !held.contains(lease))
        .copied()
        .collect();
    let unneeded = builds::unneeded_leases(&server.db, &worker.name).await?;
    let released = builds::release(&server.db, &unneeded).await?;
    server
        .metrics
        .released(&worker.name, Release::Unneeded, released);
    // A run that started needing one of them meanwhile can build it now.
    if released > 0 {
        server.scheduler.ready.notify_waiters();
    }
    abort.extend(unneeded);
    if !abort.is_empty() {
        tracing::info!(
            worker = worker.name,
            aborted = abort.len(),
            "aborting builds"
        );
    }
    Ok(Json(HeartbeatResponse { abort }))
}

async fn report(
    State(server): State<Arc<Server>>,
    Worker(worker): Worker,
    Json(report): Json<Report>,
) -> Result<StatusCode, AppError> {
    // Workers are trusted alike, so the lease alone names the build.
    if server.report(report.lease, report.event).await? {
        tracing::debug!(worker = worker.name, lease = %report.lease, "build reported");
        Ok(StatusCode::NO_CONTENT)
    } else {
        // The lease expired, and the build may run elsewhere by now.
        Ok(StatusCode::GONE)
    }
}

#[derive(Deserialize)]
struct WebhookPayload {
    action: Option<String>,
    check_run: Option<CheckRunPayload>,
    check_suite: Option<CheckSuitePayload>,
    repository: Option<RepositoryPayload>,
}

#[derive(Deserialize)]
struct RepositoryPayload {
    full_name: String,
}

#[derive(Deserialize)]
struct CheckRunPayload {
    id: i64,
}

#[derive(Deserialize)]
struct CheckSuitePayload {
    head_sha: String,
}

async fn webhook(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let signature = header("x-hub-signature-256").unwrap_or_default();
    if !server.github.verify_webhook(signature, &body) {
        return Ok(StatusCode::UNAUTHORIZED);
    }
    let event = header("x-github-event").unwrap_or_default();
    let payload: WebhookPayload = serde_json::from_slice(&body)?;
    let rerequested = payload.action.as_deref() == Some("rerequested");
    match event {
        "push" | "pull_request" | "create" | "delete" => server.wake.reconciler.notify_one(),
        "check_run" if rerequested => {
            let id = payload.check_run.map(|run| run.id);
            let run: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM runs WHERE check_eval_id = $1 OR check_build_id = $1",
            )
            .bind(id)
            .fetch_optional(&server.db)
            .await?;
            if let Some(run) = run {
                rerun(&server, run).await?;
            }
        }
        "check_suite" if rerequested => {
            // Forks and mirrors share commits.
            let repo = payload.repository.map(|repo| repo.full_name);
            let sha = payload.check_suite.map(|suite| suite.head_sha);
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM runs WHERE repo = $1 AND sha = $2")
                    .bind(repo)
                    .bind(sha)
                    .fetch_all(&server.db)
                    .await?;
            for run in ids {
                rerun(&server, run).await?;
            }
        }
        _ => {}
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rerun(server: &Server, run_id: i64) -> sqlx::Result<bool> {
    let rerun = runs::rerun(&server.db, run_id).await?;
    if rerun {
        tracing::info!(run = run_id, "rerunning run");
        server.wake.evaluator.notify_one();
        server.wake.reporter.notify_one();
    }
    Ok(rerun)
}

async fn metrics(State(server): State<Arc<Server>>) -> Result<String, AppError> {
    Ok(server.metrics.render(&server.db).await?)
}
