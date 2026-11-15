//! An in-memory GitHub API, with the niks3 pin API on the side, that can
//! fail or delay chosen requests.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

/// GitHub rejects issue bodies and comments longer than this, in characters.
const MAX_BODY: usize = 65_536;
/// GitHub rejects check summaries longer than this, in characters.
const MAX_SUMMARY: usize = 65_535;

/// What happens to the next request of an operation.
pub enum Fault {
    /// Rejected with the status, without any effect.
    Fail(u16),
    /// Takes effect, but the response is a 502, like a response lost on the way.
    LoseResponse,
    /// Takes effect, and the response waits for a permit of the semaphore.
    Hold(Arc<Semaphore>),
}

#[derive(Default)]
pub struct Repo {
    pub default_branch: String,
    pub branches: BTreeMap<String, String>,
    /// Files by commit and path.
    pub files: HashMap<(String, String), String>,
    pub pulls: Vec<Pull>,
    /// Whether the GitHub App is installed on the repository.
    pub installed: bool,
    pub issues: Vec<Issue>,
}

#[derive(Clone)]
pub struct Pull {
    pub number: u64,
    pub head_sha: String,
    /// The repository of the head branch.
    pub head_repo: String,
}

#[derive(Clone, Debug)]
pub struct CheckRun {
    pub id: i64,
    pub repo: String,
    pub head_sha: String,
    pub name: String,
    pub external_id: Option<String>,
    pub status: String,
    pub conclusion: Option<String>,
    pub title: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub comments: Vec<String>,
}

#[derive(Default)]
pub struct GithubState {
    pub repos: BTreeMap<String, Repo>,
    pub checks: Vec<CheckRun>,
    /// niks3 pins by name.
    pub pins: BTreeMap<String, String>,
    /// Store paths niks3 does not have.
    pub uncached: Vec<String>,
    /// Operations as Amphi names them in its metrics, with the response status.
    pub requests: Vec<(String, u16)>,
    faults: HashMap<String, VecDeque<Fault>>,
    next_id: i64,
    tokens_minted: usize,
}

impl GithubState {
    pub fn repo(&mut self, name: &str) -> &mut Repo {
        let name = self.full_name(name).expect("repository exists");
        self.repos.get_mut(&name).expect("exists")
    }

    /// The repository's name as GitHub spells it, which matches any letter case.
    fn full_name(&self, name: &str) -> Option<String> {
        self.repos
            .keys()
            .find(|known| known.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Answers the next request of `operation`, e.g. `create_check`, with the fault.
    pub fn fault(&mut self, operation: &str, fault: Fault) {
        self.faults
            .entry(operation.to_owned())
            .or_default()
            .push_back(fault);
    }

    pub fn clear_faults(&mut self) {
        self.faults.clear();
    }

    /// The checks of the commit with the name, oldest first.
    pub fn checks(&self, sha: &str, name: &str) -> Vec<CheckRun> {
        self.checks
            .iter()
            .filter(|check| check.head_sha == sha && check.name == name)
            .cloned()
            .collect()
    }

    pub fn count(&self, operation: &str) -> usize {
        self.requests
            .iter()
            .filter(|(op, _)| op == operation)
            .count()
    }

    pub fn tokens_minted(&self) -> usize {
        self.tokens_minted
    }

    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }
}

pub type Shared = Arc<Mutex<GithubState>>;

/// The operation of a request, named like Amphi's metrics name it.
fn operation(method: &Method, path: &[&str]) -> &'static str {
    match (method.as_str(), path) {
        ("GET", ["repos", _, _]) => "repository",
        ("GET", ["repos", _, _, "branches"]) => "branches",
        ("GET", ["repos", _, _, "pulls"]) => "pulls",
        ("GET", ["repos", _, _, "contents", ..]) => "file",
        ("GET", ["repos", _, _, "installation"]) => "installation",
        ("POST", ["app", "installations", _, "access_tokens"]) => "installation_token",
        ("GET", ["repos", _, _, "commits", _, "check-runs"]) => "find_check",
        ("POST", ["repos", _, _, "check-runs"]) => "create_check",
        ("PATCH", ["repos", _, _, "check-runs", _]) => "update_check",
        ("GET", ["repos", _, _, "issues"]) => "issues",
        ("POST", ["repos", _, _, "issues"]) => "create_issue",
        ("PATCH", ["repos", _, _, "issues", _]) => "update_issue",
        ("POST", ["repos", _, _, "issues", _, "comments"]) => "comment",
        ("POST", ["api", "pins", _]) => "niks3_pin",
        ("DELETE", ["api", "pins", _]) => "niks3_unpin",
        _ => "unknown",
    }
}

