//! Mirrors run states to two GitHub checks per run.
//!
//! The eval check finishes as soon as the jobset is evaluated, so it can be
//! required for merging without waiting for builds. The build check follows
//! the builds. Checks of superseded runs end as skipped rather than failed:
//! GitHub counts skipped checks as passing, and only the latest commit of a
//! ref gates anything.

use std::sync::Arc;
use std::time::Duration;

use super::Server;
use super::config::RepoName;
use super::github::{Check, CheckStatus};
use super::model::{AttrRow, Progress, Run, RunState};
use super::runs;
use super::summary::{self, run_url};

pub async fn run(server: Arc<Server>) {
    loop {
        let wake = server.wake.reporter.notified();
        if let Err(error) = report_due(&server).await {
            tracing::warn!("reporting checks: {error:#}");
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), wake).await;
    }
}

/// Changes of state are reported right away, while the progress of a run
/// whose checks show it building is reported at most every 30 seconds. Runs
/// wait in the order of their last attempt, so that runs whose reports keep
/// failing, e.g. in a repository the app was removed from, do not hold up
/// the others.
async fn report_due(server: &Server) -> anyhow::Result<()> {
    let due: Vec<Run> = sqlx::query_as(
        "SELECT * FROM runs WHERE report_seq > reported_seq \
         AND (state <> 'building' OR reported_state IS DISTINCT FROM 'building' \
           OR reported_at < now() - interval '30 seconds') \
         ORDER BY reported_at NULLS FIRST, id LIMIT 50",
    )
    .fetch_all(&server.db)
    .await?;
    for run in due {
        let update = match report(server, &run).await {
            Ok(()) => sqlx::query(
                "UPDATE runs SET reported_seq = $2, reported_state = $3, reported_at = now() \
                 WHERE id = $1",
            )
            .bind(run.id)
            .bind(run.report_seq)
            .bind(run.state),
            Err(error) => {
                tracing::warn!(run = run.id, "updating checks: {error:#}");
                sqlx::query("UPDATE runs SET reported_at = now() WHERE id = $1").bind(run.id)
            }
        };
        update.execute(&server.db).await?;
    }
    Ok(())
}

async fn report(server: &Server, run: &Run) -> anyhow::Result<()> {
    let repo = RepoName::try_from(run.repo.clone())?;
    let client = server.github.repo(&repo).await?;
    // Only GitHub Apps can create checks.
    if !client.is_app() {
        return Ok(());
    }
    let (eval_check, build_check) = render(server, run).await?;
    let external_id = format!("{}.{}", run.id, run.execution);
    for (update, id, check) in [
        (
            "UPDATE runs SET check_eval_id = $2 WHERE id = $1 AND execution = $3",
            run.check_eval_id,
            eval_check,
        ),
        (
            "UPDATE runs SET check_build_id = $2 WHERE id = $1 AND execution = $3",
            run.check_build_id,
            build_check,
        ),
    ] {
        if let Some(id) = id {
            client.update_check(id, &check).await?;
            continue;
        }
        // A creation whose response was lost, e.g. by a timeout, may have
        // made the check.
        let id = match client.find_check(&run.sha, &check, &external_id).await? {
            Some(id) => {
                client.update_check(id, &check).await?;
                id
            }
            // Nothing to show for runs superseded before they were reported.
            None if run.state == RunState::Superseded => continue,
            None => client.create_check(&run.sha, &external_id, &check).await?,
        };
        // A rerun meanwhile cleared the IDs for its own checks.
        sqlx::query(update)
            .bind(run.id)
            .bind(id)
            .bind(run.execution)
            .execute(&server.db)
            .await?;
    }
    Ok(())
}

async fn render(server: &Server, run: &Run) -> anyhow::Result<(Check, Check)> {
    let check = |kind: &str, status, title: String, summary: String| Check {
        name: format!("{kind}: {}", run.jobset),
        status,
        title,
        summary,
        details_url: run_url(&server.config.public_url, run),
    };
    let superseded_title = match run.superseded_by {
        Some(id) => match runs::get(&server.db, id).await? {
            Some(newer) => format!("Superseded by {}", newer.short_sha()),
            None => "Superseded".to_owned(),
        },
        None => "No watched ref points at this commit".to_owned(),
    };

    let evaluated = run.evaluated_at.is_some();
    let attrs = if evaluated {
        AttrRow::of_run(&server.db, run.id).await?
    } else {
        Vec::new()
    };
    let eval_errors = attrs.iter().filter(|attr| attr.error.is_some()).count();
    let eval_check = match run.state {
        RunState::Queued => check("eval", CheckStatus::Queued, "Queued".into(), String::new()),
        RunState::Evaluating => check(
            "eval",
            CheckStatus::InProgress,
            "Evaluating".into(),
            String::new(),
        ),
        RunState::Superseded if !evaluated => check(
            "eval",
            CheckStatus::Skipped,
            superseded_title.clone(),
            String::new(),
        ),
        _ if !evaluated => check(
            "eval",
            CheckStatus::Failure,
            "Evaluation failed".into(),
            format!(
                "```\n{}\n```",
                run.error.as_deref().unwrap_or_default().trim()
            ),
        ),
        _ if eval_errors > 0 => check(
            "eval",
            CheckStatus::Failure,
            format!(
                "{eval_errors} of {} attributes failed to evaluate",
                attrs.len()
            ),
            summary::failures(&attrs, &[]),
        ),
        _ => check(
            "eval",
            CheckStatus::Success,
            format!("{} attributes", attrs.len()),
            String::new(),
        ),
    };

    let progress = Progress::of_run(&server.db, run.id).await?;
    let counts = format!(
        "{} of {} builds done: {} succeeded, {} failed, {} with failed dependencies, {} running, {} uploading.",
        progress.done(),
        progress.total(),
        progress.succeeded,
        progress.failed,
        progress.dep_failed,
        progress.running,
        progress.uploading,
    );
    let build_check = match run.state {
        RunState::Queued | RunState::Evaluating => check(
            "build",
            CheckStatus::Queued,
            "Waiting for evaluation".into(),
            String::new(),
        ),
        RunState::Superseded => check("build", CheckStatus::Skipped, superseded_title, counts),
        // A failure rather than a skip, which GitHub would count as passing.
        _ if !evaluated => check(
            "build",
            CheckStatus::Failure,
            "Evaluation failed".into(),
            String::new(),
        ),
        RunState::Building => {
            let failed = summary::failed_builds(&server.db, run.id).await?;
            check(
                "build",
                CheckStatus::InProgress,
                format!("{} of {} builds done", progress.done(), progress.total()),
                format!("{counts}\n\n{}", summary::failures(&attrs, &failed)),
            )
        }
        RunState::Succeeded => check(
            "build",
            CheckStatus::Success,
            format!("{} attributes, {} builds", attrs.len(), progress.total()),
            counts,
        ),
        RunState::Failed => {
            let failed = summary::failed_builds(&server.db, run.id).await?;
            let failing = attrs
                .iter()
                .filter(|attr| attr.result.is_some_and(|result| result.is_failure()))
                .count();
            check(
                "build",
                CheckStatus::Failure,
                format!("{failing} of {} attributes failed", attrs.len()),
                format!("{counts}\n\n{}", summary::failures(&attrs, &failed)),
            )
        }
    };
    Ok((eval_check, build_check))
}
