# Amphisbaena

Amphisbaena (Amphi) builds the flake outputs of GitHub repositories on a
small fleet of machines and publishes them to niks3. It has two components:

- `amphi-server` watches repositories, evaluates their flakes, schedules
  builds, reports to GitHub, and keeps niks3 pins.
- `amphi-worker` runs on every build machine. It takes builds from the
  server, builds them with the local Nix daemon, and pushes the outputs to
  niks3.

Workers build only for the systems they declare. There is no cross-building.

## Repository configuration

A repository opts in with `amphi.toml` at its root:

```toml
# Branches built on every push, as globs. `*` does not cross `/`.
branches = ["master"]
# Build the heads of open pull requests from branches of this repository.
pull-requests = true
# Branches whose latest successful outputs stay pinned in niks3.
pin = ["master"]
# Branches whose failures open a GitHub issue.
issues = ["master"]

[[jobsets]]
name = "x86_64-linux"
# Every derivation below this flake attribute is a job.
attr = "buildJobs.x86_64-linux"
```

`branches`, `pull-requests`, `pin` and `issues` are read from the default
branch, like the triggers of scheduled GitHub workflows. `jobsets` are read
from the commit being built, so a pull request can change what it builds. A
commit with an invalid `amphi.toml` gets a failed `amphi.toml` run. While the
file on the default branch is invalid, the watched refs stay as they were.

Pull requests from forks are never built: their code would run on the
builders and push to the cache.

## Flow

1. The server polls each configured repository every few minutes. GitHub
   webhooks trigger a poll right away. A run evaluates and builds one
   jobset at one commit. The head commit of each watched ref gets a run per
   jobset, which every ref pointing at that commit shares.
2. Runs are evaluated one at a time with
   `nix-eval-jobs --force-recurse --check-cache-status`. Derivations whose
   outputs are in no binary cache become builds, with edges between them
   read from the `.drv` files. GC roots keep the derivations alive until the
   run finishes.
3. A run is needed while a watched ref points at its commit. When the ref
   moves on, the run keeps going until the runs of the new commit are
   evaluated, even if its jobsets are named differently, since the two
   usually share most builds, and is superseded then. A queued run is superseded right away. Builds of a superseded run
   keep running only while another run, of any repository or ref, still
   needs them. Workers abort the rest on their next heartbeat. A ref that
   moves back to the commit of a superseded run queues that run again.
4. Workers claim one derivation per free slot. A remote worker only gets
   builds of runs whose derivations the server has copied to its store, in
   one batch per run.
5. The worker builds `drv^*` with remote builders disabled and reports the
   build as built. Evaluation found the outputs in no cache, so when every
   input is already in the worker's store, the build skips substituters,
   whose lookups would otherwise dominate small builds. The slot is free
   again, and dependents can start on the same worker, whose store has the
   outputs. The worker pushes the outputs
   through `niks3 push --stdin` meanwhile, and reports the build as uploaded
   once niks3 has confirmed every output. Only then is the build done, and
   other workers can substitute the outputs for its dependents.
6. When all builds of a run are done, the run succeeds or fails. The server
   then updates the pins and failure issues of the branches whose head the
   commit still is, and drops the GC roots.

### Builds

A build is a derivation, shared by every run that needs it. Each run waits
for its builds and for any unfinished dependency of them that an earlier run
recorded. When an evaluation finds the outputs of a succeeded build gone
from the cache and queues it again, runs waiting for its dependents wait
for it too.

| Result     | Cause                                                                                                  |
| ---------- | ------------------------------------------------------------------------------------------------------ |
| uploading  | Built. The worker is pushing the outputs.                                                              |
| succeeded  | Built and pushed.                                                                                      |
| failed     | `nix build` failed after the worker confirmed the derivation was present.                              |
| dep_failed | A dependency failed.                                                                                   |
| requeued   | The derivation was missing, the upload failed, or the worker stopped responding. Counts as an attempt. |

