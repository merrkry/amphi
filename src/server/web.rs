//! The JSON API behind the web UI, for browsers and agents alike, and the
//! frontend's static files.
//!
//! Runs are addressed by repository, commit and jobset, like GitHub
//! addresses commits. A commit may be given by a unique prefix of its hash.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::services::{ServeDir, ServeFile};

use super::Server;
use super::auth::{self, User};
use super::http::{AppError, rerun};
use super::model::{AttrRow, Progress};

const STORE_DIR: &str = "/nix/store/";

pub fn router(server: &Server) -> Router<Arc<Server>> {
    let router = Router::new()
        .merge(auth::router())
        .route("/api/runs", get(list_runs))
        .route("/api/repos/{owner}/{repo}/commits/{sha}", get(commit))
        .route(
            "/api/repos/{owner}/{repo}/commits/{sha}/runs/{jobset}/builds",
            get(run_builds),
        )
        .route(
            "/api/repos/{owner}/{repo}/commits/{sha}/runs/{jobset}/builds/{name}/log",
            get(run_build_log),
        )
        .route(
            "/api/repos/{owner}/{repo}/commits/{sha}/runs/{jobset}/rerun",
            post(rerun_run),
        )
        .route("/api/builds/{name}", get(build_detail))
        .route("/api/builds/{name}/log", get(build_log))
        .route("/api/workers", get(workers))
        .route("/api/{*rest}", any(|| async { StatusCode::NOT_FOUND }));
    match server
        .config
        .web
        .as_ref()
        .and_then(|web| web.assets.as_ref())
    {
        // The frontend routes every other path itself.
        Some(assets) => router.fallback_service(
            ServeDir::new(assets).fallback(ServeFile::new(assets.join("index.html"))),
        ),
        None => router,
    }
}

/// Runs a query that returns one JSON value.
async fn json(
    query: sqlx::query::QueryAs<'_, sqlx::Postgres, (Value,), sqlx::postgres::PgArguments>,
    db: &sqlx::PgPool,
) -> Result<Json<Value>, AppError> {
    let (value,) = query.fetch_one(db).await?;
    Ok(Json(value))
}

/// The refs whose head the commit of run `r` has been, as a JSON array.
macro_rules! run_refs {
    () => {
        "(SELECT coalesce(json_agg(json_build_object('kind', rr.kind, 'name', rr.name) \
         ORDER BY rr.kind, rr.name), '[]') FROM run_refs rr WHERE rr.run_id = r.id)"
    };
}

#[derive(Deserialize)]
struct RunFilter {
    /// Only runs that are queued, evaluating or building.
    #[serde(default)]
    active: bool,
    repo: Option<String>,
    /// A branch name, or a pull request number, that pointed at the commit.
    r#ref: Option<String>,
    /// A prefix of the commit hash.
    sha: Option<String>,
    jobset: Option<String>,
    /// Only runs listed after the one with this `id`, for paging.
    before: Option<i64>,
    limit: Option<i64>,
}

