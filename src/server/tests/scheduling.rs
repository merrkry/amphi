//! Free slots get ready builds as soon as there are any, and builds lost
//! with a worker or a response go back in the queue.

use std::time::Instant;

use reqwest::StatusCode;
use sqlx::PgPool;

use super::harness::*;
use crate::api::Transient;
use crate::server::model::{BuildState, RunState};
use crate::server::scheduler;

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn other_workers_get_a_dependent_once_its_dependency_is_uploaded(db: PgPool) {
    let harness = Harness::new(db).await;
    let a = harness.worker("local-a");
    let b = harness.worker("local-b");
    let eval = Eval::default()
        .build("lib", &[])
        .job("one", &["lib"])
        .job("two", &["lib"]);
    harness.push(REPO, "master", "c1", eval).await;

    let lib = a.take().await;
    let waiting = b.claim();
    waiting.assert_waiting().await;
    a.built(&lib).await;
    // Only the worker that built the dependency has its outputs yet.
    let first = a.take().await;
    waiting.assert_waiting().await;
    a.uploaded(&lib).await;
    let second = waiting.job().await;
    let mut names = vec![first.drv_path, second.drv_path];
    names.sort();
    let mut expected = vec![drv("one"), drv("two")];
    expected.sort();
    assert_eq!(names, expected);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_newer_claim_of_a_worker_replaces_its_older_one(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    let older = worker.claim();
    older.assert_waiting().await;
    // E.g. the worker restarted while the server still held its claim.
    let newer = worker.claim();
    newer.assert_waiting().await;
    harness
        .push(REPO, "master", "c1", Eval::default().job("one", &[]))
        .await;
    older.assert_idle().await;
    newer.job().await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_restarted_worker_gets_back_the_builds_of_its_previous_process(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("one", &[]))
        .await;
    worker.take().await;
    // The server still holds the previous process's claim.
    let previous = worker.claim();
    previous.assert_waiting().await;

    worker.start().await;
    previous.assert_idle().await;
    assert_eq!(worker.take().await.drv_path, drv("one"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_busy_worker_waits_only_for_idle_workers_that_can_build(db: PgPool) {
    let harness = Harness::new(db).await;
    let busy = harness.worker("local-a");
    let idle = harness.worker("local-b");
    harness
        .push(REPO, "master", "c1", Eval::default().job("one", &[]))
        .await;
    busy.take().await;
    busy.set_busy(true);

    // An idle worker that can build the next build goes first.
    let busy_claim = busy.claim();
    let idle_claim = idle.claim();
    idle_claim.assert_waiting().await;
    harness
        .push(REPO, "feature", "c2", Eval::default().job("two", &[]))
        .await;
    assert_eq!(idle_claim.job().await.drv_path, drv("two"));
    busy_claim.assert_waiting().await;

    // One that cannot build it does not hold it up.
    idle.set_systems(&["aarch64-linux"]);
    let other_system = idle.claim();
    other_system.assert_waiting().await;
    harness
        .push(REPO, "other", "c3", Eval::default().job("three", &[]))
        .await;
    harness.until_run(REPO, "c3", RunState::Building).await;
    let started = Instant::now();
    assert_eq!(busy_claim.job().await.drv_path, drv("three"));
    // Waiting for the idle worker would take a whole yield.
    assert!(
        started.elapsed() < scheduler::YIELD / 2,
        "{:?}",
        started.elapsed()
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_waiting_worker_that_became_idle_stops_yielding(db: PgPool) {
    let harness = Harness::new(db).await;
    let a = harness.worker("local-a");
    let b = harness.worker("local-b");
    harness
        .push(REPO, "master", "c1", Eval::default().job("first", &[]))
        .await;
    let first = a.take().await;
    a.set_busy(true);
    let waiting_a = a.claim();
    let waiting_b = b.claim();
    waiting_a.assert_waiting().await;
    waiting_b.assert_waiting().await;

    // A's claim started while it was busy.
    a.finish(&first).await;
    a.set_busy(false);
    a.heartbeat().await;
    let two = Eval::default().job("one", &[]).job("two", &[]);
    harness.push(REPO, "feature", "c2", two).await;
    waiting_b.job().await;
    tokio::time::timeout(scheduler::YIELD / 2, waiting_a.job())
        .await
        .expect("the idle worker takes the other build without yielding");
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn builds_no_run_needs_are_aborted_and_taken_again_once_needed(db: PgPool) {
    let harness = Harness::new(db).await;
    let a = harness.worker("local-a");
    let b = harness.worker("local-b");
    harness
        .push(REPO, "master", "c1", Eval::default().job("old", &[]))
        .await;
    let old = a.take().await;

    harness
        .push(REPO, "master", "c2", Eval::default().job("new", &[]))
        .await;
    harness.until_run(REPO, "c1", RunState::Superseded).await;
    assert_eq!(a.heartbeat().await, vec![old.lease]);
    assert_eq!(harness.build("old").await.state, BuildState::Queued);

    assert_eq!(b.take().await.drv_path, drv("new"));

    // A branch that moves back revives the run, within the same poll.
    let waiting = b.claim();
    harness.set_branch(REPO, "master", "c1").await;
    assert_eq!(waiting.job().await.drv_path, drv("old"));
    let run = harness.run(REPO, "c1").await.expect("exists");
    assert_eq!(run.state, RunState::Building);
    assert_eq!(run.execution, 2);
    assert_eq!(a.built(&old).await, StatusCode::GONE);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_build_whose_claim_response_was_lost_goes_back_in_the_queue(db: PgPool) {
    let harness = Harness::new(db).await;
    let a = harness.worker("local-a");
    let b = harness.worker("local-b");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    let lost = a.take().await;
    a.forget(lost.lease);
    let waiting = b.claim();
    // A heartbeat right after the claim can miss a lease still on its way.
    a.heartbeat().await;
    waiting.assert_waiting().await;

    sqlx::query("UPDATE builds SET started_at = now() - interval '1 minute'")
        .execute(&harness.db)
        .await
        .expect("updating builds");
    a.heartbeat().await;
    let job = waiting.job().await;
    assert_eq!(job.drv_path, drv("x"));
    assert_eq!(harness.build("x").await.attempts, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_restarted_worker_uploads_again_and_releases_its_builds(db: PgPool) {
    let harness = Harness::new(db).await;
    let a = harness.worker("local-a");
    let b = harness.worker("local-b");
    harness
        .push(
            REPO,
            "master",
            "c1",
            Eval::default().job("one", &[]).job("two", &[]),
        )
        .await;
    let built = a.take().await;
    let running = a.take().await;
    a.built(&built).await;
    // A repeated report whose response was lost is accepted.
    assert_eq!(a.built(&built).await, StatusCode::NO_CONTENT);

    let waiting = b.claim();
    waiting.assert_waiting().await;
    let resumed = a.start().await;
    let [upload] = &resumed.uploads[..] else {
        panic!("one upload");
    };
    assert_eq!(upload.lease, built.lease);
    assert_eq!(upload.outputs, vec![out(name(&built.drv_path))]);
    assert_eq!(waiting.job().await.drv_path, running.drv_path);
    assert_eq!(a.uploaded(&built).await, StatusCode::NO_CONTENT);
    assert_eq!(harness.build("one").await.attempts, 0);
    assert_eq!(harness.build("two").await.attempts, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_build_fails_after_its_workers_stop_responding_too_often(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    let eval = Eval::default()
        .job("flaky", &[])
        .job("dependent", &["flaky"])
        .job("fine", &[]);
    harness.push(REPO, "master", "c1", eval).await;
    let mut job = worker.take().await;
    assert_eq!(job.drv_path, drv("flaky"));
    let fine = worker.take().await;
    worker.finish(&fine).await;

    for attempt in 1..=3 {
        sqlx::query("UPDATE workers SET last_seen = now() - interval '1 hour'")
            .execute(&harness.db)
            .await
            .expect("updating workers");
        scheduler::expire_orphans(&harness.server)
            .await
            .expect("expiring");
        let build = harness.build("flaky").await;
        assert_eq!(build.attempts, attempt);
        if attempt < 3 {
            assert_eq!(build.state, BuildState::Queued);
            worker.forget(job.lease);
            job = worker.take().await;
            assert_eq!(job.drv_path, drv("flaky"));
        }
    }
    assert_eq!(harness.build("flaky").await.state, BuildState::Failed);
    assert_eq!(
        harness.build("dependent").await.state,
        BuildState::DepFailed
    );
    let run = harness.until_settled(REPO, "c1").await;
    assert_eq!(run.state, RunState::Failed);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_remote_worker_builds_once_the_derivations_are_copied(db: PgPool) {
    let harness = Harness::new(db).await;
    let remote = harness.worker("remote");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    harness.until_run(REPO, "c1", RunState::Building).await;
    let job = remote.take().await;
    assert_eq!(harness.nix.copied("remote"), vec![drv("x")]);
    assert_eq!(harness.build("x").await.worker.as_deref(), Some("remote"));

    // A derivation gone from its store, e.g. collected, is copied again.
    remote.transient(&job, Transient::MissingDerivation).await;
    let job = remote.take().await;
    assert_eq!(harness.nix.copied("remote"), vec![drv("x"), drv("x")]);
    remote.finish(&job).await;
    harness.until_run(REPO, "c1", RunState::Succeeded).await;
}