Determinate Nix exits with status 1 for any failure, so Amphi tells build
failures from infrastructure failures by checking what it can before the
build, such as whether the derivation is in the store. After `max-attempts`
transient failures, a build fails.

Every new run retries the failed builds it needs. A run that ended keeps its
builds as they were then, with their errors and logs, so retries by later
runs leave its record alone. "Run again" in the web UI, and "Re-run" on a
GitHub check, evaluate a finished run once more. Only runs of commits that a
watched ref points at can run again, so pins never go backwards.

### Scheduling

Builds are shared across the whole server: a derivation is built once, no
matter how many repositories, refs and runs need it.

Scheduling must never make the fleet slower than Nix on one machine, so
slots come first and load second:

- Each worker has `max-jobs` slots, the number of CPUs by default, and asks
  for a build whenever a slot is free. A free slot gets any ready build
  right away. Nix's `cores = 0` lets a lone large build use the whole
  machine.
- Load only decides between workers that wait at the same time, by the
  load of each worker's latest claim or heartbeat. A worker is busy while it
  has less than one idle core, measured over the last 5 seconds after
  counting each build started in that window as one core, or less than
  `min-free-memory`. A busy worker leaves a ready build to an idle
  waiting worker that can take one for a moment before taking it itself.
- Ready builds are ordered by the priority of the runs that need them,
  where branches come before pull requests, then by whether the worker
  built their dependencies, then by the longest chain of builds waiting for
  them.

Claims are long polls, so a worker receives a build as soon as one becomes
ready.

### Restarts

The server keeps its state in Postgres and workers keep none, so either
can restart at any time, e.g. for a deployment.

- A restarted server evaluates again the runs it was evaluating. Before
  that, it drops the GC roots of runs no longer underway, which the
  evaluation of a superseded run may have added after the finalizer's
  pass. Workers keep building while it is away and deliver their reports
  once it is back. Remote workers get builds again once the server has copied the
  derivations of building runs to them again, which skips those already
  there.
- A worker that starts tells the server first. The server puts the builds
  of its previous run back in the queue, since their processes are gone,
  and hands back those it had built but not finished uploading. The worker
  uploads them again from its store. The start first ends the claim the
  server may still hold from the previous run, so that it takes none of
  those builds.
- A build that a worker does not report having, e.g. because the server
  restarted before answering its claim, or a claim of the worker's previous
  run took it, goes back in the queue on the worker's next heartbeat.

None of this counts as an attempt. Only a worker that stops responding for
`worker-timeout` costs its builds an attempt, since the build may have
brought the machine down.

### Uploads and the post-build hook

The server's host should run niks3's post-build-hook, which publishes builds
made there outside Amphi, such as deployments. Evaluation counts a
dependency whose outputs are in the server's store as available, even if no
cache has them, so without the hook a remote worker that needs one builds it
itself, outside Amphi's scheduling, and fails if it lacks a required system
feature. With the hook, this is left to dependencies whose upload is still
queued when a remote worker needs them, e.g. ones built moments before the
evaluation, or while niks3 is unreachable. Other hosts may run the hook too,
for ordinary development builds.
Where niks3's `niks3-auto-upload` NixOS module is imported, the Amphi module
enables it on the server's host by default, with the server URL from the
`niks3` settings; the host provides the upload token.

The hook queues uploads asynchronously and cannot tell when a path is
cached, so the worker pushes its outputs itself. niks3 deduplicates by
object: whichever push comes second finds the objects present and skips
them. Builds published by the hook are also why evaluation often finds
development builds already cached.

## GitHub

With a GitHub App, every run has two checks on its commit:

- `eval: <jobset>` finishes once the jobset is evaluated. It is quick and
  suitable as a required check.
- `build: <jobset>` follows the builds, with progress and the logs of failed
  builds.

