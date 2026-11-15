//! Build lifecycle: claiming, progress reports and recovery from lost workers.

use std::collections::BTreeMap;

use jiff_sqlx::Timestamp;
use sqlx::PgPool;
use sqlx::types::Json;
use uuid::Uuid;

use super::runs::finish_if_done;
use crate::api::{BuildEvent, Upload};

/// A build handed to a worker.
pub struct Claimed {
    pub drv_path: String,
    pub lease: Uuid,
    pub timing: Timing,
}

/// When a build passed each step, for metrics.
#[derive(Debug, sqlx::FromRow)]
pub struct Timing {
    pub worker: Option<String>,
    pub ready_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub built_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// The condition on `b` for a build the worker can take: queued and ready
/// on worker $1 with systems $2 and features $3, and needed by a building run,
/// one of $4 unless it is null.
///
/// A build is ready once every dependency is in the cache, or was built on
/// this worker and is still uploading.
macro_rules! claimable {
    () => {
        " b.state = 'queued' \
         AND (b.system = ANY($2) OR b.system = 'builtin') \
         AND b.required_features <@ $3 \
         AND NOT EXISTS ( \
           SELECT 1 FROM build_deps d JOIN builds dep ON dep.drv_path = d.dep \
           WHERE d.drv_path = b.drv_path AND dep.state <> 'succeeded' \
           AND NOT (dep.state = 'uploading' AND dep.worker = $1) \
         ) \
         AND EXISTS ( \
           SELECT 1 FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
           WHERE rb.drv_path = b.drv_path AND r.state = 'building' \
           AND ($4::bigint[] IS NULL OR r.id = ANY($4)) \
         ) "
    };
}

/// Takes the most urgent build the worker can take.
///
/// Builds whose dependencies the worker built come first among equally
/// urgent ones, since their inputs are local. `runs` limits the claim to
/// builds of these runs, e.g. the ones whose derivations were copied to the
/// worker.
///
/// The worker of each dependency is looked up by key, so that Postgres starts
/// from the few dependencies of each ready build rather than scanning for the
/// worker's builds once per ready build.
pub async fn claim(
    db: &PgPool,
    worker: &str,
    systems: &[String],
    features: &[String],
    runs: Option<&[i64]>,
) -> sqlx::Result<Option<Claimed>> {
    let lease = Uuid::new_v4();
    let row: Option<ClaimedRow> = sqlx::query_as(concat!(
        "UPDATE builds SET state = 'running', worker = $1, lease = $5, started_at = now(), \
         built_at = NULL, finished_at = NULL, \
         ready_at = GREATEST(c.queued_at, c.inputs_at) \
         FROM ( \
           SELECT b.drv_path, b.queued_at, ( \
             SELECT max(CASE WHEN dep.worker = $1 THEN dep.built_at ELSE dep.finished_at END) \
             FROM build_deps d JOIN builds dep ON dep.drv_path = d.dep WHERE d.drv_path = b.drv_path \
           ) AS inputs_at \
           FROM builds b WHERE",
        claimable!(),
        "ORDER BY \
             (SELECT max(r.priority) FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
              WHERE rb.drv_path = b.drv_path AND r.state = 'building') DESC, \
             (SELECT count(*) FROM build_deps d WHERE d.drv_path = b.drv_path \
              AND (SELECT dep.worker FROM builds dep WHERE dep.drv_path = d.dep) = $1) DESC, \
             b.height DESC, \
             b.queued_at \
           LIMIT 1 FOR UPDATE OF b SKIP LOCKED \
         ) c \
         WHERE builds.drv_path = c.drv_path \
         RETURNING builds.drv_path, builds.worker, builds.ready_at, builds.started_at, \
         builds.built_at, builds.finished_at",
    ))
    .bind(worker)
    .bind(systems)
    .bind(features)
    .bind(runs)
    .bind(lease)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| Claimed {
        drv_path: row.drv_path,
        lease,
        timing: row.timing,
    }))
}

