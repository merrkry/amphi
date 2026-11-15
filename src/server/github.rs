//! GitHub access through a GitHub App installation or a token.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use hmac::{KeyInit, Mac};
use octocrab::Octocrab;
use octocrab::models::AppId;
use octocrab::service::middleware::retry::RetryConfig;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::config::{GithubConfig, RepoName, read_secret};
use super::metrics::GithubMetrics;
use crate::text::head;

/// Without a timeout, a request to an unresponsive GitHub would stall its
/// caller, e.g. polling, without any error.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The callers retry failed requests, e.g. on the next poll, so each request
/// is one attempt, and the metrics and logs see every failure of GitHub.
const RETRY: RetryConfig = RetryConfig::None;

/// How long a repository's installation is used before it is looked up again,
/// so that installing or removing the app takes effect. Installation tokens
/// last an hour.
const INSTALLATION_RECHECK: Duration = Duration::from_secs(5 * 60);

pub struct Github {
    app: Option<App>,
    token: Option<(Octocrab, String)>,
    /// The app's installation on each repository, if any, and when it was
    /// looked up.
    installations: Mutex<HashMap<RepoName, (Option<Installation>, Instant)>>,
    metrics: GithubMetrics,
}

/// A failed request to the GitHub API.
#[derive(Debug, thiserror::Error)]
#[error("GitHub API request {operation} failed ({result})")]
pub struct ApiError {
    operation: &'static str,
    result: String,
    #[source]
    source: Box<octocrab::Error>,
}

impl ApiError {
    fn is_not_found(&self) -> bool {
        matches!(&*self.source, octocrab::Error::GitHub { source, .. } if source.status_code == 404)
    }
}

/// Sends a request to the GitHub API and records it in the metrics, so that
/// failures of GitHub stand out from failures of Amphi.
async fn request<T>(
    metrics: &GithubMetrics,
    operation: &'static str,
    request: impl Future<Output = octocrab::Result<T>>,
) -> Result<T, ApiError> {
    let started = Instant::now();
    let response = request.await;
    let result = match &response {
        Ok(_) => "ok".to_owned(),
        Err(octocrab::Error::GitHub { source, .. }) => source.status_code.as_u16().to_string(),
        Err(
            octocrab::Error::Service { .. }
            | octocrab::Error::Hyper { .. }
            | octocrab::Error::Http { .. },
        ) => "transport".to_owned(),
        Err(_) => "other".to_owned(),
    };
    metrics.request(operation, result.clone(), started.elapsed());
    response.map_err(|source| ApiError {
        operation,
        result,
        source: Box::new(source),
    })
}

struct App {
    client: Octocrab,
    webhook_secret: String,
}

/// A client acting as the app's installation on a repository, with the
/// token it uses. Minting the token here, rather than leaving it to
/// octocrab, keeps one token per lookup and counts its request.
#[derive(Clone)]
struct Installation {
    client: Octocrab,
    token: String,
}

