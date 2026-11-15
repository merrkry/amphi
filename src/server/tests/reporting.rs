//! What GitHub shows: checks and failure issues, through lost responses,
//! reruns and GitHub's limits, and what Amphi counts of its requests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use super::fake_github::Fault;
use super::harness::*;
use crate::server::model::RunState;
use crate::server::reconciler::CONFIG_JOBSET;

const EVAL_CHECK: &str = "eval: x86_64-linux";
const BUILD_CHECK: &str = "build: x86_64-linux";

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_pin_whose_update_response_was_lost_is_set_again_for_its_branch(db: PgPool) {
    let harness = Harness::new(db).await;
    harness
        .push(REPO, "master", "c1", Eval::default().attr("x", "old"))
        .await;
    harness.until_settled(REPO, "c1").await;
    {
        let mut github = harness.github();
        github.fault("niks3_pin", Fault::LoseResponse);
        // The finalizer's retries fail until the branch moved back.
        for _ in 0..10 {
            github.fault("niks3_pin", Fault::Fail(502));
        }
    }
    harness
        .push(REPO, "master", "c2", Eval::default().attr("x", "new"))
        .await;
    until("niks3 moved the pin", async || {
        harness
            .github()
            .pins
            .values()
            .any(|path| *path == out("new"))
    })
    .await;

    harness.set_branch(REPO, "master", "c1").await;
    until("the branch moved back", async || {
        sqlx::query_scalar("SELECT sha = 'c1' FROM refs WHERE repo = $1 AND name = 'master'")
            .bind(REPO)
            .fetch_one(&harness.db)
            .await
            .expect("querying refs")
    })
    .await;
    harness.github().clear_faults();
    harness.webhook("push", json!({})).await;
    until("the pin is back on the branch's output", async || {
        harness
            .github()
            .pins
            .values()
            .any(|path| *path == out("old"))
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_branch_unpinned_while_its_pins_move_loses_every_pin(db: PgPool) {
    let harness = Harness::new(db).await;
    let both = |prefix: &str| {
        Eval::default()
            .attr("a", &format!("{prefix}-a"))
            .attr("b", &format!("{prefix}-b"))
    };
    harness.push(REPO, "master", "c1", both("old")).await;
    harness.until_settled(REPO, "c1").await;

    let response = Arc::new(Semaphore::new(0));
    harness
        .github()
        .fault("niks3_pin", Fault::Hold(response.clone()));
    harness.push(REPO, "master", "c2", both("new")).await;
    until("the first pin is moving", async || {
        harness
            .github()
            .pins
            .values()
            .any(|path| *path == out("new-a"))
    })
    .await;

    let config = REPO_CONFIG.replace(r#"pin = ["master"]"#, "pin = []");
    harness.commit_with(REPO, "c3", &config, Ok(Eval::default()));
    harness.set_branch(REPO, "master", "c3").await;
    until("the branch is no longer pinned", async || {
        sqlx::query_scalar("SELECT NOT pin FROM refs WHERE repo = $1 AND name = 'master'")
            .bind(REPO)
            .fetch_one(&harness.db)
            .await
            .expect("querying refs")
    })
    .await;
    // The rest of the reconciler's pass happens meanwhile.
    tokio::time::sleep(Duration::from_millis(500)).await;
    response.add_permits(1);
    harness.until_settled(REPO, "c2").await;
    until("every pin is released", async || {
        harness.github().pins.is_empty()
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn the_eval_check_ends_with_the_evaluation_while_builds_run(db: PgPool) {
    let harness = Harness::new(db).await;
    harness.commit(REPO, "c1", Eval::default().job("x", &[]));
    let evaluation = harness.nix.hold("c1");
    harness.set_branch(REPO, "master", "c1").await;
    until("the eval check shows the evaluation", async || {
        let checks = harness.github().checks("c1", EVAL_CHECK);
        checks.iter().any(|check| check.status == "in_progress")
    })
    .await;

    evaluation.add_permits(1);
    harness.until_run(REPO, "c1", RunState::Building).await;
    until("the eval check succeeded", async || {
        let checks = harness.github().checks("c1", EVAL_CHECK);
        checks
            .iter()
            .any(|check| check.conclusion.as_deref() == Some("success"))
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_check_whose_creation_response_was_lost_is_not_created_twice(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness.github().fault("create_check", Fault::LoseResponse);
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    let job = worker.take().await;
    worker.finish(&job).await;
    harness.until_settled(REPO, "c1").await;

    let github = harness.github();
    for name in [EVAL_CHECK, BUILD_CHECK] {
        let [check] = &github.checks("c1", name)[..] else {
            panic!("one check named {name}");
        };
        assert_eq!(check.conclusion.as_deref(), Some("success"), "{name}");
    }
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_rerun_gets_checks_of_its_own_despite_a_late_response(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    let response = Arc::new(Semaphore::new(0));
    harness
        .github()
        .fault("create_check", Fault::Hold(response.clone()));
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: flaky").await;
    let run = harness.until_run(REPO, "c1", RunState::Failed).await;
    until("the run is finalized", async || {
        sqlx::query_scalar("SELECT finalized_seq = finalize_seq FROM runs WHERE id = $1")
            .bind(run.id)
            .fetch_one(&harness.db)
            .await
            .expect("querying runs")
    })
    .await;

    let payload = json!({
        "action": "rerequested",
        "repository": { "full_name": REPO },
        "check_suite": { "head_sha": "c1" },
    });
    harness.webhook("check_suite", payload).await;
    harness.until_run(REPO, "c1", RunState::Building).await;
    // The response to the first execution's first check arrives only now.
    response.add_permits(1);
    worker.finish(&worker.take().await).await;
    let run = harness.until_settled(REPO, "c1").await;
    assert_eq!(run.state, RunState::Succeeded);

    let github = harness.github();
    let checks = github.checks("c1", EVAL_CHECK);
    let external_ids: Vec<_> = checks
        .iter()
        .map(|check| check.external_id.clone().expect("an external ID"))
        .collect();
    assert_eq!(
        external_ids,
        [format!("{}.1", run.id), format!("{}.2", run.id)]
    );
    assert_eq!(run.check_eval_id, Some(checks[1].id));
    let [.., build_check] = &github.checks("c1", BUILD_CHECK)[..] else {
        panic!("build checks");
    };
    assert_eq!(run.check_build_id, Some(build_check.id));
    assert_eq!(build_check.conclusion.as_deref(), Some("success"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_issue_whose_creation_response_was_lost_is_not_opened_twice(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness.github().fault("create_issue", Fault::LoseResponse);
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    until("an issue was created", async || {
        harness.github().count("create_issue") == 1
    })
    .await;

    // The finalizer tries again with the next poll.
    harness.webhook("push", json!({})).await;
    harness.until_settled(REPO, "c1").await;
    let tracked: i64 = sqlx::query_scalar("SELECT issue_number FROM failure_issues")
        .fetch_one(&harness.db)
        .await
        .expect("querying issues");
    let github = harness.github();
    assert_eq!(github.count("create_issue"), 1);
    let [issue] = &github.repos[REPO].issues[..] else {
        panic!("one issue");
    };
    assert_eq!(tracked, issue.number);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_issue_whose_creation_response_was_lost_is_closed_by_a_passing_head(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    {
        let mut github = harness.github();
        github.fault("create_issue", Fault::LoseResponse);
        // The finalizer's retries fail until the branch moved on.
        for _ in 0..10 {
            github.fault("issues", Fault::Fail(502));
        }
    }
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    until("an issue was created", async || {
        harness.github().count("create_issue") == 1
    })
    .await;

    harness.commit(REPO, "c2", Eval::default());
    // A run that ends before the faults are cleared would leave its issue
    // to the finalizer's retry a minute later.
    let evaluation = harness.nix.hold("c2");
    harness.set_branch(REPO, "master", "c2").await;
    until("the branch moved on", async || {
        harness.run(REPO, "c2").await.is_some()
    })
    .await;
    harness.github().clear_faults();
    evaluation.add_permits(1);
    harness.until_settled(REPO, "c2").await;
    let github = harness.github();
    let [issue] = &github.repos[REPO].issues[..] else {
        panic!("one issue");
    };
    assert!(!issue.open);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn the_issue_of_a_renamed_jobset_is_closed(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    harness.until_settled(REPO, "c1").await;

    let config = REPO_CONFIG.replace(JOBSET, "renamed");
    harness.commit_with(REPO, "c2", &config, Ok(Eval::default()));
    harness.set_branch(REPO, "master", "c2").await;
    until("the issue is closed and forgotten", async || {
        let tracked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM failure_issues)")
            .fetch_one(&harness.db)
            .await
            .expect("querying issues");
        !tracked && !harness.github().repos[REPO].issues[0].open
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_head_without_jobsets_loses_the_pins_and_issues_of_its_branch(db: PgPool) {
    let harness = Harness::new(db).await;
    let config = REPO_CONFIG
        .replace(r#"pin = ["master"]"#, r#"pin = ["release"]"#)
        .replace(r#"issues = ["master"]"#, r#"issues = ["release"]"#);
    harness.commit_with(REPO, "c0", &config, Ok(Eval::default()));
    harness.set_branch(REPO, "master", "c0").await;
    harness.until_settled(REPO, "c0").await;
    let eval = Eval::default()
        .attr("good", "good")
        .eval_error("bad", "broken");
    harness.commit_with(REPO, "c1", &config, Ok(eval));
    harness.set_branch(REPO, "release", "c1").await;
    harness.until_settled(REPO, "c1").await;
    assert_eq!(harness.github().pins.len(), 1);
    assert!(harness.github().repos[REPO].issues[0].open);

    // The default branch still watches the branch, whose new head has no
    // amphi.toml.
    harness.set_branch(REPO, "release", "c2").await;
    until("the pin is released and the issue closed", async || {
        let github = harness.github();
        github.pins.is_empty() && !github.repos[REPO].issues[0].open
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn the_issue_of_a_branch_no_longer_tracked_is_closed(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    harness.until_settled(REPO, "c1").await;

    let config = REPO_CONFIG.replace(r#"issues = ["master"]"#, "issues = []");
    harness.commit_with(REPO, "c2", &config, Ok(Eval::default().job("x", &[])));
    harness.set_branch(REPO, "master", "c2").await;
    until("the issue is closed", async || {
        !harness.github().repos[REPO].issues[0].open
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_invalid_config_keeps_the_issues_of_its_branch_open(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    let config = REPO_CONFIG.replace(
        r#"issues = ["master"]"#,
        r#"issues = ["master", "release"]"#,
    );
    harness.commit_with(REPO, "c1", &config, Ok(Eval::default().job("x", &[])));
    harness.set_branch(REPO, "master", "c1").await;
    harness.set_branch(REPO, "release", "c1").await;
    worker.fail(&worker.take().await, "error: boom").await;
    harness.until_settled(REPO, "c1").await;

    harness.commit_with(REPO, "c2", "branches = 1", Err("unused".to_owned()));
    harness.set_branch(REPO, "release", "c2").await;
    until("the invalid config is finalized", async || {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE sha = 'c2' AND jobset = $1 \
             AND finalized_seq = finalize_seq)",
        )
        .bind(CONFIG_JOBSET)
        .fetch_one(&harness.db)
        .await
        .expect("querying runs")
    })
    .await;
    let github = harness.github();
    let mut titles: Vec<&str> = github.repos[REPO]
        .issues
        .iter()
        .filter(|issue| issue.open)
        .map(|issue| issue.title.as_str())
        .collect();
    titles.sort();
    assert_eq!(
        titles,
        [
            "master fails to build x86_64-linux",
            "release fails to build amphi.toml",
            "release fails to build x86_64-linux",
        ]
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_pin_whose_creation_response_was_lost_is_released_with_its_branch(db: PgPool) {
    let harness = Harness::new(db).await;
    {
        let mut github = harness.github();
        github.fault("niks3_pin", Fault::LoseResponse);
        // The finalizer's retries fail until the branch is no longer pinned.
        for _ in 0..10 {
            github.fault("niks3_pin", Fault::Fail(502));
        }
    }
    harness
        .push(REPO, "master", "c1", Eval::default().attr("x", "x"))
        .await;
    until("a pin was created", async || {
        harness.github().count("niks3_pin") >= 1
    })
    .await;

    let config = REPO_CONFIG.replace(r#"pin = ["master"]"#, "pin = []");
    harness.commit_with(REPO, "c2", &config, Ok(Eval::default()));
    harness.set_branch(REPO, "master", "c2").await;
    until("the pin is released", async || {
        harness.github().pins.is_empty()
    })
    .await;
}

fn many_failures(prefix: &str) -> Eval {
    (0..3000).fold(Eval::default(), |eval, i| {
        eval.eval_error(
            &format!("{prefix}{i:04}"),
            &format!("error: attribute {prefix}{i:04} is broken in some long and winding way"),
        )
    })
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn many_failures_fit_in_github_limits(db: PgPool) {
    let harness = Harness::new(db).await;
    harness
        .push(REPO, "master", "c1", many_failures("first"))
        .await;
    let run = harness.until_settled(REPO, "c1").await;
    assert_eq!(run.state, RunState::Failed);
    harness
        .push(REPO, "master", "c2", many_failures("second"))
        .await;
    harness.until_settled(REPO, "c2").await;

    let github = harness.github();
    let [issue] = &github.repos[REPO].issues[..] else {
        panic!("one issue");
    };
    assert!(
        issue
            .body
            .contains("The run's page lists the remaining failures.")
    );
    let [comment] = &issue.comments[..] else {
        panic!("one comment");
    };
    assert!(comment.contains("and 2900 more"), "{comment}");
    let [check] = &github.checks("c2", EVAL_CHECK)[..] else {
        panic!("one eval check");
    };
    assert_eq!(check.conclusion.as_deref(), Some("failure"));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn installation_tokens_are_reused_and_every_request_is_counted(db: PgPool) {
    let harness = Harness::new(db).await;
    let worker = harness.worker("local-a");
    harness
        .push(REPO, "master", "c1", Eval::default().job("x", &[]))
        .await;
    worker.finish(&worker.take().await).await;
    harness.until_settled(REPO, "c1").await;
    harness.github().fault("branches", Fault::Fail(502));
    harness.webhook("push", json!({})).await;
    harness.webhook("push", json!({})).await;

    // One token for each of the two repositories.
    assert_eq!(harness.github().tokens_minted(), 2);
    until("the metrics count every GitHub request", async || {
        let mut sent = BTreeMap::new();
        for (operation, status) in &harness.github().requests {
            if operation.starts_with("niks3") {
                continue;
            }
            let result = if (200..300).contains(status) {
                "ok".to_owned()
            } else {
                status.to_string()
            };
            *sent.entry((operation.clone(), result)).or_insert(0) += 1;
        }
        let counted = harness.github_metrics().await;
        counted.contains_key(&("branches".to_owned(), "502".to_owned())) && counted == sent
    })
    .await;
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_repository_without_the_app_is_built_with_the_configured_token(db: PgPool) {
    let mut harness = Harness::prepare(db).await;
    harness.github().repo(OTHER_REPO).installed = false;
    harness.start().await;
    let worker = harness.worker("local-a");
    harness
        .push(OTHER_REPO, "master", "o1", Eval::default().job("x", &[]))
        .await;
    worker.fail(&worker.take().await, "error: boom").await;
    harness.until_settled(OTHER_REPO, "o1").await;

    // Only apps can create checks; issues work with any token.
    let github = harness.github();
    assert!(github.checks.is_empty());
    assert_eq!(github.repos[OTHER_REPO].issues.len(), 1);
}
