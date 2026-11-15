//! Markdown descriptions of run results for checks and issues.

use sqlx::PgPool;

use super::model::{AttrRow, AttrStatus, Run};
use crate::text::tail;

/// Log lines shown per failed build.
const LOG_TAIL_BYTES: usize = 4 * 1024;
/// Failed builds shown with their logs.
const MAX_LOGS: usize = 10;
/// GitHub rejects check summaries and issue bodies over 65,536 characters.
/// This leaves room for the text around the failures.
const MAX_FAILURES_BYTES: usize = 60 * 1024;
const TRUNCATED: &str = "\nThe run's page lists the remaining failures.\n";

pub struct FailedBuild {
    pub drv_path: String,
    pub error: Option<String>,
    pub log_tail: Option<String>,
}

pub async fn failed_builds(db: &PgPool, run_id: i64) -> sqlx::Result<Vec<FailedBuild>> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT b.drv_path, b.error, b.log_tail FROM run_build_states b \
         WHERE b.run_id = $1 AND b.state = 'failed' ORDER BY b.finished_at",
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(drv_path, error, log_tail)| FailedBuild {
            drv_path,
            error,
            log_tail,
        })
        .collect())
}

/// The run on the web UI's page of its commit.
pub fn run_url(public_url: &str, run: &Run) -> String {
    format!(
        "{}/{}/commit/{}#{}",
        public_url.trim_end_matches('/'),
        run.repo,
        run.sha,
        run.jobset
    )
}

/// Failing attributes as a list, then the failed builds with their logs, in
/// at most [`MAX_FAILURES_BYTES`].
pub fn failures(attrs: &[AttrRow], builds: &[FailedBuild]) -> String {
    let failing: Vec<&AttrRow> = attrs
        .iter()
        .filter(|attr| matches!(attr.status(), AttrStatus::Done(result) if result.is_failure()))
        .collect();
    let mut sections = Vec::new();
    if !failing.is_empty() {
        sections.push("| Attribute | Result |\n| --- | --- |\n".to_owned());
        for attr in &failing {
            sections.push(format!(
                "| `{}` | {} |\n",
                attr.attr,
                attr.status().as_str()
            ));
        }
        sections.push("\n".to_owned());
    }
    for attr in failing.iter().filter(|attr| attr.error.is_some()) {
        let error = attr.error.as_deref().unwrap_or_default();
        sections.push(format!(
            "<details><summary><code>{}</code> failed to evaluate</summary>\n\n```\n{}\n```\n</details>\n\n",
            attr.attr,
            tail(error.trim(), LOG_TAIL_BYTES)
        ));
    }
    for build in builds.iter().take(MAX_LOGS) {
        let log = build
            .log_tail
            .as_deref()
            .or(build.error.as_deref())
            .unwrap_or_default();
        sections.push(format!(
            "<details><summary><code>{}</code></summary>\n\n```\n{}\n```\n</details>\n\n",
            build.drv_path,
            tail(log.trim(), LOG_TAIL_BYTES)
        ));
    }
    if builds.len() > MAX_LOGS {
        sections.push(format!("{} more builds failed.\n", builds.len() - MAX_LOGS));
    }

    let mut text = String::new();
    for section in sections {
        if text.len() + section.len() > MAX_FAILURES_BYTES - TRUNCATED.len() {
            text.push_str(TRUNCATED);
            break;
        }
        text.push_str(&section);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::model::AttrResult;

    #[test]
    fn many_failures_fit_github() {
        let attrs: Vec<AttrRow> = (0..5000)
            .map(|i| AttrRow {
                attr: format!("packages.x86_64-linux.package-{i}"),
                drv_path: None,
                outputs: None,
                error: Some("error: ".repeat(1000)),
                result: Some(AttrResult::EvalError),
                build_state: None,
            })
            .collect();
        let text = failures(&attrs, &[]);
        assert!(text.len() <= MAX_FAILURES_BYTES);
        assert!(text.ends_with(TRUNCATED));
    }
}
