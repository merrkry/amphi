//! Evaluates queued runs with nix-eval-jobs, one at a time.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::config::RepoName;
use super::model::Run;
use super::runs::{self, EvalOutput, EvaluatedAttr, NewBuild};
use super::{Nix, Server};
use crate::drv;
use crate::text::read_tail;

/// How long to wait before recording an evaluation again.
const RETRY_DELAY: Duration = Duration::from_secs(5);

pub async fn run(server: Arc<Server>) {
    loop {
        let wake = server.wake.evaluator.notified();
        match runs::start_next(&server.db).await {
            Ok(Some(run)) => {
                evaluate(&server, &run).await;
                if let Err(error) = runs::refinalize_if_superseded(&server.db, run.id).await {
                    tracing::error!(run = run.id, "requesting finalization: {error:#}");
                }
                server.wake.reporter.notify_one();
                server.wake.finalizer.notify_one();
                continue;
            }
            Ok(None) => {}
            Err(error) => tracing::error!("taking the next run: {error:#}"),
        }
        let _ = tokio::time::timeout(Duration::from_secs(60), wake).await;
    }
}

async fn evaluate(server: &Server, run: &Run) {
    tracing::info!(
        run = run.id,
        repo = run.repo,
        sha = run.sha,
        jobset = run.jobset,
        "evaluating"
    );
    let started = std::time::Instant::now();
    let output = match evaluate_flake(server, run).await {
        Ok(output) => output,
        Err(error) => {
            server.metrics.evaluated(run, "failed", started.elapsed());
            tracing::warn!(run = run.id, "evaluation failed: {error:#}");
            fail_evaluation(server, run, &format!("{error:#}")).await;
            return;
        }
    };
    match ingest(server, run, &output).await {
        Ok(true) => {
            server
                .metrics
                .evaluated(run, "succeeded", started.elapsed());
            tracing::info!(run = run.id, builds = output.builds.len(), "evaluated");
            server.scheduler.ready.notify_waiters();
        }
        Ok(false) => {
            server
                .metrics
                .evaluated(run, "superseded", started.elapsed());
            tracing::info!(run = run.id, "run was superseded while evaluating");
        }
        // The run fails rather than stays evaluating, which nothing would retry.
        Err(error) => {
            server.metrics.evaluated(run, "failed", started.elapsed());
            tracing::error!(run = run.id, "recording the evaluation: {error:#}");
            let error = format!("recording the evaluation: {error:#}");
            fail_evaluation(server, run, &error).await;
        }
    }
}