A commit that is the head of both a branch and a pull request, e.g. after a
fast-forward merge, has one run and one set of checks.

Required checks only gate the head commit of a pull request, so only the
newest commit of a ref matters. When a newer commit supersedes a run, its
unfinished checks complete as skipped, titled with the newer commit. GitHub
counts skipped checks as passing, so a series of quick pushes leaves no red
commits behind. A failed evaluation fails both checks, so that requiring
either catches it. GitHub has no API to delete check runs. Each check
carries the run and its execution as its external ID, so that retrying a
creation whose response was lost finds the check instead of making another.

Without the app, a token in `github.token-file` lets Amphi poll and build
repositories, but not report checks.

### GitHub App

- Webhook URL: `<public URL>/api/github/webhook`, with a secret.
- Repository permissions: checks read and write, contents read, issues read
  and write, metadata read, pull requests read.
- Events: check run, check suite, create, delete, pull request, push.

## Failure issues

For branches in `issues`, a finished run of the branch's head that fails
opens a GitHub issue for the branch and jobset. The issue body shows the
latest failures with their logs, and a comment records each change in the
failing attributes. A run that passes closes the issue with a comment, and
the next failure opens a new one. The issue also closes once the branch
leaves `issues` or is deleted, or once the branch's head has no run of the
jobset, e.g. after a rename or without `amphi.toml`; a head whose
`amphi.toml` is invalid leaves the issues open. Other branches and pull
requests rely on their checks. A hidden marker in the body identifies the
issue. The server records an issue before creating it, so that after a
creation whose response was lost, the next run of the branch looks the issue
up by its marker and updates or closes it, instead of opening another or
leaving it open.

## Pins

For branches in `pin`, each output of each attribute that succeeded has a
niks3 pin named `amphi.<repo>.<branch>.<jobset>.<attr>.<output>.<hash>`. The
next finished run of the branch's head moves the pin to its own output,
which releases the old one. A failing attribute keeps its last good output
pinned. Pins of removed attributes, of jobsets the branch's head has no run
of, and of branches deleted or no longer in `pin` are deleted; a head whose
`amphi.toml` is invalid keeps them. Outputs that niks3 does not track, e.g.
because they come from cache.nixos.org, are not pinned.

The server records each pin in its database before asking niks3 for it, and
treats its path as unknown until niks3 answers, so that a pin whose response
was lost is set again or released like any other. Only the finalizer changes
pins. Deleting pins needs a niks3 token with the admin scope.

## Web UI and API

The server serves the web UI from [`web`](web)
and the JSON API under `/api`. The UI has a page per commit at the same path
as on GitHub, `<public URL>/<owner>/<repo>/commit/<sha>`. GitHub users listed in `web.users` log in with GitHub OAuth, through
the GitHub App's client or a separate OAuth app; its callback URL is
`<public URL>/api/auth/callback`. Scripts and agents send a GitHub token
instead:

```console
$ curl -H "Authorization: Bearer $(gh auth token)" <public URL>/api/runs?active=true
```

Commits are addressed like in GitHub's API, and `<sha>` may be a unique
prefix, such as the short hash.

