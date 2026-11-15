//! Tracks the failures of a branch's jobset in a GitHub issue.
//!
//! The issue opens when a finished run of the branch fails, describes the
//! latest failures in its body, and gets a comment whenever the set of
//! failing attributes changes. It closes with a comment once a run passes,
//! or once the branch is no longer tracked or its head no longer has the
//! jobset. The next failure opens a new issue.

use std::collections::BTreeSet;
use std::fmt::Write;

use super::Server;
use super::config::RepoName;
use super::model::{AttrRow, Run};
use super::reconciler::CONFIG_JOBSET;
use super::summary::{self, run_url};

/// The pseudo-attribute that stands for an evaluation that failed as a whole.
const EVALUATION: &str = "(evaluation)";

pub async fn update(server: &Server, run: &Run, branch: &str) -> anyhow::Result<()> {
    let failing = failing_attrs(server, run).await?;
    let tracked: Option<(Option<i64>, Vec<String>)> = sqlx::query_as(
        "SELECT issue_number, failing FROM failure_issues \
         WHERE repo = $1 AND branch = $2 AND jobset = $3",
    )
    .bind(&run.repo)
    .bind(branch)
    .bind(&run.jobset)
    .fetch_optional(&server.db)
    .await?;
    if tracked.is_none() && failing.is_empty() {
        return Ok(());
    }

    let client = server
        .github
        .repo(&RepoName::try_from(run.repo.clone())?)
        .await?;
    let (number, previous) = match tracked {
        Some((Some(number), previous)) => (Some(number), Some(previous)),
        // A creation whose response was lost, e.g. by a timeout, may have
        // made the issue.
        Some((None, _)) => (
            client.find_open_issue(&marker(&run.jobset, branch)).await?,
            None,
        ),
        None => (None, None),
    };
    let url = run_url(&server.config.public_url, run);
    let failing_list: Vec<String> = failing.iter().cloned().collect();
    let record = |number: Option<i64>| {
        sqlx::query(
            "INSERT INTO failure_issues (repo, branch, jobset, issue_number, failing) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repo, branch, jobset) \
             DO UPDATE SET issue_number = $4, failing = $5",
        )
        .bind(&run.repo)
        .bind(branch)
        .bind(&run.jobset)
        .bind(number)
        .bind(&failing_list)
        .execute(&server.db)
    };
    let forget = || {
        sqlx::query("DELETE FROM failure_issues WHERE repo = $1 AND branch = $2 AND jobset = $3")
            .bind(&run.repo)
            .bind(branch)
            .bind(&run.jobset)
            .execute(&server.db)
    };
    match number {
        Some(number) if failing.is_empty() => {
            client
                .comment(number, &format!("Fixed in {} ([details]({url})).", run.sha))
                .await?;
            client.close_issue(number).await?;
            forget().await?;
            tracing::info!(run = run.id, issue = number, "closed failure issue");
        }
        None if failing.is_empty() => {
            forget().await?;
        }
        Some(number) => {
            client
                .edit_issue(number, &body(server, run, branch).await?)
                .await?;
            let previous = previous.map(BTreeSet::from_iter);
            if let Some(previous) = previous.filter(|previous| *previous != failing) {
                let comment = changes(run, &url, &previous, &failing);
                client.comment(number, &comment).await?;
            }
            record(Some(number)).await?;
        }
        None => {
            // Recorded first, so that the issue is looked up if the response
            // to its creation is lost.
            record(None).await?;
            let title = format!("{} fails to build {}", branch, run.jobset);
            let number = client
                .create_issue(&title, &body(server, run, branch).await?)
                .await?;
            record(Some(number)).await?;
            tracing::info!(run = run.id, issue = number, "opened failure issue");
        }
    }
    Ok(())
}

async fn failing_attrs(server: &Server, run: &Run) -> anyhow::Result<BTreeSet<String>> {
    if run.error.is_some() {
        return Ok(BTreeSet::from([EVALUATION.to_owned()]));
    }
    let attrs = AttrRow::of_run(&server.db, run.id).await?;
    Ok(attrs
        .into_iter()
        .filter(|attr| attr.result.is_some_and(|result| result.is_failure()))
        .map(|attr| attr.attr)
        .collect())
}