pub async fn serve(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let path: Vec<&str> = uri.path().trim_matches('/').split('/').collect();
    let operation = operation(&method, &path);
    let body: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("JSON request body")
    };
    let fault = {
        let mut state = state.lock().expect("not poisoned");
        state
            .faults
            .get_mut(operation)
            .and_then(VecDeque::pop_front)
    };
    let (status, response) = match fault {
        Some(Fault::Fail(status)) => (
            StatusCode::from_u16(status).expect("valid status"),
            json!({ "message": "injected failure" }),
        ),
        fault => {
            let response = apply(
                &mut state.lock().expect("not poisoned"),
                &method,
                &path,
                &query,
                &body,
            );
            match fault {
                Some(Fault::LoseResponse) => (
                    StatusCode::BAD_GATEWAY,
                    json!({ "message": "response lost" }),
                ),
                Some(Fault::Hold(semaphore)) => {
                    semaphore.acquire().await.expect("open").forget();
                    response
                }
                _ => response,
            }
        }
    };
    // Other local processes may probe the port. A request of Amphi's that
    // the fake does not know still shows up in Amphi's metrics.
    if operation != "unknown" {
        state
            .lock()
            .expect("not poisoned")
            .requests
            .push((operation.to_owned(), status.as_u16()));
    }
    (status, Json(response)).into_response()
}

fn not_found() -> (StatusCode, Value) {
    (StatusCode::NOT_FOUND, json!({ "message": "Not Found" }))
}

fn too_long(field: &str) -> (StatusCode, Value) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "message": format!("{field} is too long") }),
    )
}

fn apply(
    state: &mut GithubState,
    method: &Method,
    path: &[&str],
    query: &HashMap<String, String>,
    body: &Value,
) -> (StatusCode, Value) {
    let ok = |value| (StatusCode::OK, value);
    let text = |field: &str| body[field].as_str().unwrap_or_default().to_owned();
    match (method.as_str(), path) {
        ("POST", ["app", "installations", _, "access_tokens"]) => {
            state.tokens_minted += 1;
            let token = format!("installation-token-{}", state.tokens_minted);
            (StatusCode::CREATED, json!({ "token": token }))
        }
        ("POST", ["api", "pins", name]) => {
            let store_path = text("store_path");
            if state.uncached.contains(&store_path) {
                return not_found();
            }
            state.pins.insert((*name).to_owned(), store_path);
            ok(json!({}))
        }
        ("DELETE", ["api", "pins", name]) => match state.pins.remove(*name) {
            Some(_) => (StatusCode::NO_CONTENT, Value::Null),
            None => not_found(),
        },
        (_, ["repos", owner, name, rest @ ..]) => {
            let Some(full_name) = state.full_name(&format!("{owner}/{name}")) else {
                return not_found();
            };
            repo_request(state, &full_name, method, rest, query, body)
        }
        _ => not_found(),
    }
}

