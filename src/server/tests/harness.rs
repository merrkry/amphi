//! A server on a test database, with fakes for GitHub, niks3, Nix and the
//! workers.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use base64::Engine;
use hmac::{KeyInit, Mac};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use super::fake_github::{self, GithubState, Repo, Shared};
use crate::api::{
    self, BuildEvent, ClaimResponse, HeartbeatResponse, Inputs, Job, Load, Report, StartResponse,
    Transient, WorkerStatus,
};
use crate::server::config::Config;
use crate::server::evaluator::set_heights;
use crate::server::model::{BuildState, Run, RunState};
use crate::server::runs::{EvalOutput, EvaluatedAttr, NewBuild};
use crate::server::{Nix, Server, http};

/// How soon the server must act on an event. Well below the intervals at
/// which its loops look for work anyway, so that a missed wakeup fails.
pub const PROMPT: Duration = Duration::from_secs(5);

pub const REPO: &str = "owner/repo";
pub const OTHER_REPO: &str = "owner/other";
pub const JOBSET: &str = "x86_64-linux";
const WEBHOOK_SECRET: &str = "webhook-secret";

/// `amphi.toml` of every commit unless a test sets another.
pub const REPO_CONFIG: &str = r#"
branches = ["*"]
pull-requests = true
pin = ["master"]
issues = ["master"]

[[jobsets]]
name = "x86_64-linux"
attr = "jobs"
"#;

fn hash(text: &str) -> String {
    hex::encode(&Sha256::digest(text.as_bytes())[..16])
}

/// The derivation of a fake build.
pub fn drv(name: &str) -> String {
    format!("/nix/store/{}-{name}.drv", hash(name))
}

/// The output of a fake build.
pub fn out(name: &str) -> String {
    format!("/nix/store/{}-{name}", hash(&format!("out:{name}")))
}

/// The name of a fake build from its derivation.
pub fn name(drv_path: &str) -> &str {
    let (_, name) = drv_path.split_once('-').expect("a hash");
    name.trim_end_matches(".drv")
}

/// What a fake evaluation of a jobset finds.
#[derive(Clone, Default)]
pub struct Eval {
    attrs: Vec<EvalAttr>,
    builds: Vec<EvalBuild>,
}

#[derive(Clone)]
struct EvalAttr {
    attr: String,
    drv: Option<String>,
    error: Option<String>,
}

#[derive(Clone)]
struct EvalBuild {
    name: String,
    deps: Vec<String>,
}

impl Eval {
    /// An attribute whose derivation is to build, after the builds it depends on.
    pub fn job(self, name: &str, deps: &[&str]) -> Self {
        self.attr(name, name).build(name, deps)
    }

    /// A derivation to build that no attribute is, only a dependency.
    pub fn build(mut self, name: &str, deps: &[&str]) -> Self {
        self.builds.push(EvalBuild {
            name: name.to_owned(),
            deps: deps.iter().map(|dep| (*dep).to_owned()).collect(),
        });
        self
    }

    /// An attribute whose derivation `name` is a build, or else cached.
    pub fn attr(mut self, attr: &str, name: &str) -> Self {
        self.attrs.push(EvalAttr {
            attr: attr.to_owned(),
            drv: Some(name.to_owned()),
            error: None,
        });
        self
    }

    pub fn eval_error(mut self, attr: &str, error: &str) -> Self {
        self.attrs.push(EvalAttr {
            attr: attr.to_owned(),
            drv: None,
            error: Some(error.to_owned()),
        });
        self
    }

    fn output(&self) -> EvalOutput {
        let attrs = self
            .attrs
            .iter()
            .map(|attr| EvaluatedAttr {
                attr: attr.attr.clone(),
                drv_path: attr.drv.as_deref().map(drv),
                outputs: attr
                    .drv
                    .as_deref()
                    .map(|name| BTreeMap::from([("out".to_owned(), Some(out(name)))])),
                error: attr.error.clone(),
            })
            .collect();
        let mut builds: Vec<NewBuild> = self
            .builds
            .iter()
            .map(|build| NewBuild {
                drv_path: drv(&build.name),
                system: JOBSET.to_owned(),
                required_features: Vec::new(),
                deps: build.deps.iter().map(|dep| drv(dep)).collect(),
                height: 1,
            })
            .collect();
        set_heights(&mut builds);
        EvalOutput { attrs, builds }
    }
}