/// Records the evaluation's builds, retrying when Postgres aborted it to
/// resolve a deadlock, e.g. with a build report locking the same builds, and
/// while Postgres is unreachable.
async fn ingest(server: &Server, run: &Run, output: &EvalOutput) -> sqlx::Result<bool> {
    const DEADLOCK: &str = "40P01";
    let mut attempts = 1;
    loop {
        match runs::ingest(&server.db, run.id, output).await {
            Err(sqlx::Error::Database(error))
                if error.code().as_deref() == Some(DEADLOCK) && attempts < 3 =>
            {
                attempts += 1;
            }
            Err(error @ (sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut)) => {
                tracing::warn!(run = run.id, "recording the evaluation: {error:#}");
                tokio::time::sleep(RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

/// Retries until it is recorded, since nothing else takes a run out of
/// `evaluating` while the server runs.
async fn fail_evaluation(server: &Server, run: &Run, error: &str) {
    while let Err(recording) = runs::fail_evaluation(&server.db, run.id, error).await {
        tracing::error!(
            run = run.id,
            "recording the failed evaluation: {recording:#}"
        );
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// Holds a directory of GC roots per run, named by the run's ID.
pub fn gc_roots_base(server: &Server) -> PathBuf {
    server.config.state_dir.join("gcroots")
}

pub fn gc_roots_dir(server: &Server, run_id: i64) -> PathBuf {
    gc_roots_base(server).join(run_id.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobLine {
    attr: String,
    drv_path: Option<String>,
    outputs: Option<BTreeMap<String, Option<String>>>,
    error: Option<String>,
    cache_status: Option<String>,
    #[serde(default)]
    needed_builds: Vec<String>,
}

async fn evaluate_flake(server: &Server, run: &Run) -> anyhow::Result<EvalOutput> {
    let gc_roots = gc_roots_dir(server, run.id);
    tokio::fs::create_dir_all(&gc_roots)
        .await
        .with_context(|| format!("creating {}", gc_roots.display()))?;
    match &server.nix {
        Nix::Commands => nix_eval_jobs(server, run, &gc_roots).await,
        #[cfg(test)]
        Nix::Fake(nix) => nix.evaluate(run, &gc_roots).await,
    }
}

async fn nix_eval_jobs(server: &Server, run: &Run, gc_roots: &Path) -> anyhow::Result<EvalOutput> {
    let repo = RepoName::try_from(run.repo.clone())?;
    let token = server.github.repo(&repo).await?.token().to_owned();
    let eval_config = &server.config.eval;
    let mut child = Command::new("nix-eval-jobs")
        .arg("--flake")
        .arg(format!("{}#{}", run.flake_ref, run.attr))
        .args(["--force-recurse", "--check-cache-status"])
        .args(["--workers", &eval_config.workers.to_string()])
        .args([
            "--max-memory-size",
            &eval_config.max_memory_size.to_string(),
        ])
        .arg("--gc-roots-dir")
        .arg(gc_roots)
        .env("NIX_CONFIG", format!("access-tokens = github.com={token}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting nix-eval-jobs")?;

    let stderr = tokio::spawn(read_tail(child.stderr.take().expect("piped"), 16 * 1024));

    let mut attrs = Vec::new();
    let mut needed = BTreeSet::new();
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("reading nix-eval-jobs output")?
    {
        let job: JobLine = serde_json::from_str(&line)
            .with_context(|| format!("invalid nix-eval-jobs output: {line}"))?;
        needed.extend(job.needed_builds);
        // Outputs that exist only in the local store are in no cache yet.
        if job.cache_status.as_deref() == Some("local")
            && let Some(drv_path) = &job.drv_path
        {
            needed.insert(drv_path.clone());
        }
        attrs.push(EvaluatedAttr {
            attr: job.attr,
            drv_path: job.drv_path,
            outputs: job.outputs,
            error: job.error,
        });
    }
    let status = child.wait().await.context("waiting for nix-eval-jobs")?;
    let stderr = stderr.await.unwrap_or_default();
    if !status.success() {
        bail!("nix-eval-jobs failed with {status}:\n{}", stderr.trim());
    }

    let builds = tokio::task::spawn_blocking(move || read_builds(&needed)).await??;
    Ok(EvalOutput { attrs, builds })
}

fn read_builds(needed: &BTreeSet<String>) -> anyhow::Result<Vec<NewBuild>> {
    let needed_set: HashSet<&str> = needed.iter().map(String::as_str).collect();
    let mut builds = needed
        .iter()
        .map(|drv_path| {
            let text =
                std::fs::read_to_string(drv_path).with_context(|| format!("reading {drv_path}"))?;
            let parsed = drv::parse(&text).with_context(|| format!("parsing {drv_path}"))?;
            Ok(NewBuild {
                drv_path: drv_path.clone(),
                system: parsed.system,
                required_features: parsed.required_features,
                deps: parsed
                    .input_drvs
                    .into_iter()
                    .map(|(input, _)| input)
                    .filter(|input| needed_set.contains(input.as_str()))
                    .collect(),
                height: 1,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    set_heights(&mut builds);
    Ok(builds)
}

/// Sets each build's height: the longest chain of builds waiting for it,
/// itself included. Builds on long chains start first, since they bound how
/// soon the run can finish.
pub(super) fn set_heights(builds: &mut [NewBuild]) {
    let index: HashMap<&str, usize> = builds
        .iter()
        .enumerate()
        .map(|(i, build)| (build.drv_path.as_str(), i))
        .collect();
    let deps: Vec<Vec<usize>> = builds
        .iter()
        .map(|build| build.deps.iter().map(|dep| index[dep.as_str()]).collect())
        .collect();
    // Builds in an order where each comes before its dependencies.
    let mut dependents = vec![0usize; builds.len()];
    for dep in deps.iter().flatten() {
        dependents[*dep] += 1;
    }
    let mut order: Vec<usize> = (0..builds.len()).filter(|&i| dependents[i] == 0).collect();
    let mut next = 0;
    while let Some(&build) = order.get(next) {
        next += 1;
        for &dep in &deps[build] {
            dependents[dep] -= 1;
            if dependents[dep] == 0 {
                order.push(dep);
            }
        }
    }
    let mut heights = vec![1i32; builds.len()];
    for &build in &order {
        for &dep in &deps[build] {
            heights[dep] = heights[dep].max(heights[build] + 1);
        }
    }
    for (build, height) in builds.iter_mut().zip(heights) {
        build.height = height;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(drv_path: &str, deps: &[&str]) -> NewBuild {
        NewBuild {
            drv_path: drv_path.into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            deps: deps.iter().map(|dep| (*dep).into()).collect(),
            height: 1,
        }
    }

    #[test]
    fn heights_follow_the_longest_chain_of_dependents() {
        // a <- b <- c <- d, and a <- d directly.
        let mut builds = vec![
            build("d", &["c", "a"]),
            build("a", &[]),
            build("c", &["b"]),
            build("b", &["a"]),
            build("lone", &[]),
        ];
        set_heights(&mut builds);
        let heights: HashMap<&str, i32> = builds
            .iter()
            .map(|build| (build.drv_path.as_str(), build.height))
            .collect();
        assert_eq!(heights["d"], 1);
        assert_eq!(heights["c"], 2);
        assert_eq!(heights["b"], 3);
        assert_eq!(heights["a"], 4);
        assert_eq!(heights["lone"], 1);
    }
}
