//! Pushes going all the way through: polling, evaluation, builds, checks,
//! pins and failure issues.

use std::collections::BTreeSet;

use sqlx::PgPool;

use super::fake_github;
use super::harness::*;
use crate::server::model::RunState;
use crate::server::reconciler::CONFIG_JOBSET;
use crate::server::{evaluator, http};

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_push_is_built_reported_pinned_and_tracked(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    let eval = Eval::default()
        .build("lib", &[])
        .job("app", &["lib"])
        .job("broken", &[])
        .attr("cached", "cached");
    harness.push(REPO, "master", "c1", eval).await;

    // The longest chain goes first.
    let lib = worker.take().await;
    assert_eq!(lib.drv_path, drv("lib"));
    let broken = worker.take().await;
    assert_eq!(broken.drv_path, drv("broken"));
    worker.fail(&broken, "error: boom").await;
    // The dependent starts on the worker that built its dependency, while
    // the dependency uploads.
    let next = worker.claim();
    next.assert_waiting().await;
    worker.built(&lib).await;
    let app = next.job().await;
    assert_eq!(app.drv_path, drv("app"));
    worker.uploaded(&lib).await;
    worker.finish(&app).await;

    let run = harness.until_settled(REPO, "c1").await;
    assert_eq!(run.state, RunState::Failed);
    assert!(!evaluator::gc_roots_dir(&harness.server, run.id).exists());
    {
        let github = harness.github();
        let [eval_check] = &github.checks("c1", "eval: x86_64-linux")[..] else {
            panic!("one eval check");
        };
        assert_eq!(eval_check.conclusion.as_deref(), Some("success"));
        let [build_check] = &github.checks("c1", "build: x86_64-linux")[..] else {
            panic!("one build check");
        };
        assert_eq!(build_check.conclusion.as_deref(), Some("failure"));
        assert_eq!(build_check.title, "1 of 3 attributes failed");

        let issues = &github.repos[REPO].issues;
        let [issue] = &issues[..] else {
            panic!("one issue");
        };
        assert!(issue.open);
        assert_eq!(issue.title, "master fails to build x86_64-linux");
        assert!(issue.body.contains("broken"), "{}", issue.body);
        assert!(issue.body.contains("error: boom"), "{}", issue.body);

        // Failed attributes keep their earlier pins, of which there are none.
        let pinned: BTreeSet<&String> = github.pins.values().collect();
        assert_eq!(pinned, BTreeSet::from([&out("app"), &out("cached")]));
    }

    // The fix needs only the fixed build: the others are cached now.
    let eval = Eval::default()
        .attr("app", "app")
        .attr("broken", "fixed")
        .build("fixed", &[])
        .attr("cached", "cached");
    harness.push(REPO, "master", "c2", eval).await;
    let fixed = worker.take().await;
    assert_eq!(fixed.drv_path, drv("fixed"));
    worker.finish(&fixed).await;

    let run = harness.until_settled(REPO, "c2").await;
    assert_eq!(run.state, RunState::Succeeded);
    let github = harness.github();
    let issue = &github.repos[REPO].issues[0];
    assert!(!issue.open);
    assert!(issue.comments[0].starts_with("Fixed in c2"));
    assert!(github.pins.values().any(|path| *path == out("fixed")));
    let [build_check] = &github.checks("c2", "build: x86_64-linux")[..] else {
        panic!("one build check");
    };
    assert_eq!(build_check.conclusion.as_deref(), Some("success"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_invalid_config_fails_its_commit_and_keeps_the_watched_refs(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.take().await;

    harness.commit_with(REPO, "c2", "branches = 1", Err("unused".to_owned()));
    harness.set_branch(REPO, "master", "c2").await;
    let failed: (String, Option<String>, i64) = {
        let mut found = None;
        until("the invalid config has a failed run", async || {
            found = sqlx::query_as(
                "SELECT state::text, error, id FROM runs WHERE repo = $1 AND sha = 'c2' AND jobset = $2",
            )
            .bind(REPO)
            .bind(CONFIG_JOBSET)
            .fetch_optional(&harness.db)
            .await
            .expect("querying runs");
            found.is_some()
        })
        .await;
        found.expect("found")
    };
    assert_eq!(failed.0, "failed");
    assert!(failed.1.is_some_and(|error| error.contains("branches")));
    // The file at the commit cannot change.
    assert!(!http::rerun(&harness.server, failed.2).await.expect("rerun"));

    // Until the file is fixed, the branch stays at the commit it was, whose
    // run keeps building.
    let run = harness.run(REPO, "c1").await.expect("exists");
    assert_eq!(run.state, RunState::Building);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn pull_requests_from_forks_are_not_built_and_branches_go_first(db: PgPool) {
    let harness = Harness::new(db).await;
    harness.commit(REPO, "fork-head", Eval::default().job("fork", &[]));
    harness.commit(REPO, "pull-head", Eval::default().job("pull", &[]));
    harness.github().repo(REPO).pulls = vec![
        fake_github::Pull {
            number: 1,
            head_sha: "fork-head".to_owned(),
            head_repo: "someone/repo".to_owned(),
        },
        fake_github::Pull {
            number: 2,
            head_sha: "pull-head".to_owned(),
            head_repo: REPO.to_owned(),
        },
    ];
    harness
        .push(REPO, "master", "c1", Eval::default().job("branch", &[]))
        .await;
    harness.until_run(REPO, "c1", RunState::Building).await;
    harness
        .until_run(REPO, "pull-head", RunState::Building)
        .await;
    assert!(harness.run(REPO, "fork-head").await.is_none());

    let worker = harness.worker("local-a");
    assert_eq!(worker.take().await.drv_path, drv("branch"));
    assert_eq!(worker.take().await.drv_path, drv("pull"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_repository_configured_unlike_github_spells_it_is_not_polled(db: PgPool) {
    let mut harness = Harness::prepare(db).await;
    {
        let mut github = harness.github();
        let repo = github.repos.remove(REPO).expect("exists");
        github.repos.insert(REPO.to_uppercase(), repo);
    }
    harness.start().await;
    harness
        .push(REPO, "master", "c1", Eval::default().job("a", &[]))
        .await;
    // The other repository is polled after it.
    harness
        .push(OTHER_REPO, "master", "c2", Eval::default().job("b", &[]))
        .await;
    harness
        .until_run(OTHER_REPO, "c2", RunState::Building)
        .await;
    assert!(harness.run(REPO, "c1").await.is_none());
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_queued_run_another_update_holds_is_evaluated_once_it_ends(db: PgPool) {
    let harness = Harness::new(db).await;
    // The evaluator is busy while the run is queued.
    harness.commit(OTHER_REPO, "o1", Eval::default());
    let evaluation = harness.nix.hold("o1");
    harness.set_branch(OTHER_REPO, "master", "o1").await;
    harness
        .until_run(OTHER_REPO, "o1", RunState::Evaluating)
        .await;
    harness.commit(REPO, "c1", Eval::default().job("x", &[]));
    harness.set_branch(REPO, "master", "c1").await;
    let run = harness.until_run(REPO, "c1", RunState::Queued).await;

    // Stands for an update of the run, e.g. the reporter's.
    let mut update = harness.db.begin().await.expect("beginning");
    sqlx::query("SELECT 1 FROM runs WHERE id = $1 FOR UPDATE")
        .bind(run.id)
        .execute(&mut *update)
        .await
        .expect("locking the run");
    evaluation.add_permits(1);
    until("the evaluator waits for the update", async || {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
             AND query LIKE '%state = ''evaluating''%')",
        )
        .fetch_one(&harness.db)
        .await
        .expect("querying activity")
    })
    .await;
    update.commit().await.expect("ending the update");
    harness.until_run(REPO, "c1", RunState::Building).await;
}