/// Evaluates commits to what the test set for them, and records copies to
/// remote workers.
#[derive(Default)]
pub struct FakeNix {
    evaluations: Mutex<HashMap<String, Evaluation>>,
    copied: Mutex<HashMap<String, Vec<String>>>,
}

#[derive(Clone)]
struct Evaluation {
    result: Result<Eval, String>,
    /// Holds the evaluation until the test adds a permit.
    hold: Option<Arc<Semaphore>>,
}

impl FakeNix {
    pub fn set(&self, sha: &str, result: Result<Eval, String>) {
        self.evaluations
            .lock()
            .expect("not poisoned")
            .insert(sha.to_owned(), Evaluation { result, hold: None });
    }

    /// Makes evaluations of the commit wait for a permit of the semaphore.
    pub fn hold(&self, sha: &str) -> Arc<Semaphore> {
        let semaphore = Arc::new(Semaphore::new(0));
        self.evaluations
            .lock()
            .expect("not poisoned")
            .get_mut(sha)
            .expect("commit has an evaluation")
            .hold = Some(semaphore.clone());
        semaphore
    }

    /// Derivations copied to the remote worker so far.
    pub fn copied(&self, worker: &str) -> Vec<String> {
        self.copied
            .lock()
            .expect("not poisoned")
            .get(worker)
            .cloned()
            .unwrap_or_default()
    }

    pub(in crate::server) async fn evaluate(
        &self,
        run: &Run,
        gc_roots: &Path,
    ) -> anyhow::Result<EvalOutput> {
        let evaluation = self
            .evaluations
            .lock()
            .expect("not poisoned")
            .get(&run.sha)
            .cloned();
        let Some(evaluation) = evaluation else {
            anyhow::bail!("no evaluation set for {}", run.sha);
        };
        if let Some(hold) = evaluation.hold {
            hold.acquire().await?.forget();
        }
        // Stands for the roots nix-eval-jobs adds while it evaluates, which
        // recreates the directory if it is gone.
        std::fs::create_dir_all(gc_roots)?;
        std::fs::write(gc_roots.join("root"), "")?;
        evaluation
            .result
            .map(|eval| eval.output())
            .map_err(anyhow::Error::msg)
    }

    pub(in crate::server) async fn copy(
        &self,
        worker: &str,
        drv_paths: &[String],
    ) -> anyhow::Result<()> {
        self.copied
            .lock()
            .expect("not poisoned")
            .entry(worker.to_owned())
            .or_default()
            .extend_from_slice(drv_paths);
        Ok(())
    }
}

/// Waits until the check holds, polling it, and fails after [`PROMPT`].
pub async fn until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + PROMPT;
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A PEM private key for the fake GitHub App, made once per test process.
fn app_private_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
        let key = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)
            .expect("generating a key");
        let der: Pkcs8V1Der = key.as_der().expect("encoding the key");
        let base64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = base64
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).expect("base64"))
            .collect();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        )
    })
}

/// A build as the database has it.
#[derive(Debug, sqlx::FromRow)]
pub struct BuildRow {
    pub state: BuildState,
    pub attempts: i32,
    pub worker: Option<String>,
}

pub struct Harness {
    pub db: PgPool,
    pub nix: Arc<FakeNix>,
    pub server: Arc<Server>,
    github: Shared,
    github_url: String,
    dir: tempfile::TempDir,
    address: SocketAddr,
    listener: Option<tokio::net::TcpListener>,
    tasks: JoinSet<()>,
}

impl Harness {
    /// Starts a server watching [`REPO`] and [`OTHER_REPO`], both with the
    /// GitHub App installed, on a fresh database. Its workers are `local-a`
    /// and `local-b`, which share its store, and `remote`.
    pub async fn new(db: PgPool) -> Self {
        let mut harness = Self::prepare(db).await;
        harness.start().await;
        harness
    }

