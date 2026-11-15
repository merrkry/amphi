//! Publishes build outputs through one long-running `niks3 push --stdin`.
//!
//! niks3 reports each path once it is in the cache, which is what lets a
//! build count as done only when its outputs can be substituted. niks3
//! skips objects that are already uploaded, so the host's post-build-hook
//! uploading the same outputs costs little.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct Uploader {
    requests: mpsc::Sender<Request>,
}

struct Request {
    paths: Vec<String>,
    done: oneshot::Sender<Result<(), String>>,
}

#[derive(Serialize)]
struct StreamRequest<'a> {
    id: u64,
    paths: &'a [String],
}

#[derive(Deserialize)]
struct StreamResult {
    id: u64,
    path: String,
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    skipped: String,
}

struct Pending {
    remaining: HashSet<String>,
    errors: Vec<String>,
    done: oneshot::Sender<Result<(), String>>,
}

impl Uploader {
    /// `args` are passed to `niks3 push --stdin`, e.g. the server URL and credentials.
    pub fn start(args: Vec<String>) -> Self {
        Self::with_pusher(move || spawn_niks3(&args))
    }

    /// Uploads through the pushers `spawn` starts, one at a time.
    fn with_pusher(spawn: impl FnMut() -> anyhow::Result<Pusher> + Send + 'static) -> Self {
        let (requests, receiver) = mpsc::channel(64);
        tokio::spawn(run(spawn, receiver));
        Self { requests }
    }

    pub async fn upload(&self, paths: Vec<String>) -> Result<(), String> {
        let (done, result) = oneshot::channel();
        self.requests
            .send(Request { paths, done })
            .await
            .map_err(|_| "the uploader stopped".to_owned())?;
        result
            .await
            .map_err(|_| "the uploader stopped".to_owned())?
    }
}

/// A running `niks3 push --stdin`.
struct Pusher {
    /// Waited for once its output ends.
    child: Option<Child>,
    /// Lines for niks3's stdin. A task of its own writes them: niks3 stops
    /// reading while its results go unread, so waiting for it to read must
    /// not keep this loop from reading them.
    lines: mpsc::UnboundedSender<String>,
    results: tokio::io::Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>,
}

impl Pusher {
    fn new(
        stdin: impl AsyncWrite + Send + Unpin + 'static,
        stdout: impl AsyncRead + Send + Unpin + 'static,
        child: Option<Child>,
    ) -> Self {
        let (lines, queued) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(stdin, queued));
        let stdout: Box<dyn AsyncRead + Send + Unpin> = Box::new(stdout);
        Self {
            child,
            lines,
            results: BufReader::new(stdout).lines(),
        }
    }
}

fn spawn_niks3(args: &[String]) -> anyhow::Result<Pusher> {
    let mut child = Command::new("niks3")
        .args(["push", "--stdin"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting niks3 push")?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    Ok(Pusher::new(stdin, stdout, Some(child)))
}

/// Closes niks3's stdin when a write fails, so that niks3 exits once it has
/// reported the requests it read.
async fn write_lines(
    mut stdin: impl AsyncWrite + Unpin,
    mut lines: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        if let Err(error) = stdin.write_all(line.as_bytes()).await {
            tracing::warn!("writing to niks3: {error}");
            return;
        }
    }
}

async fn run(
    mut spawn: impl FnMut() -> anyhow::Result<Pusher>,
    mut requests: mpsc::Receiver<Request>,
) {
    let mut pusher: Option<Pusher> = None;
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut next_id = 1;
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return };
                if pusher.is_none() {
                    match spawn() {
                        Ok(started) => pusher = Some(started),
                        Err(error) => {
                            let _ = request.done.send(Err(format!("{error:#}")));
                            continue;
                        }
                    }
                }
                let id = next_id;
                next_id += 1;
                let mut line = serde_json::to_string(&StreamRequest { id, paths: &request.paths })
                    .expect("serializable");
                line.push('\n');
                if pusher.as_ref().expect("started").lines.send(line).is_err() {
                    let _ = request.done.send(Err("niks3 stopped reading".to_owned()));
                    continue;
                }
                pending.insert(id, Pending {
                    remaining: request.paths.into_iter().collect(),
                    errors: Vec::new(),
                    done: request.done,
                });
            }
            line = async { pusher.as_mut().expect("guarded").results.next_line().await }, if pusher.is_some() => {
                match line {
                    Ok(Some(line)) => handle_result(&line, &mut pending),
                    Ok(None) | Err(_) => {
                        if let Some(mut child) = pusher.take().expect("guarded").child {
                            let status = child.wait().await;
                            tracing::warn!("niks3 push exited: {status:?}");
                        }
                        for (_, request) in pending.drain() {
                            let _ = request.done.send(Err("niks3 push exited".to_owned()));
                        }
                    }
                }
            }
        }
    }
}

