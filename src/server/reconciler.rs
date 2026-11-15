//! Turns the watched refs of each repository into runs.
//!
//! Which refs are built, pinned and tracked in issues comes from `amphi.toml` on the
//! default branch. The jobsets come from `amphi.toml` at the commit itself,
//! so pull requests can change what they build.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};

use super::Server;
use super::config::RepoName;
use super::github::RepoClient;
use super::model::RefKind;
use super::runs::{self, NewRun, WatchedRef};
use crate::repo_config::{self, RepoConfig};

/// The pseudo-jobset that reports an invalid `amphi.toml` at a commit.
pub const CONFIG_JOBSET: &str = repo_config::FILE_NAME;

pub async fn run(server: Arc<Server>) {
    let interval = Duration::from_secs(server.config.github.poll_interval);
    loop {
        let wake = server.wake.reconciler.notified();
        for repo in &server.config.repos {
            if let Err(error) = reconcile(&server, repo).await {
                tracing::warn!(%repo, "polling the repository: {error:#}");
            }
        }
        server.wake.evaluator.notify_one();
        server.wake.reporter.notify_one();
        server.scheduler.ready.notify_waiters();
        let _ = tokio::time::timeout(interval, wake).await;
    }
}

async fn reconcile(server: &Server, repo: &RepoName) -> anyhow::Result<()> {
    let client = server.github.repo(repo).await?;
    let repository = client.repository().await?;
    // Runs are recorded under the configured name, which webhooks and pull
    // requests must match.
    ensure!(
        repository.full_name == repo.to_string(),
        "GitHub spells the repository {}, which the configuration must match",
        repository.full_name
    );
    let default_branch = repository.default_branch;
    let branches = client.branches().await?;
    let default_sha = branches
        .iter()
        .find(|(name, _)| *name == default_branch)
        .map(|(_, sha)| sha.clone())
        .with_context(|| format!("default branch {default_branch} not found"))?;

    let repo_name = repo.to_string();
    let mut targets = Vec::new();
    if let Some(text) = client.file(repo_config::FILE_NAME, &default_sha).await? {
        let config = match RepoConfig::parse(&text) {
            Ok(config) => config,
            Err(error) => {
                // Which refs to watch is unknown until the file is fixed, so
                // they stay as they are. The commit gets its failed run.
                if !runs::exist_for_commit(&server.db, &repo_name, &default_sha).await? {
                    let default = WatchedRef {
                        kind: RefKind::Branch,
                        name: default_branch.clone(),
                        sha: default_sha,
                        priority: 1,
                        pin: false,
                        issues: false,
                    };
                    let planned = plan_runs(&client, repo, &default).await?;
                    let mut tx = server.db.begin().await?;
                    let created = create_runs(&mut tx, &planned).await?;
                    tx.commit().await?;
                    log_created(&created);
                }
                return Err(
                    error.context(format!("{} on {default_branch}", repo_config::FILE_NAME))
                );
            }
        };
        for (name, sha) in branches {
            if config.branches.matches(&name) {
                targets.push(WatchedRef {
                    kind: RefKind::Branch,
                    pin: config.pin.matches(&name),
                    issues: config.issues.matches(&name),
                    priority: 1,
                    name,
                    sha,
                });
            }
        }
        if config.pull_requests {
            // Pull requests from forks would run untrusted code and push to the cache.
            for pull in client.open_pulls().await? {
                if pull.same_repo {
                    targets.push(WatchedRef {
                        kind: RefKind::Pull,
                        name: pull.number.to_string(),
                        sha: pull.head_sha,
                        priority: 0,
                        pin: false,
                        issues: false,
                    });
                }
            }
        }
    }

    // GitHub is read before the transaction, which should stay short.
    let mut planned = Vec::new();
    let mut seen = HashSet::new();
    for target in &targets {
        if seen.insert(&target.sha)
            && !runs::exist_for_commit(&server.db, &repo_name, &target.sha).await?
        {
            planned.extend(plan_runs(&client, repo, target).await?);
        }
    }
    // New runs and the refs pointing at them appear together. A run without
    // refs would be superseded by whatever supersedes runs meanwhile, e.g.
    // the end of another evaluation.
    let mut tx = server.db.begin().await?;
    let created = create_runs(&mut tx, &planned).await?;
    runs::set_refs(&mut tx, &repo_name, &targets).await?;
    let revived = runs::revive(&mut tx, &repo_name).await?;
    let superseded = runs::supersede_unneeded(&mut tx, &repo_name).await?;
    tx.commit().await?;
    log_created(&created);
    if revived > 0 {
        tracing::info!(%repo, revived, "queued superseded runs whose commit is a head again");
    }
    if superseded > 0 {
        tracing::info!(%repo, superseded, "superseded runs that no ref needs");
    }
    // Also for the pins and issues that the refs no longer want.
    server.wake.finalizer.notify_one();
    Ok(())
}

/// A run to create, failed from the start if the `amphi.toml` of its
/// commit is invalid.
struct PlannedRun {
    run: NewRun,
    error: Option<String>,
}

/// The runs of the target's commit, from its `amphi.toml`.
async fn plan_runs(
    client: &RepoClient,
    repo: &RepoName,
    target: &WatchedRef,
) -> anyhow::Result<Vec<PlannedRun>> {
    let Some(text) = client.file(repo_config::FILE_NAME, &target.sha).await? else {
        return Ok(Vec::new());
    };
    let new_run = |jobset: &str, attr: &str| NewRun {
        repo: repo.to_string(),
        sha: target.sha.clone(),
        jobset: jobset.to_owned(),
        attr: attr.to_owned(),
        flake_ref: format!("github:{}/{}/{}", repo.owner, repo.name, target.sha),
        priority: target.priority,
    };
    Ok(match RepoConfig::parse(&text) {
        Ok(config) => config
            .jobsets
            .iter()
            .map(|jobset| PlannedRun {
                run: new_run(&jobset.name, &jobset.attr),
                error: None,
            })
            .collect(),
        Err(error) => vec![PlannedRun {
            run: new_run(CONFIG_JOBSET, ""),
            error: Some(format!("{error:#}")),
        }],
    })
}

/// Creates the runs, all at once since later polls skip commits that have
/// runs. Returns the queued ones.
async fn create_runs<'a>(
    tx: &mut sqlx::PgConnection,
    planned: &'a [PlannedRun],
) -> sqlx::Result<Vec<(i64, &'a NewRun)>> {
    let mut created = Vec::new();
    for PlannedRun { run, error } in planned {
        match error {
            None => {
                if let Some(id) = runs::create(tx, run).await? {
                    created.push((id, run));
                }
            }
            Some(error) => runs::create_failed(tx, run, error).await?,
        }
    }
    Ok(created)
}

fn log_created(created: &[(i64, &NewRun)]) {
    for (id, run) in created {
        tracing::info!(
            run = id,
            repo = run.repo,
            sha = run.sha,
            jobset = run.jobset,
            "queued run"
        );
    }
}