    /// Sets up a server like [`Harness::new`], which [`Harness::start`]
    /// starts, e.g. once the test changed what GitHub shows at first.
    pub async fn prepare(db: PgPool) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let secrets = [
            ("app-key.pem", app_private_key()),
            ("webhook-secret", WEBHOOK_SECRET),
            ("github-token", "configured-token"),
            ("niks3-token", "niks3-token"),
            ("worker-local-a", "token-local-a"),
            ("worker-local-b", "token-local-b"),
            ("worker-remote", "token-remote"),
        ];
        for (name, secret) in secrets {
            std::fs::write(dir.path().join(name), secret).expect("writing a secret");
        }

        let github = Shared::default();
        for repo in [REPO, OTHER_REPO] {
            github.lock().expect("not poisoned").repos.insert(
                repo.to_owned(),
                Repo {
                    default_branch: "master".to_owned(),
                    installed: true,
                    ..Repo::default()
                },
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding");
        let github_url = format!("http://{}", listener.local_addr().expect("bound"));
        let router = axum::Router::new()
            .fallback(fake_github::serve)
            .with_state(github.clone());
        tokio::spawn(axum::serve(listener, router).into_future());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding");
        let address = listener.local_addr().expect("bound");
        let nix = Arc::new(FakeNix::default());
        let config = config(dir.path(), &github_url);
        let server = Server::new(config, db.clone(), Nix::Fake(nix.clone())).expect("server");
        Self {
            db,
            nix,
            server,
            github,
            github_url,
            dir,
            address,
            listener: Some(listener),
            tasks: JoinSet::new(),
        }
    }

    pub async fn start(&mut self) {
        let listener = self.listener.take().expect("not started");
        self.launch(listener).await;
    }

    async fn launch(&mut self, listener: tokio::net::TcpListener) {
        self.tasks = self.server.start().await.expect("starting");
        let serve = axum::serve(listener, http::router(self.server.clone())).into_future();
        self.tasks.spawn(async move {
            serve.await.expect("serving");
        });
    }

    /// Stops the server and starts a new one on the same database and address.
    pub async fn restart(&mut self) {
        self.tasks.shutdown().await;
        let config = config(self.dir.path(), &self.github_url);
        self.server =
            Server::new(config, self.db.clone(), Nix::Fake(self.nix.clone())).expect("server");
        let socket = tokio::net::TcpSocket::new_v4().expect("socket");
        socket.set_reuseaddr(true).expect("reusable address");
        socket.bind(self.address).expect("binding");
        self.launch(socket.listen(64).expect("listening")).await;
    }

    pub fn github(&self) -> MutexGuard<'_, GithubState> {
        self.github.lock().expect("not poisoned")
    }

    pub fn worker(&self, name: &str) -> FakeWorker {
        FakeWorker::new(&format!("http://{}", self.address), name)
    }

    /// Adds a commit with the default `amphi.toml`, whose jobset evaluates to `eval`.
    pub fn commit(&self, repo: &str, sha: &str, eval: Eval) {
        self.commit_with(repo, sha, REPO_CONFIG, Ok(eval));
    }

    pub fn commit_with(
        &self,
        repo: &str,
        sha: &str,
        repo_config: &str,
        eval: Result<Eval, String>,
    ) {
        self.github().repo(repo).files.insert(
            (sha.to_owned(), "amphi.toml".to_owned()),
            repo_config.to_owned(),
        );
        self.nix.set(sha, eval);
    }

