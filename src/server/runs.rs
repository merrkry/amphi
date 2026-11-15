//! Run lifecycle: creation, evaluation results, supersession and reruns.

use std::collections::BTreeMap;

use sqlx::PgPool;

use super::model::{RefKind, Run};
use super::reconciler::CONFIG_JOBSET;

/// Resets a run to be evaluated from scratch.
macro_rules! requeue {
    () => {
        " state = 'queued', error = NULL, superseded_by = NULL, \
         execution = r.execution + 1, queued_at = now(), \
         started_at = NULL, evaluated_at = NULL, finished_at = NULL, \
         finalize_seq = r.finalize_seq + 1, check_eval_id = NULL, \
         check_build_id = NULL, report_seq = r.report_seq + 1 "
    };
}

/// A CTE that keeps the builds of the runs in the CTE `ended` as they are,
/// since later runs may build them again. Within the statement that ends the
/// runs, it sees the builds at the same moment as the statement does. Builds
/// kept by an earlier execution stay as they were, e.g. for a rerun
/// superseded before its evaluation.
macro_rules! keep_builds {
    () => {
        " kept AS ( \
           UPDATE run_builds rb SET state = b.state, worker = b.worker, \
           attempts = b.attempts, queued_at = b.queued_at, ready_at = b.ready_at, \
           started_at = b.started_at, built_at = b.built_at, finished_at = b.finished_at, \
           error = b.error, log_tail = b.log_tail \
           FROM builds b WHERE rb.run_id IN (SELECT id FROM ended) AND rb.state IS NULL \
           AND b.drv_path = rb.drv_path \
         ) "
    };
}

pub struct NewRun {
    pub repo: String,
    pub sha: String,
    pub jobset: String,
    pub attr: String,
    pub flake_ref: String,
    pub priority: i32,
}

/// Creates the run unless it exists.
pub async fn create(db: &mut sqlx::PgConnection, new: &NewRun) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "INSERT INTO runs (repo, sha, jobset, attr, flake_ref, priority) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(&new.repo)
    .bind(&new.sha)
    .bind(&new.jobset)
    .bind(&new.attr)
    .bind(&new.flake_ref)
    .bind(new.priority)
    .fetch_optional(db)
    .await
}

/// Records a failed run, e.g. for a commit whose `amphi.toml` is invalid.
pub async fn create_failed(
    db: &mut sqlx::PgConnection,
    new: &NewRun,
    error: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO runs (repo, sha, jobset, attr, flake_ref, priority, state, error, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 'failed', $7, now()) ON CONFLICT DO NOTHING",
    )
    .bind(&new.repo)
    .bind(&new.sha)
    .bind(&new.jobset)
    .bind(&new.attr)
    .bind(&new.flake_ref)
    .bind(new.priority)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether the commit has runs.
pub async fn exist_for_commit(db: &PgPool, repo: &str, sha: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM runs WHERE repo = $1 AND sha = $2)")
        .bind(repo)
        .bind(sha)
        .fetch_one(db)
        .await
}

/// A watched ref and its head.
pub struct WatchedRef {
    pub kind: RefKind,
    pub name: String,
    pub sha: String,
    pub priority: i32,
    pub pin: bool,
    pub issues: bool,
}