fn handle_result(line: &str, pending: &mut HashMap<u64, Pending>) {
    let result: StreamResult = match serde_json::from_str(line) {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!("unexpected niks3 output {line:?}: {error}");
            return;
        }
    };
    let Some(request) = pending.get_mut(&result.id) else {
        return;
    };
    request.remaining.remove(&result.path);
    if result.status != "ok" {
        request
            .errors
            .push(format!("{}: {}", result.path, result.message));
    } else if !result.skipped.is_empty() {
        request.errors.push(format!(
            "{} was not cached ({}): {}",
            result.path, result.skipped, result.message
        ));
    }
    if request.remaining.is_empty() {
        let request = pending.remove(&result.id).expect("present");
        let outcome = if request.errors.is_empty() {
            Ok(())
        } else {
            Err(request.errors.join("\n"))
        };
        let _ = request.done.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::DuplexStream;

    use super::*;

    /// Answers each request with a result per path, in the order niks3
    /// reports them. The pipes hold little, like a pipe to a process, so
    /// niks3 stops reading while its results go unread.
    fn fake_niks3(answer: fn(&str) -> serde_json::Value) -> Pusher {
        let (stdin, mut niks3_stdin) = tokio::io::duplex(256);
        let (mut niks3_stdout, stdout) = tokio::io::duplex(256);
        tokio::spawn(async move {
            let mut requests = BufReader::new(&mut niks3_stdin).lines();
            while let Ok(Some(line)) = requests.next_line().await {
                let request: serde_json::Value = serde_json::from_str(&line).expect("JSON");
                for path in request["paths"].as_array().expect("paths") {
                    let path = path.as_str().expect("a path");
                    let mut result = answer(path);
                    result["id"] = request["id"].clone();
                    result["path"] = path.into();
                    let mut line = result.to_string();
                    line.push('\n');
                    if niks3_stdout.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
        });
        Pusher::new(stdin, stdout, None)
    }

    fn paths(upload: usize) -> Vec<String> {
        (0..4)
            .map(|output| format!("/nix/store/{upload:032}-output-{output}"))
            .collect()
    }

    #[tokio::test]
    async fn many_uploads_at_once_finish() {
        let uploader =
            Uploader::with_pusher(|| Ok(fake_niks3(|_| serde_json::json!({ "status": "ok" }))));
        let mut uploads = tokio::task::JoinSet::new();
        for upload in 0..2000 {
            let uploader = uploader.clone();
            uploads.spawn(async move { uploader.upload(paths(upload)).await });
        }
        let results = tokio::time::timeout(Duration::from_secs(30), uploads.join_all())
            .await
            .expect("uploads finish");
        assert!(results.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn paths_that_are_not_cached_fail_the_upload() {
        let uploader = Uploader::with_pusher(|| {
            Ok(fake_niks3(|path| {
                if path.ends_with("output-1") {
                    serde_json::json!({ "status": "error", "message": "S3 is down" })
                } else if path.ends_with("output-2") {
                    serde_json::json!({ "status": "ok", "skipped": "too large", "message": "1 GiB" })
                } else {
                    serde_json::json!({ "status": "ok" })
                }
            }))
        });
        let error = uploader.upload(paths(0)).await.expect_err("fails");
        assert!(error.contains("output-1: S3 is down"), "{error}");
        assert!(
            error.contains("output-2 was not cached (too large)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn uploads_continue_after_niks3_exits() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let uploader = Uploader::with_pusher({
            let spawned = spawned.clone();
            move || {
                if spawned.fetch_add(1, Ordering::Relaxed) == 0 {
                    // Exits before answering.
                    let (stdin, _) = tokio::io::duplex(256);
                    let (_, stdout): (DuplexStream, DuplexStream) = tokio::io::duplex(256);
                    return Ok(Pusher::new(stdin, stdout, None));
                }
                Ok(fake_niks3(|_| serde_json::json!({ "status": "ok" })))
            }
        });
        assert_eq!(
            uploader.upload(paths(0)).await,
            Err("niks3 push exited".to_owned())
        );
        assert_eq!(uploader.upload(paths(1)).await, Ok(()));
        assert_eq!(spawned.load(Ordering::Relaxed), 2);
    }
}