/// Whether the worker could take a build, like [`claim`].
pub async fn can_claim(
    db: &PgPool,
    worker: &str,
    systems: &[String],
    features: &[String],
    runs: Option<&[i64]>,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(concat!(
        "SELECT EXISTS (SELECT 1 FROM builds b WHERE",
        claimable!(),
        ")"
    ))
    .bind(worker)
    .bind(systems)
    .bind(features)
    .bind(runs)
    .fetch_one(db)
    .await
}

#[derive(sqlx::FromRow)]
struct ClaimedRow {
    drv_path: String,
    #[sqlx(flatten)]
    timing: Timing,
}

/// What a report changed.
#[derive(Default)]
pub struct Reported {
    /// The build, if the lease matched a build in the state the event follows.
    pub build: Option<ReportedBuild>,
    /// Other builds may have become ready.
    pub unblocked: bool,
    pub finished_runs: Vec<i64>,
}

macro_rules! returning_build {
    () => {
        " RETURNING drv_path, state::text, attempts, worker, ready_at, started_at, built_at, \
         finished_at"
    };
}

/// A build as a report left it.
#[derive(sqlx::FromRow)]
pub struct ReportedBuild {
    pub drv_path: String,
    pub state: String,
    pub attempts: i32,
    #[sqlx(flatten)]
    pub timing: Timing,
}

pub async fn report(
    db: &PgPool,
    lease: Uuid,
    event: &BuildEvent,
    max_attempts: i32,
) -> sqlx::Result<Reported> {
    let mut tx = db.begin().await?;
    let row: Option<ReportedBuild> = match event {
        BuildEvent::Built { outputs, .. } => {
            sqlx::query_as(concat!(
                "UPDATE builds SET state = 'uploading', outputs = $2, built_at = now(), \
                 error = NULL, log_tail = NULL \
                 WHERE lease = $1 AND state = 'running'",
                returning_build!()
            ))
            .bind(lease)
            .bind(Json(outputs))
            .fetch_optional(&mut *tx)
            .await?
        }
        BuildEvent::Uploaded => {
            sqlx::query_as(concat!(
                "UPDATE builds SET state = 'succeeded', lease = NULL, finished_at = now() \
                 WHERE lease = $1 AND state = 'uploading'",
                returning_build!()
            ))
            .bind(lease)
            .fetch_optional(&mut *tx)
            .await?
        }
        BuildEvent::Failed {
            error, log_tail, ..
        } => {
            sqlx::query_as(concat!(
                "UPDATE builds SET state = 'failed', error = $2, log_tail = $3, lease = NULL, \
                 built_at = now(), finished_at = now() \
                 WHERE lease = $1 AND state = 'running'",
                returning_build!()
            ))
            .bind(lease)
            .bind(error)
            .bind(log_tail)
            .fetch_optional(&mut *tx)
            .await?
        }
        BuildEvent::Transient { error, .. } => {
            sqlx::query_as(concat!(
                "UPDATE builds SET attempts = attempts + 1, lease = NULL, error = $2, \
                 outputs = NULL, queued_at = now(), \
                 state = CASE WHEN attempts + 1 >= $3 THEN 'failed'::build_state ELSE 'queued'::build_state END, \
                 finished_at = CASE WHEN attempts + 1 >= $3 THEN now() END \
                 WHERE lease = $1 AND state IN ('running', 'uploading')",
                returning_build!()
            ))
            .bind(lease)
            .bind(error)
            .bind(max_attempts)
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    let Some(row) = row else {
        return Ok(Reported::default());
    };

    let mut affected = vec![row.drv_path.clone()];
    if row.state == "failed" {
        // Dependents can already be running or uploading on the worker that
        // built this build, if its upload failed. Their leases end, so that
        // they do not go back in the queue to wait for it.
        let dependents: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE dependents(drv_path) AS ( \
               SELECT drv_path FROM build_deps WHERE dep = $1 \
               UNION \
               SELECT d.drv_path FROM build_deps d JOIN dependents x ON d.dep = x.drv_path \
             ) \
             UPDATE builds SET state = 'dep_failed', error = 'dependency failed: ' || $1, \
             lease = NULL, finished_at = now() \
             WHERE drv_path IN (SELECT drv_path FROM dependents) \
             AND state IN ('queued', 'running', 'uploading') \
             RETURNING drv_path",
        )
        .bind(&row.drv_path)
        .fetch_all(&mut *tx)
        .await?;
        affected.extend(dependents);
    }

    let run_ids: Vec<i64> = sqlx::query_scalar(
        "UPDATE runs SET report_seq = report_seq + 1 WHERE state = 'building' AND id IN ( \
           SELECT run_id FROM run_builds WHERE drv_path = ANY($1) \
         ) RETURNING id",
    )
    .bind(&affected)
    .fetch_all(&mut *tx)
    .await?;
    let finished_runs = finish_if_done(&mut tx, &run_ids).await?;
    tx.commit().await?;
    Ok(Reported {
        unblocked: matches!(row.state.as_str(), "uploading" | "succeeded" | "queued"),
        finished_runs,
        build: Some(row),
    })
}

/// Whether the build of the lease is built and uploading.
pub async fn is_uploading(db: &PgPool, lease: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM builds WHERE lease = $1 AND state = 'uploading')",
    )
    .bind(lease)
    .fetch_one(db)
    .await
}