/// Records the watched refs of a repository and attaches them to the runs
/// of their heads. Succeeded and failed runs whose commit gained a ref with
/// pins or issues are finalized again for it.
pub async fn set_refs(
    tx: &mut sqlx::PgConnection,
    repo: &str,
    refs: &[WatchedRef],
) -> sqlx::Result<()> {
    let column = |f: fn(&WatchedRef) -> String| refs.iter().map(f).collect::<Vec<_>>();
    let kinds: Vec<RefKind> = refs.iter().map(|r| r.kind).collect();
    let names = column(|r| r.name.clone());
    let shas = column(|r| r.sha.clone());
    sqlx::query(
        "DELETE FROM refs WHERE repo = $1 \
         AND (kind, name) NOT IN (SELECT * FROM unnest($2::ref_kind[], $3::text[]))",
    )
    .bind(repo)
    .bind(&kinds)
    .bind(&names)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH changed AS ( \
           INSERT INTO refs (repo, kind, name, sha, pin, issues) \
           SELECT $1, * FROM unnest($2::ref_kind[], $3::text[], $4::text[], $5::bool[], $6::bool[]) \
           ON CONFLICT (repo, kind, name) DO UPDATE \
           SET sha = EXCLUDED.sha, pin = EXCLUDED.pin, issues = EXCLUDED.issues \
           WHERE (refs.sha, refs.pin, refs.issues) \
             IS DISTINCT FROM (EXCLUDED.sha, EXCLUDED.pin, EXCLUDED.issues) \
           RETURNING sha, pin OR issues AS tracked \
         ) \
         UPDATE runs SET finalize_seq = finalize_seq + 1 \
         WHERE repo = $1 AND state IN ('succeeded', 'failed') \
         AND sha IN (SELECT sha FROM changed WHERE tracked)",
    )
    .bind(repo)
    .bind(&kinds)
    .bind(&names)
    .bind(&shas)
    .bind(refs.iter().map(|r| r.pin).collect::<Vec<_>>())
    .bind(refs.iter().map(|r| r.issues).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO run_refs (run_id, kind, name) \
         SELECT r.id, t.kind, t.name FROM unnest($2::ref_kind[], $3::text[], $4::text[]) \
           AS t(kind, name, sha) \
         JOIN runs r ON r.repo = $1 AND r.sha = t.sha \
         ON CONFLICT DO NOTHING",
    )
    .bind(repo)
    .bind(&kinds)
    .bind(&names)
    .bind(&shas)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE runs r SET priority = t.priority \
         FROM (SELECT sha, max(priority) AS priority FROM unnest($2::text[], $3::int[]) \
               AS t(sha, priority) GROUP BY sha) t \
         WHERE r.repo = $1 AND r.sha = t.sha AND r.priority < t.priority",
    )
    .bind(repo)
    .bind(&shas)
    .bind(refs.iter().map(|r| r.priority).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Queues the superseded runs of the repository whose commit a watched ref
/// points at again, e.g. after a branch was reset to it. Like [`rerun`], it
/// waits until the finalizer handled the run.
pub async fn revive(db: &mut sqlx::PgConnection, repo: &str) -> sqlx::Result<u64> {
    let result = sqlx::query(concat!(
        "UPDATE runs r SET",
        requeue!(),
        "WHERE r.repo = $1 AND r.state = 'superseded' AND r.finalized_seq = r.finalize_seq \
         AND EXISTS (SELECT 1 FROM refs f WHERE f.repo = r.repo AND f.sha = r.sha)"
    ))
    .bind(repo)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Supersedes the active runs of the repository that no watched ref needs.
///
/// A run is needed while a watched ref points at its commit. Once the ref
/// moves on, the run keeps going until the runs of the new commit have been
/// evaluated, whatever their jobsets are called, since most of their builds
/// are usually shared. A queued run has nothing to share and stops right
/// away.
pub async fn supersede_unneeded(db: &mut sqlx::PgConnection, repo: &str) -> sqlx::Result<u64> {
    let superseded: Vec<i64> = sqlx::query_scalar(concat!(
        "WITH ended AS ( \
         UPDATE runs r SET state = 'superseded', finished_at = now(), \
         report_seq = r.report_seq + 1, superseded_by = ( \
           SELECT max(h.id) FROM run_refs rr \
           JOIN refs f ON f.repo = r.repo AND f.kind = rr.kind AND f.name = rr.name \
           JOIN runs h ON h.repo = f.repo AND h.sha = f.sha \
           WHERE rr.run_id = r.id \
         ) \
         WHERE r.repo = $1 AND r.state IN ('queued', 'evaluating', 'building') \
         AND NOT EXISTS (SELECT 1 FROM refs f WHERE f.repo = r.repo AND f.sha = r.sha) \
         AND NOT (r.state <> 'queued' AND EXISTS ( \
           SELECT 1 FROM run_refs rr \
           JOIN refs f ON f.repo = r.repo AND f.kind = rr.kind AND f.name = rr.name \
           JOIN runs h ON h.repo = f.repo AND h.sha = f.sha \
           WHERE rr.run_id = r.id AND h.state IN ('queued', 'evaluating') \
         )) RETURNING r.id), ",
        keep_builds!(),
        "SELECT id FROM ended"
    ))
    .bind(repo)
    .fetch_all(db)
    .await?;
    Ok(superseded.len() as u64)
}

/// Takes the next queued run for evaluation. A run that another update holds
/// a lock on, e.g. the reporter's, is waited for rather than skipped, since
/// the end of that update does not wake the evaluator.
pub async fn start_next(db: &PgPool) -> sqlx::Result<Option<Run>> {
    sqlx::query_as(
        "UPDATE runs SET state = 'evaluating', started_at = now(), report_seq = report_seq + 1 \
         WHERE id = (SELECT id FROM runs WHERE state = 'queued' ORDER BY priority DESC, id \
                     LIMIT 1 FOR UPDATE) \
         RETURNING *",
    )
    .fetch_optional(db)
    .await
}

/// Asks the finalizer for another pass over a run superseded while it was
/// evaluated, since the evaluation may have added GC roots after the last one.
pub async fn refinalize_if_superseded(db: &PgPool, run_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE runs SET finalize_seq = finalize_seq + 1 WHERE id = $1 AND state = 'superseded'",
    )
    .bind(run_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Evaluations interrupted by a restart are run again.
pub async fn requeue_interrupted(db: &PgPool) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE runs SET state = 'queued', report_seq = report_seq + 1 WHERE state = 'evaluating'",
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

pub struct EvalOutput {
    pub attrs: Vec<EvaluatedAttr>,
    /// Derivations whose outputs are in no binary cache.
    pub builds: Vec<NewBuild>,
}

pub struct EvaluatedAttr {
    pub attr: String,
    pub drv_path: Option<String>,
    pub outputs: Option<BTreeMap<String, Option<String>>>,
    pub error: Option<String>,
}

pub struct NewBuild {
    pub drv_path: String,
    pub system: String,
    pub required_features: Vec<String>,
    /// Inputs that are builds of the same run.
    pub deps: Vec<String>,
    /// The longest chain of builds of the run waiting for this one, itself included.
    pub height: i32,
}

/// Records the evaluation and queues its builds. Returns false if the run
/// was superseded while it was evaluated.
pub async fn ingest(db: &PgPool, run_id: i64, output: &EvalOutput) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let repo: Option<String> = sqlx::query_scalar(
        "SELECT repo FROM runs WHERE id = $1 AND state = 'evaluating' FOR UPDATE",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(repo) = repo else {
        return Ok(false);
    };

    // A rerun evaluates again from scratch.
    sqlx::query("DELETE FROM run_attrs WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM run_builds WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;

    let attrs = &output.attrs;
    sqlx::query(
        "INSERT INTO run_attrs (run_id, attr, drv_path, outputs, error) \
         SELECT $1, attr, drv_path, outputs::jsonb, error \
         FROM unnest($2::text[], $3::text[], $4::text[], $5::text[]) AS t(attr, drv_path, outputs, error)",
    )
    .bind(run_id)
    .bind(attrs.iter().map(|a| a.attr.clone()).collect::<Vec<_>>())
    .bind(attrs.iter().map(|a| a.drv_path.clone()).collect::<Vec<_>>())
    .bind(
        attrs
            .iter()
            .map(|a| a.outputs.as_ref().map(|o| serde_json::to_string(o).expect("serializable")))
            .collect::<Vec<_>>(),
    )
    .bind(attrs.iter().map(|a| a.error.clone()).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;

    let builds = &output.builds;
    let drv_paths: Vec<String> = builds.iter().map(|b| b.drv_path.clone()).collect();
    sqlx::query(
        "INSERT INTO builds (drv_path, system, required_features, height) \
         SELECT drv_path, system, string_to_array(features, ' '), height \
         FROM unnest($1::text[], $2::text[], $3::text[], $4::int[]) \
           AS t(drv_path, system, features, height) \
         ON CONFLICT (drv_path) DO UPDATE SET height = GREATEST(builds.height, EXCLUDED.height)",
    )
    .bind(&drv_paths)
    .bind(builds.iter().map(|b| b.system.clone()).collect::<Vec<_>>())
    .bind(
        builds
            .iter()
            .map(|b| b.required_features.join(" "))
            .collect::<Vec<_>>(),
    )
    .bind(builds.iter().map(|b| b.height).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;

    let (dependents, deps): (Vec<&str>, Vec<&str>) = builds
        .iter()
        .flat_map(|b| b.deps.iter().map(|dep| (b.drv_path.as_str(), dep.as_str())))
        .unzip();
    sqlx::query(
        "INSERT INTO build_deps (drv_path, dep) SELECT * FROM unnest($1::text[], $2::text[]) \
         ON CONFLICT DO NOTHING",
    )
    .bind(dependents)
    .bind(deps)
    .execute(&mut *tx)
    .await?;

    // The outputs were in no cache when the evaluation began, whatever earlier
    // builds of these derivations recorded: their results may have been
    // collected since. Builds that succeeded during the evaluation are
    // cached now. Failed builds get another try with every new run.
    let requeued: Vec<String> = sqlx::query_scalar(
        "UPDATE builds SET state = 'queued', worker = NULL, lease = NULL, attempts = 0, \
         error = NULL, log_tail = NULL, outputs = NULL, queued_at = now() \
         WHERE drv_path = ANY($1) AND (state IN ('failed', 'dep_failed') \
           OR state = 'succeeded' AND finished_at < (SELECT started_at FROM runs WHERE id = $2)) \
         RETURNING drv_path",
    )
    .bind(&drv_paths)
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await?;

    // Earlier runs may have recorded unfinished dependencies of these builds
    // that this evaluation found cached. The run waits for those too, so that
    // nothing it waits for is left without a run that needs it.
    sqlx::query(
        "WITH RECURSIVE closure(drv_path) AS ( \
           SELECT unnest($2::text[]) \
           UNION \
           SELECT d.dep FROM build_deps d JOIN closure c ON d.drv_path = c.drv_path \
           JOIN builds b ON b.drv_path = d.dep WHERE b.state <> 'succeeded' \
         ) \
         INSERT INTO run_builds (run_id, drv_path) SELECT $1, drv_path FROM closure",
    )
    .bind(run_id)
    .bind(&drv_paths)
    .execute(&mut *tx)
    .await?;
    // Other building runs found the requeued builds succeeded, so they wait
    // for dependents of them, but not for them. They wait for them now, with
    // their unfinished dependencies, so that a dependent never waits for a
    // build that no run needs, e.g. once this run is superseded.
    sqlx::query(
        "WITH RECURSIVE waiting(run_id, drv_path) AS ( \
           SELECT DISTINCT rb.run_id, d.dep FROM build_deps d \
           JOIN builds dependent ON dependent.drv_path = d.drv_path AND dependent.state = 'queued' \
           JOIN run_builds rb ON rb.drv_path = d.drv_path \
           JOIN runs r ON r.id = rb.run_id AND r.state = 'building' \
           WHERE d.dep = ANY($1) \
           UNION \
           SELECT w.run_id, d.dep FROM build_deps d JOIN waiting w ON d.drv_path = w.drv_path \
           JOIN builds b ON b.drv_path = d.dep WHERE b.state <> 'succeeded' \
         ) \
         INSERT INTO run_builds (run_id, drv_path) SELECT run_id, drv_path FROM waiting \
         ON CONFLICT DO NOTHING",
    )
    .bind(&requeued)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE builds b SET state = 'queued', attempts = 0, error = NULL, log_tail = NULL, \
         queued_at = now() \
         FROM run_builds rb WHERE rb.run_id = $1 AND rb.drv_path = b.drv_path \
         AND b.state IN ('failed', 'dep_failed')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE runs SET state = 'building', evaluated_at = now(), report_seq = report_seq + 1 \
         WHERE id = $1",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    supersede_unneeded(&mut tx, &repo).await?;
    finish_if_done(&mut tx, &[run_id]).await?;
    tx.commit().await?;
    Ok(true)
}

/// Records that the evaluation as a whole failed, e.g. the flake did not fetch.
pub async fn fail_evaluation(db: &PgPool, run_id: i64, error: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let repo: Option<String> = sqlx::query_scalar(
        "UPDATE runs SET state = 'failed', error = $2, finished_at = now(), \
         report_seq = report_seq + 1 WHERE id = $1 AND state = 'evaluating' RETURNING repo",
    )
    .bind(run_id)
    .bind(error)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(repo) = repo {
        supersede_unneeded(&mut tx, &repo).await?;
    }
    tx.commit().await
}

/// Finishes the building runs among `run_ids` that have no unfinished builds.
pub async fn finish_if_done(
    tx: &mut sqlx::PgConnection,
    run_ids: &[i64],
) -> sqlx::Result<Vec<i64>> {
    #[derive(sqlx::FromRow)]
    struct Finished {
        id: i64,
        repo: String,
        sha: String,
        jobset: String,
        state: String,
        evaluation_seconds: f64,
        building_seconds: f64,
    }
    // One statement, so that the run's state, its attributes' results and
    // its kept builds see the builds at the same moment, while evaluations
    // of other runs may queue shared builds again.
    let finished: Vec<Finished> = sqlx::query_as(concat!(
        "WITH ended AS ( \
         UPDATE runs r SET finished_at = now(), report_seq = r.report_seq + 1, \
         state = CASE WHEN EXISTS ( \
             SELECT 1 FROM run_builds rb JOIN builds b USING (drv_path) \
             WHERE rb.run_id = r.id AND b.state IN ('failed', 'dep_failed') \
           ) OR EXISTS (SELECT 1 FROM run_attrs a WHERE a.run_id = r.id AND a.error IS NOT NULL) \
           THEN 'failed'::run_state ELSE 'succeeded'::run_state END \
         WHERE r.id = ANY($1) AND r.state = 'building' AND NOT EXISTS ( \
           SELECT 1 FROM run_builds rb JOIN builds b USING (drv_path) \
           WHERE rb.run_id = r.id AND b.state IN ('queued', 'running', 'uploading') \
         ) RETURNING r.id, r.repo, r.sha, r.jobset, r.state::text, \
         extract(epoch FROM r.evaluated_at - r.started_at)::float8 AS evaluation_seconds, \
         extract(epoch FROM now() - r.evaluated_at)::float8 AS building_seconds \
         ), results AS ( \
         UPDATE run_attrs a SET result = CASE \
           WHEN a.error IS NOT NULL THEN 'eval_error'::attr_result \
           ELSE coalesce(( \
             SELECT b.state::text::attr_result FROM run_builds rb JOIN builds b USING (drv_path) \
             WHERE rb.run_id = a.run_id AND rb.drv_path = a.drv_path \
           ), 'cached') END \
         WHERE a.run_id IN (SELECT id FROM ended) \
         ), ",
        keep_builds!(),
        "SELECT * FROM ended"
    ))
    .bind(run_ids)
    .fetch_all(&mut *tx)
    .await?;
    for run in &finished {
        tracing::info!(
            run = run.id,
            repo = run.repo,
            sha = run.sha,
            jobset = run.jobset,
            state = run.state,
            evaluation_seconds = run.evaluation_seconds,
            building_seconds = run.building_seconds,
            "run finished"
        );
    }
    Ok(finished.into_iter().map(|run| run.id).collect())
}

/// Evaluates a finished run again, which retries its failed builds. Only
/// runs of commits that a watched ref points at can run again, so that pins
/// never go backwards, and only once the finalizer handled the run, which
/// would otherwise act on the new execution, e.g. remove its GC roots. A
/// run reporting an invalid `amphi.toml` cannot, since the file at its
/// commit stays the same.
pub async fn rerun(db: &PgPool, run_id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query(concat!(
        "UPDATE runs r SET",
        requeue!(),
        "WHERE r.id = $1 AND r.state IN ('succeeded', 'failed') AND r.jobset <> $2 \
         AND r.finalized_seq = r.finalize_seq \
         AND EXISTS (SELECT 1 FROM refs f WHERE f.repo = r.repo AND f.sha = r.sha)"
    ))
    .bind(run_id)
    .bind(CONFIG_JOBSET)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get(db: &PgPool, run_id: i64) -> sqlx::Result<Option<Run>> {
    sqlx::query_as("SELECT * FROM runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(db)
        .await
}