/// The latest failures, with logs.
async fn body(server: &Server, run: &Run, branch: &str) -> anyhow::Result<String> {
    let url = run_url(&server.config.public_url, run);
    let mut body = format!(
        "`{}` fails to build `{}` as of {} ([details]({url})).\n\n",
        branch, run.jobset, run.sha
    );
    match &run.error {
        Some(error) => {
            let _ = write!(
                body,
                "The evaluation failed:\n\n```\n{}\n```\n",
                crate::text::tail(error.trim(), 16 * 1024)
            );
        }
        None => {
            let attrs = AttrRow::of_run(&server.db, run.id).await?;
            let failed = summary::failed_builds(&server.db, run.id).await?;
            body.push_str(&summary::failures(&attrs, &failed));
        }
    }
    body.push_str(
        "\nAmphi updates this issue with each commit and closes it once the jobset builds again.\n",
    );
    body.push_str(&marker(&run.jobset, branch));
    Ok(body)
}

#[derive(sqlx::FromRow)]
struct Unwanted {
    repo: String,
    branch: String,
    jobset: String,
    issue_number: Option<i64>,
    /// The branch's head, if the branch is still tracked.
    head: Option<String>,
}

/// Closes the issues that no branch wants: those of branches that are no
/// longer tracked or no longer exist, and of jobsets the branch's head has
/// no run of, e.g. after a rename. A head whose `amphi.toml` is invalid says
/// nothing about its jobsets, so it keeps them all.
pub async fn close_unwanted(server: &Server) -> anyhow::Result<()> {
    let unwanted: Vec<Unwanted> = sqlx::query_as(
        "SELECT i.repo, i.branch, i.jobset, i.issue_number, f.sha AS head FROM failure_issues i \
         LEFT JOIN refs f ON f.repo = i.repo AND f.kind = 'branch' AND f.name = i.branch \
           AND f.issues \
         WHERE NOT EXISTS (SELECT 1 FROM runs r WHERE r.repo = i.repo AND r.sha = f.sha \
           AND r.jobset IN (i.jobset, $1))",
    )
    .bind(CONFIG_JOBSET)
    .fetch_all(&server.db)
    .await?;
    for Unwanted {
        repo,
        branch,
        jobset,
        issue_number,
        head,
    } in unwanted
    {
        let client = server
            .github
            .repo(&RepoName::try_from(repo.clone())?)
            .await?;
        let number = match issue_number {
            Some(number) => Some(number),
            None => client.find_open_issue(&marker(&jobset, &branch)).await?,
        };
        if let Some(number) = number {
            let comment = match head {
                None => format!("Amphi no longer tracks the failures of `{branch}`."),
                Some(sha) if jobset == CONFIG_JOBSET => format!("Fixed in {sha}."),
                Some(sha) => format!("{sha} no longer has the jobset `{jobset}`."),
            };
            client.comment(number, &comment).await?;
            client.close_issue(number).await?;
            tracing::info!(repo, issue = number, "closed failure issue");
        }
        sqlx::query("DELETE FROM failure_issues WHERE repo = $1 AND branch = $2 AND jobset = $3")
            .bind(&repo)
            .bind(&branch)
            .bind(&jobset)
            .execute(&server.db)
            .await?;
    }
    Ok(())
}

/// Identifies the issue of the branch and jobset in its body.
fn marker(jobset: &str, branch: &str) -> String {
    format!("<!-- amphi: {jobset} on {branch} -->\n")
}

fn changes(
    run: &Run,
    url: &str,
    previous: &BTreeSet<String>,
    failing: &BTreeSet<String>,
) -> String {
    // Keeps the comment within GitHub's limit when many attributes change.
    const MAX_LISTED: usize = 100;
    let list = |attrs: Vec<&String>| {
        let mut list = attrs
            .iter()
            .take(MAX_LISTED)
            .map(|attr| format!("`{attr}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if attrs.len() > MAX_LISTED {
            let _ = write!(list, " and {} more", attrs.len() - MAX_LISTED);
        }
        list
    };
    let mut comment = format!("At {} ([details]({url})):\n", run.short_sha());
    let broken: Vec<&String> = failing.difference(previous).collect();
    let fixed: Vec<&String> = previous.difference(failing).collect();
    if !broken.is_empty() {
        let _ = writeln!(comment, "- Now failing: {}", list(broken));
    }
    if !fixed.is_empty() {
        let _ = writeln!(comment, "- Fixed: {}", list(fixed));
    }
    comment
}
