//! Runs sharing builds, superseding each other and surviving restarts.

use sqlx::PgPool;

use super::harness::*;
use crate::server::evaluator;
use crate::server::model::{BuildState, RunState};

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_shared_build_queued_again_is_awaited_by_every_run_with_dependents(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(
            REPO,
            "master",
            "c1",
            Eval::default().build("shared", &[]).job("app", &["shared"]),
        )
        .await;
    worker.finish(&worker.take().await).await;

    // The next commit finds `shared` cached, so its run waits for `app` alone.
    harness
        .push(REPO, "master", "c2", Eval::default().job("app", &[]))
        .await;
    let waiting = harness.until_run(REPO, "c2", RunState::Building).await;
    assert_eq!(harness.run_builds(waiting.id).await, vec!["app"]);

    // Another repository finds the outputs of `shared` gone from the cache.
    harness
        .push(
            OTHER_REPO,
            "master",
            "o1",
            Eval::default().job("shared", &[]),
        )
        .await;
    harness
        .until_run(OTHER_REPO, "o1", RunState::Building)
        .await;
    assert_eq!(harness.build("shared").await.state, BuildState::Queued);
    assert_eq!(harness.run_builds(waiting.id).await, vec!["app", "shared"]);

    // The run that queued it again goes away, and the other still gets it built.
    harness
        .push(OTHER_REPO, "master", "o2", Eval::default())
        .await;
    harness
        .until_run(OTHER_REPO, "o1", RunState::Superseded)
        .await;
    let shared = worker.take().await;
    assert_eq!(shared.drv_path, drv("shared"));
    worker.finish(&shared).await;
    worker.finish(&worker.take().await).await;
    harness.until_run(REPO, "c2", RunState::Succeeded).await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_run_waits_for_unfinished_dependencies_it_found_cached(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(
            REPO,
            "master",
            "c1",
            Eval::default().build("dep", &[]).job("app", &["dep"]),
        )
        .await;
    harness.until_run(REPO, "c1", RunState::Building).await;

    // The outputs of `dep` reached the cache another way, e.g. through the
    // post-build hook, while its build waits in the queue. `app` still
    // waits for that build.
    harness
        .push(REPO, "master", "c2", Eval::default().job("app", &[]))
        .await;
    harness.until_run(REPO, "c1", RunState::Superseded).await;
    let run = harness.run(REPO, "c2").await.expect("exists");
    assert_eq!(harness.run_builds(run.id).await, vec!["app", "dep"]);
    let dep = worker.take().await;
    assert_eq!(dep.drv_path, drv("dep"));
    worker.finish(&dep).await;
    worker.finish(&worker.take().await).await;
    harness.until_run(REPO, "c2", RunState::Succeeded).await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_build_that_succeeds_during_an_evaluation_needing_it_is_not_built_again(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("shared", &[]))
        .await;
    let shared = worker.take().await;

    harness.commit(OTHER_REPO, "o1", Eval::default().job("shared", &[]));
    let evaluation = harness.nix.hold("o1");
    harness.set_branch(OTHER_REPO, "master", "o1").await;
    harness
        .until_run(OTHER_REPO, "o1", RunState::Evaluating)
        .await;
    worker.finish(&shared).await;
    evaluation.add_permits(1);

    let run = harness
        .until_run(OTHER_REPO, "o1", RunState::Succeeded)
        .await;
    assert_eq!(harness.run_builds(run.id).await, vec!["shared"]);
    let build = harness.build("shared").await;
    assert_eq!(build.state, BuildState::Succeeded);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_run_keeps_its_builds_until_a_renamed_jobset_is_evaluated(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("shared", &[]))
        .await;
    let shared = worker.take().await;

    let config = REPO_CONFIG.replace(JOBSET, "renamed");
    let eval = Eval::default().job("shared", &[]);
    harness.commit_with(REPO, "c2", &config, Ok(eval));
    let evaluation = harness.nix.hold("c2");
    harness.set_branch(REPO, "master", "c2").await;
    until("the new commit is evaluating", async || {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE sha = 'c2' AND state = 'evaluating')",
        )
        .fetch_one(&harness.db)
        .await
        .expect("querying runs")
    })
    .await;
    assert!(!worker.heartbeat().await.contains(&shared.lease));

    evaluation.add_permits(1);
    harness.until_run(REPO, "c1", RunState::Superseded).await;
    worker.finish(&shared).await;
    assert_eq!(harness.build("shared").await.state, BuildState::Succeeded);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_run_superseded_while_evaluating_drops_the_roots_added_after(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.finish(&worker.take().await).await;
    harness.until_settled(REPO, "c1").await;

    harness.commit(REPO, "c2", Eval::default().job("y", &[]));
    let evaluation = harness.nix.hold("c2");
    harness.set_branch(REPO, "master", "c2").await;
    let run = harness.until_run(REPO, "c2", RunState::Evaluating).await;
    // The branch moves back to a commit whose run is done.
    harness.set_branch(REPO, "master", "c1").await;
    let passes = harness.until_settled(REPO, "c2").await.finalize_seq;

    // The evaluation goes on and adds roots after the finalizer's pass.
    evaluation.add_permits(1);
    until("the run is finalized again", async || {
        let run = harness.until_settled(REPO, "c2").await;
        run.finalize_seq > passes
    })
    .await;
    assert!(!evaluator::gc_roots_dir(&harness.server, run.id).exists());
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_ended_run_keeps_its_failure_when_a_later_run_retries_the_build(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    let failed = harness.until_settled(REPO, "c1").await;

    harness
        .push(REPO, "master", "c2", Eval::default().job("x", &[]))
        .await;
    worker.finish(&worker.take().await).await;
    harness.until_run(REPO, "c2", RunState::Succeeded).await;

    let (state, log_tail): (BuildState, Option<String>) = sqlx::query_as(
        "SELECT state, log_tail FROM run_build_states WHERE run_id = $1 AND drv_path = $2",
    )
    .bind(failed.id)
    .bind(drv("x"))
    .fetch_one(&harness.db)
    .await
    .expect("querying run builds");
    assert_eq!(state, BuildState::Failed);
    assert_eq!(log_tail.as_deref(), Some("error: boom"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_restarted_server_evaluates_again_and_drops_stale_roots(db: PgPool) {
    let mut harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness.commit(REPO, "c1", Eval::default().job("x", &[]));
    let evaluation = harness.nix.hold("c1");
    harness.set_branch(REPO, "master", "c1").await;
    let run = harness.until_run(REPO, "c1", RunState::Evaluating).await;
    let roots = evaluator::gc_roots_dir(&harness.server, run.id);
    let base = roots.parent().expect("a parent").to_owned();
    for stale in ["999", "not-a-run"] {
        std::fs::create_dir_all(base.join(stale)).expect("creating roots");
    }

    harness.restart().await;
    evaluation.add_permits(1);
    let job = worker.take().await;
    assert_eq!(job.drv_path, drv("x"));
    let mut left: Vec<String> = std::fs::read_dir(&base)
        .expect("reading roots")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .collect();
    left.sort();
    assert_eq!(left, vec![run.id.to_string()]);

    worker.finish(&job).await;
    harness.until_settled(REPO, "c1").await;
    assert!(!roots.exists());
}