| Endpoint                                                                          | Content                                                                                                                                 |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /api/runs`                                                                   | Recent runs with their refs and build counts. Filters: `active`, `repo`, `ref`, `sha` prefix, `jobset`, `limit`, `before` a run's `id`. |
| `GET /api/repos/<owner>/<repo>/commits/<sha>`                                     | The commit's runs, their attributes, and where their time went.                                                                         |
| `GET /api/repos/<owner>/<repo>/commits/<sha>/runs/<jobset>/builds`                | A run's builds with their timestamps. Filter: `state`.                                                                                  |
| `GET /api/repos/<owner>/<repo>/commits/<sha>/runs/<jobset>/builds/<drv name>/log` | The end of the build's log as the run saw it, as text.                                                                                  |
| `POST /api/repos/<owner>/<repo>/commits/<sha>/runs/<jobset>/rerun`                | Evaluates a finished run again.                                                                                                         |
| `GET /api/builds/<drv name>`                                                      | A build, its dependencies and runs.                                                                                                     |
| `GET /api/builds/<drv name>/log`                                                  | The end of its log, as text.                                                                                                            |
| `GET /api/workers`                                                                | Workers with their slots, load and builds.                                                                                              |

`/api/github/webhook` and the worker API authenticate on their own.

## Metrics and logs

With `metrics-listen`, the server serves `/metrics` for Prometheus on a
separate address. The metrics show whether Amphi itself slows builds down,
and which part fails when something does:

| Metric                                                                    | Question                                                                   |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| `amphi_build_wait_seconds`                                                | How long do ready builds wait for a slot?                                  |
| `amphi_ready_builds`, `amphi_worker_slots`, `amphi_worker_running_builds` | Are builds ready while slots are free?                                     |
| `amphi_claim_query_seconds`                                               | How long does picking a build take?                                        |
| `amphi_stage_seconds`                                                     | How long do derivations take to reach a remote worker?                     |
| `amphi_build_seconds`, `amphi_upload_seconds`                             | How long do builds and uploads take, per worker and source of inputs?      |
| `amphi_build_attempts_total`                                              | How often do builds fail for reasons other than their derivation, and why? |
| `amphi_builds_released_total`                                             | How often do restarts and superseded runs send builds back to the queue?   |
| `amphi_evaluation_seconds`                                                | How long does evaluation take, and how often does it fail, per jobset?     |
| `amphi_github_requests_total`, `amphi_github_request_seconds`             | Is GitHub failing or slow?                                                 |
| `amphi_builds`                                                            | How many builds of active runs are in each state?                          |

Host load is left to the hosts' own metrics.

Both programs log to stderr. Under systemd, every line has its level as its
journal priority. The server logs every build that fails or is retried,
and every run that finishes.

## Nix

The flake provides the packages `amphi` and `amphi-web`, an overlay that adds
both to nixpkgs, and a NixOS module with `services.amphi.server` and
`services.amphi.worker`. The module's packages default to `pkgs.amphi` and
`pkgs.amphi-web`, so a configuration that imports `nixosModules.default`
also applies `overlays.default`:

```nix
# flake.nix
inputs.amphi = {
  url = "github:merrkry/amphi";
  inputs.nixpkgs.follows = "nixpkgs";
};

# A NixOS configuration
{ inputs, ... }:
{
  imports = [ inputs.amphi.nixosModules.default ];
  nixpkgs.overlays = [ inputs.amphi.overlays.default ];
}
```

## Configuration

Both programs take `--config <file>`. See `src/server/config.rs` and
`src/worker.rs` for every setting. The NixOS module writes the files.

Worker tokens are shared secrets between the server and each worker. All
workers are trusted alike, since each uploads to the cache: a token
identifies a worker for its claims and heartbeats, and a report names its
build by lease, whichever worker sends it.
`web.session-key-file` holds a random secret that signs session cookies. A
worker on another machine also needs a `store-uri` on the server, e.g.
`ssh-ng://nix-ssh@cobalt?ssh-key=...`, as a trusted user of that store.

## Tests

- `mise run check` runs the static checks and the unit tests. The unit
  tests skip the server's tests, which need Postgres.
- `mise run test` runs every test, the server's on a temporary Postgres
  server. They drive the server through the worker API and fakes of
  GitHub, niks3 and Nix, and expect it to act on each event within
  seconds, so that a missed wakeup fails them instead of waiting for a
  timer. Building the package runs them as well.
- `mise run test:vm` deploys Amphi in VMs to check the NixOS module,
  nix-eval-jobs, `nix build` and `nix copy` to a remote worker, with fakes
  of GitHub and niks3. It takes a few minutes.