impl Github {
    pub fn new(config: &GithubConfig, metrics: GithubMetrics) -> anyhow::Result<Self> {
        let app = match &config.app {
            Some(app) => {
                let key = read_secret(&app.private_key_file)?;
                let client = Octocrab::builder()
                    .base_uri(&config.api_url)?
                    .set_connect_timeout(Some(CONNECT_TIMEOUT))
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .add_retry_config(RETRY)
                    .app(
                        AppId(app.id),
                        octocrab::jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())
                            .context("invalid GitHub App private key")?,
                    )
                    .build()?;
                Some(App {
                    client,
                    webhook_secret: read_secret(&app.webhook_secret_file)?,
                })
            }
            None => None,
        };
        let token = match &config.token_file {
            Some(path) => {
                let token = read_secret(path)?;
                let client = Octocrab::builder()
                    .base_uri(&config.api_url)?
                    .set_connect_timeout(Some(CONNECT_TIMEOUT))
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .add_retry_config(RETRY)
                    .personal_token(token.clone())
                    .build()?;
                Some((client, token))
            }
            None => None,
        };
        Ok(Self {
            app,
            token,
            installations: Mutex::default(),
            metrics,
        })
    }

    /// A client for the repository: the app's installation if the app is
    /// installed there, the token otherwise.
    pub async fn repo(&self, repo: &RepoName) -> anyhow::Result<RepoClient> {
        if let Some(installation) = self.installation(repo).await? {
            return Ok(RepoClient {
                client: installation.client,
                repo: repo.clone(),
                token: installation.token,
                app: true,
                metrics: self.metrics.clone(),
            });
        }
        match &self.token {
            Some((client, token)) => Ok(RepoClient {
                client: client.clone(),
                repo: repo.clone(),
                token: token.clone(),
                app: false,
                metrics: self.metrics.clone(),
            }),
            None => bail!("the GitHub App is not installed on {repo} and no token is configured"),
        }
    }

    async fn installation(&self, repo: &RepoName) -> anyhow::Result<Option<Installation>> {
        let Some(app) = &self.app else {
            return Ok(None);
        };
        let mut installations = self.installations.lock().await;
        if let Some((installation, looked_up)) = installations.get(repo)
            && looked_up.elapsed() < INSTALLATION_RECHECK
        {
            return Ok(installation.clone());
        }
        #[derive(Deserialize)]
        struct Found {
            id: u64,
        }
        #[derive(Deserialize)]
        struct AccessToken {
            token: String,
        }
        let route = format!("/repos/{}/{}/installation", repo.owner, repo.name);
        let lookup = app.client.get::<Found, _, ()>(route, None);
        let installation = match request(&self.metrics, "installation", lookup).await {
            Ok(found) => {
                let route = format!("/app/installations/{}/access_tokens", found.id);
                let mint = app.client.post::<(), AccessToken>(route, None);
                let token = request(&self.metrics, "installation_token", mint)
                    .await?
                    .token;
                Some(Installation {
                    client: app.client.user_access_token(token.clone())?,
                    token,
                })
            }
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(error.into()),
        };
        installations.insert(repo.clone(), (installation.clone(), Instant::now()));
        Ok(installation)
    }

    /// Checks the `X-Hub-Signature-256` header of a webhook delivery.
    pub fn verify_webhook(&self, signature: &str, body: &[u8]) -> bool {
        let Some(app) = &self.app else {
            return false;
        };
        let Some(Ok(signature)) = signature.strip_prefix("sha256=").map(hex::decode) else {
            return false;
        };
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(app.webhook_secret.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(body);
        mac.verify_slice(&signature).is_ok()
    }
}

pub struct RepoClient {
    client: Octocrab,
    repo: RepoName,
    /// The installation's token or the configured one.
    token: String,
    /// Whether the client acts as the GitHub App's installation.
    app: bool,
    metrics: GithubMetrics,
}

#[derive(Deserialize)]
pub struct Repository {
    /// The name as GitHub spells it. The API takes any letter case, but
    /// webhooks and pull requests carry this spelling.
    pub full_name: String,
    pub default_branch: String,
}

#[derive(Debug)]
pub struct Pull {
    pub number: u64,
    pub head_sha: String,
    /// Whether the head branch is in this repository rather than a fork.
    pub same_repo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Queued,
    InProgress,
    Success,
    Failure,
    Skipped,
}

#[derive(Debug)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub title: String,
    pub summary: String,
    pub details_url: String,
}

/// GitHub rejects check summaries longer than this.
const MAX_SUMMARY: usize = 65_535;