/// Puts builds back in the queue without counting an attempt, since they
/// stopped for reasons unrelated to their derivations, e.g. the worker never
/// received them or no run needs them anymore. Returns how many there were.
pub async fn release(db: &PgPool, leases: &[Uuid]) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE builds SET state = 'queued', lease = NULL, outputs = NULL, queued_at = now() \
         WHERE lease = ANY($1) AND state IN ('running', 'uploading')",
    )
    .bind(leases)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Puts the running builds of a worker that started again back in the
/// queue, since their processes are gone. Returns how many there were.
pub async fn release_running(db: &PgPool, worker: &str) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE builds SET state = 'queued', lease = NULL, queued_at = now() \
         WHERE worker = $1 AND state = 'running'",
    )
    .bind(worker)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Builds of the worker whose outputs it has not finished uploading.
pub async fn uploads(db: &PgPool, worker: &str) -> sqlx::Result<Vec<Upload>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        lease: Uuid,
        drv_path: String,
        outputs: Json<BTreeMap<String, String>>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT lease, drv_path, outputs FROM builds WHERE worker = $1 AND state = 'uploading'",
    )
    .bind(worker)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Upload {
            lease: row.lease,
            drv_path: row.drv_path,
            outputs: row.outputs.0.into_values().collect(),
        })
        .collect())
}

/// Running leases of the worker that no building run needs anymore.
pub async fn unneeded_leases(db: &PgPool, worker: &str) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT b.lease FROM builds b WHERE b.worker = $1 AND b.state = 'running' AND NOT EXISTS ( \
           SELECT 1 FROM run_builds rb JOIN runs r ON r.id = rb.run_id \
           WHERE rb.drv_path = b.drv_path AND r.state = 'building' \
         )",
    )
    .bind(worker)
    .fetch_all(db)
    .await
}

/// Leases assigned to the worker a while ago that it does not report.
pub async fn forgotten_leases(
    db: &PgPool,
    worker: &str,
    known: &[Uuid],
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT lease FROM builds WHERE worker = $1 AND state IN ('running', 'uploading') \
         AND started_at < now() - interval '30 seconds' AND NOT (lease = ANY($2))",
    )
    .bind(worker)
    .bind(known)
    .fetch_all(db)
    .await
}

/// Leases the server holds for the worker among `leases`.
pub async fn held_leases(db: &PgPool, worker: &str, leases: &[Uuid]) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT lease FROM builds WHERE worker = $1 AND state IN ('running', 'uploading') \
         AND lease = ANY($2)",
    )
    .bind(worker)
    .bind(leases)
    .fetch_all(db)
    .await
}

/// Leases of workers that stopped sending heartbeats.
pub async fn orphaned_leases(db: &PgPool, timeout_secs: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT b.lease FROM builds b LEFT JOIN workers w ON w.name = b.worker \
         WHERE b.state IN ('running', 'uploading') \
         AND coalesce(w.last_seen, b.started_at) < now() - make_interval(secs => $1)",
    )
    .bind(timeout_secs as f64)
    .fetch_all(db)
    .await
}
