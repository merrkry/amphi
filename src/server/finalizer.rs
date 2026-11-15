//! Acts once on each run that reached a final state: moves the pins of the
//! branches at its commit, tracks their failures in issues, and drops the GC
//! roots of its derivations. After each pass, releases the pins and closes
//! the issues that no branch wants any more.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use super::Server;
use super::evaluator::{gc_roots_base, gc_roots_dir};
use super::model::{Ref, RefKind, Run, RunState};
use super::{issues, pins, runs};

pub async fn run(server: Arc<Server>) {
    loop {
        let wake = server.wake.finalizer.notified();
        if let Err(error) = finalize_due(&server).await {
            tracing::warn!("finalizing runs: {error:#}");
        }
        if let Err(error) = pins::release_unwanted(&server).await {
            tracing::warn!("releasing pins: {error:#}");
        }
        if let Err(error) = issues::close_unwanted(&server).await {
            tracing::warn!("closing issues: {error:#}");
        }
        let _ = tokio::time::timeout(Duration::from_secs(60), wake).await;
    }
}

async fn finalize_due(server: &Server) -> anyhow::Result<()> {
    let due: Vec<Run> = sqlx::query_as(
        "SELECT * FROM runs WHERE finalized_seq < finalize_seq \
         AND state IN ('succeeded', 'failed', 'superseded') ORDER BY id",
    )
    .fetch_all(&server.db)
    .await?;
    for run in due {
        if let Err(error) = finalize(server, &run).await {
            tracing::warn!(run = run.id, "finalizing: {error:#}");
            continue;
        }
        // Requests made meanwhile, e.g. by a ref that started tracking pins,
        // leave the run due for another pass.
        sqlx::query("UPDATE runs SET finalized_seq = $2 WHERE id = $1")
            .bind(run.id)
            .bind(run.finalize_seq)
            .execute(&server.db)
            .await?;
        // A ref that returned to the commit during the pass found the run
        // waiting for it, so the reconciler left the run superseded.
        if run.state == RunState::Superseded {
            let revived = runs::revive(&mut *server.db.acquire().await?, &run.repo).await?;
            if revived > 0 {
                tracing::info!(
                    repo = run.repo,
                    revived,
                    "queued superseded runs whose commit is a head again"
                );
                server.wake.evaluator.notify_one();
                server.wake.reporter.notify_one();
            }
        }
    }
    Ok(())
}

async fn finalize(server: &Server, run: &Run) -> anyhow::Result<()> {
    if run.state != RunState::Superseded {
        // Branches that moved on wait for the run of their new head.
        for head in Ref::at(&server.db, run).await? {
            if head.kind != RefKind::Branch {
                continue;
            }
            // An evaluation that failed as a whole says nothing about the attributes.
            if head.pin && run.error.is_none() {
                pins::update(server, run, &head.name)
                    .await
                    .with_context(|| format!("updating the pins of {}", head.name))?;
            }
            if head.issues {
                issues::update(server, run, &head.name)
                    .await
                    .with_context(|| format!("updating the failure issue of {}", head.name))?;
            }
        }
    }
    let gc_roots = gc_roots_dir(server, run.id);
    match tokio::fs::remove_dir_all(&gc_roots).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", gc_roots.display()))
        }
        _ => Ok(()),
    }
}

/// Removes the GC roots of every run that is not underway. Run at startup,
/// before any evaluation, it drops the roots that a superseded run's
/// evaluation added after the finalizer's last pass, when a restart cut that
/// evaluation short.
pub async fn remove_stale_gc_roots(server: &Server) -> anyhow::Result<()> {
    let base = gc_roots_base(server);
    let mut entries = match tokio::fs::read_dir(&base).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        result => result.with_context(|| format!("reading {}", base.display()))?,
    };
    let underway: HashSet<String> = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM runs WHERE state IN ('queued', 'evaluating', 'building')",
    )
    .fetch_all(&server.db)
    .await?
    .into_iter()
    .map(|id| id.to_string())
    .collect();
    while let Some(entry) = entries.next_entry().await? {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| underway.contains(name))
        {
            continue;
        }
        let path = entry.path();
        tokio::fs::remove_dir_all(&path)
            .await
            .with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}