async fn list_runs(
    State(server): State<Arc<Server>>,
    _: User,
    Query(filter): Query<RunFilter>,
) -> Result<Response, AppError> {
    if let Some(sha) = &filter.sha
        && !is_hex(sha)
    {
        return Ok((StatusCode::BAD_REQUEST, "sha must be hexadecimal").into_response());
    }
    let query = concat!(
        "SELECT coalesce(json_agg(t ORDER BY t.id DESC), '[]') FROM ( \
           SELECT r.id, r.repo, r.sha, r.jobset, r.state, left(r.error, 200) AS error, \
           (SELECT n.sha FROM runs n WHERE n.id = r.superseded_by) AS superseded_by, \
           r.created_at, r.started_at, r.evaluated_at, r.finished_at, ",
        run_refs!(),
        " AS refs, ( \
             SELECT coalesce(json_object_agg(state, n), '{}') FROM ( \
               SELECT b.state, count(*) AS n FROM run_build_states b \
               WHERE b.run_id = r.id GROUP BY b.state \
             ) s \
           ) AS builds \
           FROM runs r \
           WHERE (NOT $1 OR r.state IN ('queued', 'evaluating', 'building')) \
           AND ($2::text IS NULL OR r.repo = $2) \
           AND ($3::text IS NULL OR EXISTS ( \
             SELECT 1 FROM run_refs rr WHERE rr.run_id = r.id AND rr.name = $3)) \
           AND ($4::text IS NULL OR r.sha LIKE $4 || '%') \
           AND ($5::text IS NULL OR r.jobset = $5) \
           AND ($6::bigint IS NULL OR r.id < $6) \
           ORDER BY r.id DESC LIMIT $7 \
         ) t"
    );
    Ok(json(
        sqlx::query_as(query)
            .bind(filter.active)
            .bind(filter.repo)
            .bind(filter.r#ref)
            .bind(filter.sha)
            .bind(filter.jobset)
            .bind(filter.before)
            .bind(filter.limit.unwrap_or(50).clamp(1, 500)),
        &server.db,
    )
    .await?
    .into_response())
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// A commit of a repository, given as `owner`, `repo` and a hash prefix.
#[derive(Deserialize)]
struct CommitPath {
    owner: String,
    repo: String,
    sha: String,
}

/// The run of a jobset at a commit.
#[derive(Deserialize)]
struct RunPath {
    owner: String,
    repo: String,
    sha: String,
    jobset: String,
}

/// Why a commit or run in a path was not found.
enum Lookup {
    NotHex,
    Missing,
    Ambiguous,
}

impl IntoResponse for Lookup {
    fn into_response(self) -> Response {
        match self {
            Self::NotHex => {
                (StatusCode::BAD_REQUEST, "the commit must be hexadecimal").into_response()
            }
            Self::Missing => StatusCode::NOT_FOUND.into_response(),
            Self::Ambiguous => {
                (StatusCode::CONFLICT, "more than one commit has this prefix").into_response()
            }
        }
    }
}

/// Finds the full hash of a commit with runs.
async fn resolve_commit(
    server: &Server,
    repo: &str,
    prefix: &str,
) -> sqlx::Result<Result<String, Lookup>> {
    if !is_hex(prefix) {
        return Ok(Err(Lookup::NotHex));
    }
    let shas: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT sha FROM runs WHERE repo = $1 AND sha LIKE $2 || '%' LIMIT 2",
    )
    .bind(repo)
    .bind(prefix.to_ascii_lowercase())
    .fetch_all(&server.db)
    .await?;
    Ok(match <[String; 1]>::try_from(shas) {
        Ok([sha]) => Ok(sha),
        Err(shas) if shas.is_empty() => Err(Lookup::Missing),
        Err(_) => Err(Lookup::Ambiguous),
    })
}

async fn run_id(server: &Server, path: &RunPath) -> sqlx::Result<Result<i64, Lookup>> {
    let repo = format!("{}/{}", path.owner, path.repo);
    let sha = match resolve_commit(server, &repo, &path.sha).await? {
        Ok(sha) => sha,
        Err(lookup) => return Ok(Err(lookup)),
    };
    let id = sqlx::query_scalar("SELECT id FROM runs WHERE repo = $1 AND sha = $2 AND jobset = $3")
        .bind(&repo)
        .bind(&sha)
        .bind(&path.jobset)
        .fetch_optional(&server.db)
        .await?;
    Ok(id.ok_or(Lookup::Missing))
}

#[derive(Serialize)]
struct Commit {
    repo: String,
    sha: String,
    /// The watched refs whose head this commit is.
    heads: Value,
    runs: Vec<RunDetail>,
}

#[derive(Serialize)]
struct RunDetail {
    #[serde(flatten)]
    run: Value,
    progress: Progress,
    /// Where the run's time went, to tell a slow build from a slow scheduler.
    timing: Value,
    attrs: Vec<Attr>,
}

#[derive(Serialize)]
struct Attr {
    attr: String,
    status: &'static str,
    drv_path: Option<String>,
    outputs: Option<Value>,
    error: Option<String>,
}

async fn commit(
    State(server): State<Arc<Server>>,
    _: User,
    Path(path): Path<CommitPath>,
) -> Result<Response, AppError> {
    let repo = format!("{}/{}", path.owner, path.repo);
    let sha = match resolve_commit(&server, &repo, &path.sha).await? {
        Ok(sha) => sha,
        Err(lookup) => return Ok(lookup.into_response()),
    };
    let (heads,): (Value,) = sqlx::query_as(
        "SELECT coalesce(json_agg(json_build_object('kind', kind, 'name', name) \
         ORDER BY kind, name), '[]') FROM refs WHERE repo = $1 AND sha = $2",
    )
    .bind(&repo)
    .bind(&sha)
    .fetch_one(&server.db)
    .await?;
    let rows: Vec<(i64, Value)> = sqlx::query_as(concat!(
        "SELECT r.id, to_json(t) FROM runs r, LATERAL ( \
           SELECT r.jobset, r.attr, r.flake_ref, r.state, r.error, \
           (SELECT n.sha FROM runs n WHERE n.id = r.superseded_by) AS superseded_by, \
           r.created_at, r.started_at, r.evaluated_at, r.finished_at, ",
        run_refs!(),
        " AS refs) t WHERE r.repo = $1 AND r.sha = $2 ORDER BY r.jobset"
    ))
    .bind(&repo)
    .bind(&sha)
    .fetch_all(&server.db)
    .await?;
    let mut runs = Vec::new();
    for (id, run) in rows {
        runs.push(RunDetail {
            run,
            progress: Progress::of_run(&server.db, id).await?,
            timing: timing(&server.db, id).await?,
            attrs: attrs(&server.db, id).await?,
        });
    }
    Ok(Json(Commit {
        repo,
        sha,
        heads,
        runs,
    })
    .into_response())
}

async fn timing(db: &sqlx::PgPool, run_id: i64) -> sqlx::Result<Value> {
    sqlx::query_scalar(
        "SELECT json_build_object( \
           'queued_seconds', extract(epoch FROM r.started_at - r.queued_at), \
           'evaluation_seconds', extract(epoch FROM r.evaluated_at - r.started_at), \
           'building_seconds', extract(epoch FROM coalesce(r.finished_at, now()) - r.evaluated_at), \
           'builds', ( \
             SELECT json_build_object( \
               'started', count(b.started_at), \
               'wait_seconds_total', sum(extract(epoch FROM b.started_at - b.ready_at)), \
               'wait_seconds_max', max(extract(epoch FROM b.started_at - b.ready_at)), \
               'build_seconds_total', sum(extract(epoch FROM b.built_at - b.started_at)), \
               'build_seconds_max', max(extract(epoch FROM b.built_at - b.started_at)), \
               'upload_seconds_total', sum(extract(epoch FROM b.finished_at - b.built_at)) \
                 FILTER (WHERE b.state = 'succeeded'), \
               'upload_seconds_max', max(extract(epoch FROM b.finished_at - b.built_at)) \
                 FILTER (WHERE b.state = 'succeeded') \
             ) FROM run_build_states b WHERE b.run_id = r.id \
           ) \
         ) FROM runs r WHERE r.id = $1",
    )
    .bind(run_id)
    .fetch_one(db)
    .await
}

async fn attrs(db: &sqlx::PgPool, run_id: i64) -> sqlx::Result<Vec<Attr>> {
    Ok(AttrRow::of_run(db, run_id)
        .await?
        .into_iter()
        .map(|row| Attr {
            status: row.status().as_str(),
            attr: row.attr,
            drv_path: row.drv_path,
            outputs: row.outputs.map(|outputs| serde_json::json!(outputs.0)),
            error: row.error,
        })
        .collect())
}

#[derive(Deserialize)]
struct BuildFilter {
    state: Option<String>,
}

async fn run_builds(
    State(server): State<Arc<Server>>,
    _: User,
    Path(path): Path<RunPath>,
    Query(filter): Query<BuildFilter>,
) -> Result<Response, AppError> {
    let id = match run_id(&server, &path).await? {
        Ok(id) => id,
        Err(lookup) => return Ok(lookup.into_response()),
    };
    Ok(json(
        sqlx::query_as(
            "SELECT coalesce(json_agg(t ORDER BY t.state, t.started_at DESC NULLS LAST), '[]') FROM ( \
               SELECT b.drv_path, b.system, b.state, b.height, b.worker, b.attempts, \
               b.queued_at, b.ready_at, b.started_at, b.built_at, b.finished_at, \
               left(b.error, 500) AS error \
               FROM run_build_states b \
               WHERE b.run_id = $1 AND ($2::text IS NULL OR b.state::text = $2) \
             ) t",
        )
        .bind(id)
        .bind(filter.state),
        &server.db,
    )
    .await?
    .into_response())
}

async fn rerun_run(
    State(server): State<Arc<Server>>,
    User(login): User,
    Path(path): Path<RunPath>,
) -> Result<Response, AppError> {
    let id = match run_id(&server, &path).await? {
        Ok(id) => id,
        Err(lookup) => return Ok(lookup.into_response()),
    };
    if rerun(&server, id).await? {
        tracing::info!(run = id, login, "rerun requested");
        Ok(StatusCode::ACCEPTED.into_response())
    } else {
        Ok((
            StatusCode::CONFLICT,
            "only a finished run of a commit that a watched ref points at can run again, \
             once its pins and issues are updated, and not one reporting an invalid amphi.toml",
        )
            .into_response())
    }
}

/// `name` is the derivation's file name, e.g. `<hash>-hello-2.12.drv`.
async fn build_detail(
    State(server): State<Arc<Server>>,
    _: User,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let build: Option<(Value,)> = sqlx::query_as(
        "SELECT to_json(t) FROM ( \
           SELECT b.drv_path, b.system, b.required_features, b.state, b.height, b.worker, \
           b.attempts, b.queued_at, b.ready_at, b.started_at, b.built_at, b.finished_at, \
           b.outputs, b.error, \
           (SELECT coalesce(json_agg(d.dep ORDER BY d.dep), '[]') FROM build_deps d \
            WHERE d.drv_path = b.drv_path) AS deps, \
           (SELECT coalesce(json_agg(json_build_object( \
              'repo', r.repo, 'sha', r.sha, 'jobset', r.jobset, 'state', r.state \
            ) ORDER BY r.id DESC), '[]') \
            FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
            WHERE rb.drv_path = b.drv_path) AS runs \
           FROM builds b WHERE b.drv_path = $1 \
         ) t",
    )
    .bind(format!("{STORE_DIR}{name}"))
    .fetch_optional(&server.db)
    .await?;
    Ok(match build {
        Some((build,)) => Json(build).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

/// The end of the build's latest log, as plain text.
async fn build_log(
    State(server): State<Arc<Server>>,
    _: User,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let log = sqlx::query_scalar("SELECT log_tail FROM builds WHERE drv_path = $1")
        .bind(format!("{STORE_DIR}{name}"))
        .fetch_optional(&server.db)
        .await?;
    Ok(log_response(log))
}

#[derive(Deserialize)]
struct RunBuildPath {
    #[serde(flatten)]
    run: RunPath,
    name: String,
}

/// The end of the build's log as the run saw it, which a later run building
/// the derivation again leaves unchanged.
async fn run_build_log(
    State(server): State<Arc<Server>>,
    _: User,
    Path(path): Path<RunBuildPath>,
) -> Result<Response, AppError> {
    let id = match run_id(&server, &path.run).await? {
        Ok(id) => id,
        Err(lookup) => return Ok(lookup.into_response()),
    };
    let log = sqlx::query_scalar(
        "SELECT log_tail FROM run_build_states WHERE run_id = $1 AND drv_path = $2",
    )
    .bind(id)
    .bind(format!("{STORE_DIR}{}", path.name))
    .fetch_optional(&server.db)
    .await?;
    Ok(log_response(log))
}

fn log_response(log: Option<Option<String>>) -> Response {
    match log {
        Some(log) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            log.unwrap_or_default(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn workers(State(server): State<Arc<Server>>, _: User) -> Result<Json<Value>, AppError> {
    json(
        sqlx::query_as(
            "SELECT coalesce(json_agg(t ORDER BY t.name), '[]') FROM ( \
               SELECT w.name, w.last_seen, w.status, ( \
                 SELECT coalesce(json_agg(json_build_object( \
                   'drv_path', b.drv_path, 'state', b.state, 'started_at', b.started_at, \
                   'built_at', b.built_at \
                 ) ORDER BY b.started_at), '[]') \
                 FROM builds b WHERE b.worker = w.name AND b.state IN ('running', 'uploading') \
               ) AS builds \
               FROM workers w \
             ) t",
        ),
        &server.db,
    )
    .await
}