impl Check {
    fn body(&self) -> serde_json::Value {
        let (status, conclusion) = match self.status {
            CheckStatus::Queued => ("queued", None),
            CheckStatus::InProgress => ("in_progress", None),
            CheckStatus::Success => ("completed", Some("success")),
            CheckStatus::Failure => ("completed", Some("failure")),
            CheckStatus::Skipped => ("completed", Some("skipped")),
        };
        let mut body = json!({
            "name": self.name,
            "status": status,
            "details_url": self.details_url,
            "output": {
                "title": self.title,
                "summary": head(&self.summary, MAX_SUMMARY),
            },
        });
        if let Some(conclusion) = conclusion {
            body["conclusion"] = conclusion.into();
        }
        body
    }
}

impl RepoClient {
    /// Whether the client acts as the GitHub App, which checks require.
    pub fn is_app(&self) -> bool {
        self.app
    }

    /// A token that lets Nix fetch the repository.
    pub fn token(&self) -> &str {
        &self.token
    }

    fn route(&self, path: &str) -> String {
        format!("/repos/{}/{}{path}", self.repo.owner, self.repo.name)
    }

    async fn request<T>(
        &self,
        operation: &'static str,
        response: impl Future<Output = octocrab::Result<T>>,
    ) -> Result<T, ApiError> {
        request(&self.metrics, operation, response).await
    }

    pub async fn repository(&self) -> anyhow::Result<Repository> {
        Ok(self
            .request("repository", self.client.get(self.route(""), None::<&()>))
            .await?)
    }

    /// Every branch with its head commit.
    pub async fn branches(&self) -> anyhow::Result<Vec<(String, String)>> {
        #[derive(Deserialize)]
        struct Branch {
            name: String,
            commit: Commit,
        }
        #[derive(Deserialize)]
        struct Commit {
            sha: String,
        }
        let branches: Vec<Branch> = self
            .paginate("branches", &self.route("/branches"), &[])
            .await?;
        Ok(branches
            .into_iter()
            .map(|b| (b.name, b.commit.sha))
            .collect())
    }

    pub async fn open_pulls(&self) -> anyhow::Result<Vec<Pull>> {
        #[derive(Deserialize)]
        struct PullRequest {
            number: u64,
            head: Head,
        }
        #[derive(Deserialize)]
        struct Head {
            sha: String,
            repo: Option<HeadRepo>,
        }
        #[derive(Deserialize)]
        struct HeadRepo {
            full_name: String,
        }
        let pulls: Vec<PullRequest> = self
            .paginate("pulls", &self.route("/pulls"), &[("state", "open")])
            .await?;
        let full_name = self.repo.to_string();
        Ok(pulls
            .into_iter()
            .map(|pull| Pull {
                number: pull.number,
                same_repo: pull
                    .head
                    .repo
                    .is_some_and(|repo| repo.full_name == full_name),
                head_sha: pull.head.sha,
            })
            .collect())
    }

    async fn paginate<T: serde::de::DeserializeOwned>(
        &self,
        operation: &'static str,
        route: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<Vec<T>> {
        const PER_PAGE: usize = 100;
        let mut items = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let per_page = PER_PAGE.to_string();
            let mut parameters: Vec<(&str, &str)> = query.to_vec();
            parameters.push(("per_page", &per_page));
            parameters.push(("page", &page));
            let batch: Vec<T> = self
                .request(operation, self.client.get(route, Some(&parameters)))
                .await?;
            let last = batch.len() < PER_PAGE;
            items.extend(batch);
            if last {
                break;
            }
        }
        Ok(items)
    }

    /// The file at the commit, or `None` if it does not exist.
    pub async fn file(&self, path: &str, sha: &str) -> anyhow::Result<Option<String>> {
        #[derive(Deserialize)]
        struct Content {
            content: String,
        }
        let route = self.route(&format!("/contents/{path}"));
        let response = self
            .request("file", self.client.get(route, Some(&[("ref", sha)])))
            .await;
        let content: Content = match response {
            Ok(content) => content,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.content.replace('\n', ""))
            .context("decoding file contents")?;
        Ok(Some(String::from_utf8(bytes).context("file is not UTF-8")?))
    }

