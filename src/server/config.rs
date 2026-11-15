use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Serves `/metrics` for Prometheus, kept off the public listener.
    pub metrics_listen: Option<SocketAddr>,
    pub database_url: String,
    /// Holds the GC roots of evaluated derivations.
    pub state_dir: PathBuf,
    /// Base URL of the web UI, linked from GitHub checks and issues.
    pub public_url: String,
    #[serde(default)]
    pub eval: EvalConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    pub github: GithubConfig,
    /// Repositories as `owner/name`, spelled as on GitHub.
    pub repos: Vec<RepoName>,
    pub niks3: Option<Niks3Config>,
    /// The web UI and its API. Without it, they are disabled.
    pub web: Option<WebConfig>,
    pub workers: Vec<WorkerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct EvalConfig {
    /// `nix-eval-jobs --workers`.
    pub workers: u32,
    /// `nix-eval-jobs --max-memory-size`, in MiB.
    pub max_memory_size: u32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            max_memory_size: 4096,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SchedulerConfig {
    /// A worker with less available memory counts as busy, in MiB.
    pub min_free_memory: u64,
    /// A worker that misses heartbeats for this many seconds loses its builds.
    pub worker_timeout: u64,
    /// Transient failures of a build before it counts as failed.
    pub max_attempts: i32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            min_free_memory: 2048,
            worker_timeout: 120,
            max_attempts: 3,
        }
    }
}

impl SchedulerConfig {
    pub fn worker_timeout(&self) -> Duration {
        Duration::from_secs(self.worker_timeout)
    }

    pub fn min_free_memory_bytes(&self) -> u64 {
        self.min_free_memory * 1024 * 1024
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct GithubConfig {
    #[serde(default = "default_github_api_url")]
    pub api_url: String,
    /// Where users log in, for the web UI.
    #[serde(default = "default_github_url")]
    pub url: String,
    /// Enables webhooks, checks and access to every repository the app is installed on.
    pub app: Option<GithubAppConfig>,
    /// A token for repositories without the app, e.g. a fine-grained PAT.
    pub token_file: Option<PathBuf>,
    /// Seconds between polls of the watched refs. Webhooks trigger a poll right away.
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
}

fn default_github_api_url() -> String {
    "https://api.github.com".to_owned()
}

fn default_github_url() -> String {
    "https://github.com".to_owned()
}

fn default_poll_interval() -> u64 {
    300
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct GithubAppConfig {
    pub id: u64,
    pub private_key_file: PathBuf,
    pub webhook_secret_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Niks3Config {
    /// The niks3 API, used to manage pins.
    pub server_url: String,
    /// The binary cache, linked for build logs.
    pub cache_url: String,
    /// A token with the admin scope, which deleting pins requires.
    pub token_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WebConfig {
    /// The built frontend, served for every path outside the API.
    pub assets: Option<PathBuf>,
    /// The OAuth client users log in with: the GitHub App's, or an OAuth app's.
    pub client_id: String,
    pub client_secret_file: PathBuf,
    /// Signs session cookies. Any long random string.
    pub session_key_file: PathBuf,
    /// GitHub logins allowed to use the web UI and its API.
    pub users: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WorkerConfig {
    pub name: String,
    pub token_file: PathBuf,
    /// Where `nix copy` sends derivations before the worker builds them.
    /// Unset for a worker that shares the server's Nix store.
    pub store_uri: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl TryFrom<String> for RepoName {
    type Error = anyhow::Error;

    fn try_from(value: String) -> anyhow::Result<Self> {
        let (owner, name) = value
            .split_once('/')
            .with_context(|| format!("repository {value:?} is not of the form owner/name"))?;
        ensure!(
            !owner.is_empty() && !name.is_empty() && !name.contains('/'),
            "repository {value:?} is not of the form owner/name"
        );
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        ensure!(
            config.github.app.is_some() || config.github.token_file.is_some(),
            "github needs an app or a token-file"
        );
        Ok(config)
    }
}

pub fn read_secret(path: &Path) -> anyhow::Result<String> {
    let secret = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret {}", path.display()))?;
    Ok(secret.trim().to_owned())
}