fn repo_request(
    state: &mut GithubState,
    full_name: &str,
    method: &Method,
    path: &[&str],
    query: &HashMap<String, String>,
    body: &Value,
) -> (StatusCode, Value) {
    let ok = |value| (StatusCode::OK, value);
    let text = |field: &str| body[field].as_str().unwrap_or_default().to_owned();
    let summary = || {
        body["output"]["summary"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let first_page = query.get("page").is_none_or(|page| page == "1");
    let index = state
        .repos
        .keys()
        .position(|name| name == full_name)
        .expect("exists") as i64;
    let id = state.id();
    let repo = state.repos.get_mut(full_name).expect("exists");
    match (method.as_str(), path) {
        ("GET", []) => ok(json!({ "full_name": full_name, "default_branch": repo.default_branch })),
        ("GET", ["branches"]) => ok(if first_page {
            repo.branches
                .iter()
                .map(|(name, sha)| json!({ "name": name, "commit": { "sha": sha } }))
                .collect()
        } else {
            json!([])
        }),
        ("GET", ["pulls"]) => ok(if first_page {
            repo.pulls
                .iter()
                .map(|pull| {
                    json!({
                        "number": pull.number,
                        "head": { "sha": pull.head_sha, "repo": { "full_name": pull.head_repo } },
                    })
                })
                .collect()
        } else {
            json!([])
        }),
        ("GET", ["contents", file @ ..]) => {
            let sha = query.get("ref").cloned().unwrap_or_default();
            match repo.files.get(&(sha, file.join("/"))) {
                Some(content) => ok(json!({
                    "content": base64::engine::general_purpose::STANDARD.encode(content),
                })),
                None => not_found(),
            }
        }
        ("GET", ["installation"]) if repo.installed => ok(json!({ "id": index + 1 })),
        ("GET", ["installation"]) => not_found(),
        ("GET", ["issues"]) => ok(repo
            .issues
            .iter()
            .rev()
            .filter(|issue| issue.open)
            .take(100)
            .map(|issue| json!({ "number": issue.number, "body": issue.body }))
            .collect()),
        ("POST", ["issues"]) => {
            let body = text("body");
            if body.chars().count() > MAX_BODY {
                return too_long("body");
            }
            let number = repo.issues.len() as i64 + 1;
            repo.issues.push(Issue {
                number,
                title: text("title"),
                body,
                open: true,
                comments: Vec::new(),
            });
            (StatusCode::CREATED, json!({ "number": number }))
        }
        ("PATCH", ["issues", number]) => {
            let Some(issue) = repo
                .issues
                .iter_mut()
                .find(|issue| issue.number.to_string() == *number)
            else {
                return not_found();
            };
            if let Some(new_body) = body["body"].as_str() {
                if new_body.chars().count() > MAX_BODY {
                    return too_long("body");
                }
                issue.body = new_body.to_owned();
            }
            if body["state"] == "closed" {
                issue.open = false;
            }
            ok(json!({}))
        }
        ("POST", ["issues", number, "comments"]) => {
            let comment = text("body");
            if comment.chars().count() > MAX_BODY {
                return too_long("body");
            }
            let Some(issue) = repo
                .issues
                .iter_mut()
                .find(|issue| issue.number.to_string() == *number)
            else {
                return not_found();
            };
            issue.comments.push(comment);
            (StatusCode::CREATED, json!({}))
        }
        ("GET", ["commits", sha, "check-runs"]) => {
            let name = query.get("check_name");
            let runs: Vec<Value> = state
                .checks
                .iter()
                .filter(|check| {
                    check.repo == full_name
                        && check.head_sha == *sha
                        && name.is_none_or(|name| *name == check.name)
                })
                .map(|check| json!({ "id": check.id, "external_id": check.external_id }))
                .collect();
            ok(json!({ "total_count": runs.len(), "check_runs": runs }))
        }
        ("POST", ["check-runs"]) => {
            if summary().chars().count() > MAX_SUMMARY {
                return too_long("summary");
            }
            state.checks.push(CheckRun {
                id,
                repo: full_name.to_owned(),
                head_sha: text("head_sha"),
                name: text("name"),
                external_id: body["external_id"].as_str().map(str::to_owned),
                status: text("status"),
                conclusion: body["conclusion"].as_str().map(str::to_owned),
                title: body["output"]["title"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                summary: summary(),
            });
            (StatusCode::CREATED, json!({ "id": id }))
        }
        ("PATCH", ["check-runs", check_id]) => {
            if summary().chars().count() > MAX_SUMMARY {
                return too_long("summary");
            }
            let Some(check) = state
                .checks
                .iter_mut()
                .find(|check| check.id.to_string() == *check_id && check.repo == full_name)
            else {
                return not_found();
            };
            check.status = text("status");
            check.conclusion = body["conclusion"].as_str().map(str::to_owned);
            check.title = body["output"]["title"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            check.summary = summary();
            ok(json!({ "id": check.id }))
        }
        _ => not_found(),
    }
}