    /// The check of the commit with the check's name and `external_id`.
    pub async fn find_check(
        &self,
        sha: &str,
        check: &Check,
        external_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        #[derive(Deserialize)]
        struct CheckRuns {
            check_runs: Vec<CheckRun>,
        }
        #[derive(Deserialize)]
        struct CheckRun {
            id: i64,
            external_id: Option<String>,
        }
        let route = self.route(&format!("/commits/{sha}/check-runs"));
        let query = [
            ("check_name", check.name.as_str()),
            ("filter", "all"),
            ("per_page", "100"),
        ];
        let response = self.client.get(route, Some(&query));
        let found: CheckRuns = self.request("find_check", response).await?;
        Ok(found
            .check_runs
            .into_iter()
            .find(|run| run.external_id.as_deref() == Some(external_id))
            .map(|run| run.id))
    }

    pub async fn create_check(
        &self,
        sha: &str,
        external_id: &str,
        check: &Check,
    ) -> anyhow::Result<i64> {
        #[derive(Deserialize)]
        struct Created {
            id: i64,
        }
        let mut body = check.body();
        body["head_sha"] = sha.into();
        body["external_id"] = external_id.into();
        let response = self.client.post(self.route("/check-runs"), Some(&body));
        let created: Created = self.request("create_check", response).await?;
        Ok(created.id)
    }

    pub async fn update_check(&self, id: i64, check: &Check) -> anyhow::Result<()> {
        let route = self.route(&format!("/check-runs/{id}"));
        let body = check.body();
        let _: serde_json::Value = self
            .request("update_check", self.client.patch(route, Some(&body)))
            .await?;
        Ok(())
    }

    /// The newest open issue whose body contains `marker`, among the 100
    /// issues and pull requests opened last. The finalizer looks for an issue
    /// whose creation response was lost on its next pass, within a minute,
    /// so that issue is among them.
    pub async fn find_open_issue(&self, marker: &str) -> anyhow::Result<Option<i64>> {
        #[derive(Deserialize)]
        struct Issue {
            number: i64,
            body: Option<String>,
        }
        let query = [
            ("state", "open"),
            ("sort", "created"),
            ("direction", "desc"),
            ("per_page", "100"),
        ];
        let response = self.client.get(self.route("/issues"), Some(&query));
        let issues: Vec<Issue> = self.request("issues", response).await?;
        Ok(issues
            .into_iter()
            .find(|issue| {
                issue
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains(marker))
            })
            .map(|issue| issue.number))
    }

    pub async fn create_issue(&self, title: &str, body: &str) -> anyhow::Result<i64> {
        #[derive(Deserialize)]
        struct Created {
            number: i64,
        }
        let body = json!({ "title": title, "body": body });
        let response = self.client.post(self.route("/issues"), Some(&body));
        let created: Created = self.request("create_issue", response).await?;
        Ok(created.number)
    }

    pub async fn edit_issue(&self, number: i64, body: &str) -> anyhow::Result<()> {
        self.patch_issue(number, &json!({ "body": body })).await
    }

    pub async fn close_issue(&self, number: i64) -> anyhow::Result<()> {
        self.patch_issue(
            number,
            &json!({ "state": "closed", "state_reason": "completed" }),
        )
        .await
    }

    async fn patch_issue(&self, number: i64, body: &serde_json::Value) -> anyhow::Result<()> {
        let response = self
            .client
            .patch(self.route(&format!("/issues/{number}")), Some(body));
        let _: serde_json::Value = self.request("update_issue", response).await?;
        Ok(())
    }

    pub async fn comment(&self, number: i64, body: &str) -> anyhow::Result<()> {
        let route = self.route(&format!("/issues/{number}/comments"));
        let body = json!({ "body": body });
        let _: serde_json::Value = self
            .request("comment", self.client.post(route, Some(&body)))
            .await?;
        Ok(())
    }
}