    /// Points the branch at the commit and tells the server through a webhook.
    pub async fn set_branch(&self, repo: &str, branch: &str, sha: &str) {
        self.github()
            .repo(repo)
            .branches
            .insert(branch.to_owned(), sha.to_owned());
        assert_eq!(
            self.webhook("push", json!({})).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Adds the commit and points the branch at it.
    pub async fn push(&self, repo: &str, branch: &str, sha: &str, eval: Eval) {
        self.commit(repo, sha, eval);
        self.set_branch(repo, branch, sha).await;
    }

    pub async fn webhook(&self, event: &str, payload: Value) -> StatusCode {
        let body = serde_json::to_vec(&payload).expect("serializable");
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes())
            .expect("any key length");
        mac.update(&body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        reqwest::Client::new()
            .post(format!("http://{}/api/github/webhook", self.address))
            .header("x-github-event", event)
            .header("x-hub-signature-256", signature)
            .body(body)
            .send()
            .await
            .expect("webhook delivered")
            .status()
    }

    /// The run of the commit's jobset.
    pub async fn run(&self, repo: &str, sha: &str) -> Option<Run> {
        sqlx::query_as("SELECT * FROM runs WHERE repo = $1 AND sha = $2 AND jobset = $3")
            .bind(repo)
            .bind(sha)
            .bind(JOBSET)
            .fetch_optional(&self.db)
            .await
            .expect("querying runs")
    }

    pub async fn until_run(&self, repo: &str, sha: &str, state: RunState) -> Run {
        until(&format!("the run of {sha} is {state:?}"), async || {
            self.run(repo, sha)
                .await
                .is_some_and(|run| run.state == state)
        })
        .await;
        self.run(repo, sha).await.expect("exists")
    }

    /// Waits until the run ended and GitHub and niks3 show it.
    pub async fn until_settled(&self, repo: &str, sha: &str) -> Run {
        let mut settled = None;
        until(&format!("the run of {sha} is settled"), async || {
            settled = sqlx::query_as(
                "SELECT * FROM runs WHERE repo = $1 AND sha = $2 AND jobset = $3 \
                 AND state IN ('succeeded', 'failed', 'superseded') \
                 AND reported_seq = report_seq AND finalized_seq = finalize_seq",
            )
            .bind(repo)
            .bind(sha)
            .bind(JOBSET)
            .fetch_optional(&self.db)
            .await
            .expect("querying runs");
            settled.is_some()
        })
        .await;
        settled.expect("settled")
    }

    pub async fn build(&self, name: &str) -> BuildRow {
        sqlx::query_as("SELECT state, attempts, worker FROM builds WHERE drv_path = $1")
            .bind(drv(name))
            .fetch_one(&self.db)
            .await
            .expect("querying builds")
    }

    /// The names of the builds the run waits for.
    pub async fn run_builds(&self, run_id: i64) -> Vec<String> {
        let mut names: Vec<String> =
            sqlx::query_scalar("SELECT drv_path FROM run_builds WHERE run_id = $1")
                .bind(run_id)
                .fetch_all(&self.db)
                .await
                .expect("querying run builds");
        for drv_path in &mut names {
            *drv_path = name(drv_path).to_owned();
        }
        names.sort();
        names
    }

    /// GitHub requests in Amphi's metrics, by operation and result.
    pub async fn github_metrics(&self) -> BTreeMap<(String, String), u64> {
        let text = self
            .server
            .metrics
            .render(&self.db)
            .await
            .expect("rendering metrics");
        let mut counts = BTreeMap::new();
        for line in text.lines() {
            let Some(labels) = line.strip_prefix("amphi_github_requests_total{") else {
                continue;
            };
            let (labels, count) = labels.split_once("} ").expect("a sample");
            let label = |name: &str| {
                let start = labels.find(&format!("{name}=\"")).expect("label") + name.len() + 2;
                labels[start..]
                    .split('"')
                    .next()
                    .expect("quoted")
                    .to_owned()
            };
            counts.insert(
                (label("operation"), label("result")),
                count.parse().expect("a count"),
            );
        }
        counts
    }
}

fn config(dir: &Path, github_url: &str) -> Config {
    let dir = dir.display();
    let text = format!(
        r#"
listen = "127.0.0.1:0"
database-url = "unused"
state-dir = "{dir}/state"
public-url = "https://amphi.test"
repos = ["{REPO}", "{OTHER_REPO}"]

[github]
api-url = "{github_url}"
token-file = "{dir}/github-token"
# Webhooks trigger every poll in the tests.
poll-interval = 3600

[github.app]
id = 1
private-key-file = "{dir}/app-key.pem"
webhook-secret-file = "{dir}/webhook-secret"

[niks3]
server-url = "{github_url}"
cache-url = "https://cache.test"
token-file = "{dir}/niks3-token"

[[workers]]
name = "local-a"
token-file = "{dir}/worker-local-a"

[[workers]]
name = "local-b"
token-file = "{dir}/worker-local-b"

[[workers]]
name = "remote"
token-file = "{dir}/worker-remote"
store-uri = "ssh-ng://remote"
"#
    );
    toml::from_str(&text).expect("valid configuration")
}

/// A worker that the test drives through the worker API.
#[derive(Clone)]
pub struct FakeWorker(Arc<WorkerInner>);

struct WorkerInner {
    url: String,
    token: String,
    http: reqwest::Client,
    systems: Mutex<Vec<String>>,
    busy: AtomicBool,
    max_jobs: u32,
    running: Mutex<Vec<Uuid>>,
    uploading: Mutex<Vec<Uuid>>,
}

impl FakeWorker {
    fn new(url: &str, name: &str) -> Self {
        Self(Arc::new(WorkerInner {
            url: url.to_owned(),
            token: format!("token-{name}"),
            // Every request opens a connection, so that none reaches a
            // server stopped by a restart.
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("client"),
            systems: Mutex::new(vec![JOBSET.to_owned()]),
            busy: AtomicBool::new(false),
            max_jobs: 4,
            running: Mutex::default(),
            uploading: Mutex::default(),
        }))
    }

    pub fn set_systems(&self, systems: &[&str]) {
        *self.0.systems.lock().expect("not poisoned") =
            systems.iter().map(|system| (*system).to_owned()).collect();
    }

    /// Reports no idle CPU from now on, so that it lets idle workers go first.
    pub fn set_busy(&self, busy: bool) {
        self.0.busy.store(busy, Ordering::Relaxed);
    }

    /// Drops the lease without telling the server, like a worker that never
    /// received the claim's response.
    pub fn forget(&self, lease: Uuid) {
        self.0
            .running
            .lock()
            .expect("not poisoned")
            .retain(|running| *running != lease);
    }

    pub fn running(&self) -> Vec<Uuid> {
        self.0.running.lock().expect("not poisoned").clone()
    }

    fn status(&self) -> WorkerStatus {
        let busy = self.0.busy.load(Ordering::Relaxed);
        WorkerStatus {
            systems: self.0.systems.lock().expect("not poisoned").clone(),
            features: Vec::new(),
            max_jobs: self.0.max_jobs,
            running: self.running(),
            uploading: self.0.uploading.lock().expect("not poisoned").clone(),
            load: Load {
                cpus: 8,
                idle_cores: if busy { 0.0 } else { 8.0 },
                window_secs: 5,
                mem_available_bytes: 64 << 30,
            },
        }
    }

    async fn post(&self, path: &str, body: &impl serde::Serialize) -> reqwest::Response {
        self.0
            .http
            .post(format!("{}{path}", self.0.url))
            .bearer_auth(&self.0.token)
            .json(body)
            .send()
            .await
            .expect("request delivered")
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> T {
        let response = self.post(path, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        response.json().await.expect("JSON response")
    }

    /// Tells the server the worker started, after its process restarted,
    /// and takes over the uploads it hands back.
    pub async fn start(&self) -> StartResponse {
        self.0.running.lock().expect("not poisoned").clear();
        self.0.uploading.lock().expect("not poisoned").clear();
        let response: StartResponse = self.post_json(api::START_PATH, &self.status()).await;
        self.0
            .uploading
            .lock()
            .expect("not poisoned")
            .extend(response.uploads.iter().map(|upload| upload.lease));
        response
    }

    /// Asks for a build, waiting in the background like a worker with a free slot.
    pub fn claim(&self) -> PendingClaim {
        let worker = self.clone();
        PendingClaim(tokio::spawn(async move {
            let response: ClaimResponse = worker.post_json(api::CLAIM_PATH, &worker.status()).await;
            if let ClaimResponse::Job(job) = &response {
                worker
                    .0
                    .running
                    .lock()
                    .expect("not poisoned")
                    .push(job.lease);
            }
            response
        }))
    }

    /// The build the server hands out next, within [`PROMPT`].
    pub async fn take(&self) -> Job {
        self.claim().job().await
    }

    /// Sends a heartbeat and stops the builds the server aborts.
    pub async fn heartbeat(&self) -> Vec<Uuid> {
        let response: HeartbeatResponse = self.post_json(api::HEARTBEAT_PATH, &self.status()).await;
        self.0
            .running
            .lock()
            .expect("not poisoned")
            .retain(|lease| !response.abort.contains(lease));
        response.abort
    }

    async fn report(&self, lease: Uuid, event: BuildEvent) -> StatusCode {
        self.post(api::REPORT_PATH, &Report { lease, event })
            .await
            .status()
    }

    /// Reports the build as built, which starts its upload.
    pub async fn built(&self, job: &Job) -> StatusCode {
        let outputs = BTreeMap::from([("out".to_owned(), out(name(&job.drv_path)))]);
        let status = self
            .report(
                job.lease,
                BuildEvent::Built {
                    outputs,
                    inputs: Inputs::Local,
                },
            )
            .await;
        self.0
            .running
            .lock()
            .expect("not poisoned")
            .retain(|lease| *lease != job.lease);
        if status.is_success() {
            self.0
                .uploading
                .lock()
                .expect("not poisoned")
                .push(job.lease);
        }
        status
    }

    pub async fn uploaded(&self, job: &Job) -> StatusCode {
        let status = self.report(job.lease, BuildEvent::Uploaded).await;
        self.0
            .uploading
            .lock()
            .expect("not poisoned")
            .retain(|lease| *lease != job.lease);
        status
    }

    /// Builds and uploads the build.
    pub async fn finish(&self, job: &Job) {
        assert_eq!(self.built(job).await, StatusCode::NO_CONTENT);
        assert_eq!(self.uploaded(job).await, StatusCode::NO_CONTENT);
    }

    pub async fn fail(&self, job: &Job, log: &str) -> StatusCode {
        let status = self
            .report(
                job.lease,
                BuildEvent::Failed {
                    error: "nix build failed with exit status: 1".to_owned(),
                    log_tail: log.to_owned(),
                    inputs: Inputs::Local,
                },
            )
            .await;
        self.0
            .running
            .lock()
            .expect("not poisoned")
            .retain(|lease| *lease != job.lease);
        status
    }

    pub async fn transient(&self, job: &Job, cause: Transient) -> StatusCode {
        let status = self
            .report(
                job.lease,
                BuildEvent::Transient {
                    cause,
                    error: "injected".to_owned(),
                },
            )
            .await;
        self.0
            .running
            .lock()
            .expect("not poisoned")
            .retain(|lease| *lease != job.lease);
        self.0
            .uploading
            .lock()
            .expect("not poisoned")
            .retain(|lease| *lease != job.lease);
        status
    }
}

/// A claim waiting for a build.
pub struct PendingClaim(JoinHandle<ClaimResponse>);

impl PendingClaim {
    /// The build the claim gets within [`PROMPT`].
    pub async fn job(mut self) -> Job {
        match tokio::time::timeout(PROMPT, &mut self.0).await {
            Ok(Ok(ClaimResponse::Job(job))) => job,
            Ok(Ok(ClaimResponse::Idle)) => panic!("the claim ended without a build"),
            Ok(Err(error)) => panic!("the claim failed: {error}"),
            Err(_) => panic!("no build within {PROMPT:?}"),
        }
    }

    /// Asserts that the claim ends without a build within [`PROMPT`].
    pub async fn assert_idle(mut self) {
        match tokio::time::timeout(PROMPT, &mut self.0).await {
            Ok(Ok(ClaimResponse::Idle)) => {}
            Ok(Ok(ClaimResponse::Job(job))) => panic!("the claim got {}", job.drv_path),
            Ok(Err(error)) => panic!("the claim failed: {error}"),
            Err(_) => panic!("the claim still waits after {PROMPT:?}"),
        }
    }

    /// Asserts that the claim is still waiting after a moment.
    pub async fn assert_waiting(&self) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!self.0.is_finished(), "the claim ended");
    }
}

impl Drop for PendingClaim {
    fn drop(&mut self) {
        self.0.abort();
    }
}
