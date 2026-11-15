CREATE TYPE ref_kind AS ENUM ('branch', 'pull');

CREATE TYPE run_state AS ENUM (
  'queued',
  'evaluating',
  'building',
  'succeeded',
  'failed',
  'superseded'
);

CREATE TYPE build_state AS ENUM (
  'queued',
  'running',
  -- Built on its worker, which is uploading the outputs.
  'uploading',
  'succeeded',
  'failed',
  'dep_failed'
);

CREATE TYPE attr_result AS ENUM ('eval_error', 'cached', 'succeeded', 'failed', 'dep_failed');

-- The watched refs of each repository and their heads, as of the last poll.
CREATE TABLE refs (
  repo TEXT NOT NULL,
  kind ref_kind NOT NULL,
  name TEXT NOT NULL,
  sha TEXT NOT NULL,
  -- Outputs of the head's runs are pinned in niks3.
  pin BOOLEAN NOT NULL,
  -- Failures of the head's runs open a GitHub issue.
  issues BOOLEAN NOT NULL,
  PRIMARY KEY (repo, kind, name)
);

CREATE INDEX refs_sha ON refs (repo, sha);

-- One evaluation and build of a jobset at a commit.
CREATE TABLE runs (
  id BIGSERIAL PRIMARY KEY,
  repo TEXT NOT NULL,
  sha TEXT NOT NULL,
  jobset TEXT NOT NULL,
  attr TEXT NOT NULL,
  flake_ref TEXT NOT NULL,
  -- The highest priority of the refs that pointed at the commit.
  priority INTEGER NOT NULL,
  state run_state NOT NULL DEFAULT 'queued',
  error TEXT,
  superseded_by BIGINT REFERENCES runs (id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- Counts the run's executions, e.g. reruns. Identifies the current
  -- execution's checks on GitHub.
  execution INTEGER NOT NULL DEFAULT 1,
  -- When the current execution was queued.
  queued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  started_at TIMESTAMPTZ,
  evaluated_at TIMESTAMPTZ,
  finished_at TIMESTAMPTZ,
  check_eval_id BIGINT,
  check_build_id BIGINT,
  -- Bumped on every change the GitHub checks should show.
  report_seq BIGINT NOT NULL DEFAULT 1,
  reported_seq BIGINT NOT NULL DEFAULT 0,
  -- When the reporter last tried to report the run.
  reported_at TIMESTAMPTZ,
  -- The state the checks showed at the last report.
  reported_state run_state,
  -- Bumped whenever pins, issues and GC roots need handling again: for every
  -- execution, and when a ref at the commit starts tracking pins or issues.
  finalize_seq BIGINT NOT NULL DEFAULT 1,
  -- The finalize_seq the finalizer last handled, once the run was final.
  finalized_seq BIGINT NOT NULL DEFAULT 0,
  UNIQUE (repo, sha, jobset)
);

CREATE INDEX runs_active ON runs (state)
WHERE
  state IN ('queued', 'evaluating', 'building');

-- The refs whose head the run's commit has been.
CREATE TABLE run_refs (
  run_id BIGINT NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
  kind ref_kind NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (run_id, kind, name)
);

CREATE TABLE run_attrs (
  run_id BIGINT NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
  attr TEXT NOT NULL,
  drv_path TEXT,
  outputs JSONB,
  error TEXT,
  -- Set when the run finishes, because later runs may rebuild shared builds.
  result attr_result,
  PRIMARY KEY (run_id, attr)
);

-- Builds are shared by every run that needs the same derivation.
CREATE TABLE builds (
  drv_path TEXT PRIMARY KEY,
  system TEXT NOT NULL,
  required_features TEXT[] NOT NULL,
  state build_state NOT NULL DEFAULT 'queued',
  -- The longest chain of builds waiting for this one, itself included.
  height INTEGER NOT NULL DEFAULT 1,
  -- The worker of the current or last attempt.
  worker TEXT,
  lease UUID UNIQUE,
  attempts INTEGER NOT NULL DEFAULT 0,
  queued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- When the build of the current or last attempt became ready for its worker.
  ready_at TIMESTAMPTZ,
  started_at TIMESTAMPTZ,
  built_at TIMESTAMPTZ,
  finished_at TIMESTAMPTZ,
  outputs JSONB,
  error TEXT,
  log_tail TEXT
);

CREATE INDEX builds_queued ON builds (queued_at)
WHERE
  state = 'queued';

CREATE INDEX builds_leased ON builds (worker)
WHERE
  state IN ('running', 'uploading');

-- Edges between builds that still had to be built when they were evaluated.
CREATE TABLE build_deps (
  drv_path TEXT NOT NULL REFERENCES builds (drv_path),
  dep TEXT NOT NULL REFERENCES builds (drv_path),
  PRIMARY KEY (drv_path, dep)
);

CREATE INDEX build_deps_dep ON build_deps (dep);

-- The builds a run waits for, closed under build_deps.
CREATE TABLE run_builds (
  run_id BIGINT NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
  drv_path TEXT NOT NULL REFERENCES builds (drv_path),
  -- The build as it was when the run ended, since later runs may build it
  -- again. NULL while the run is active.
  state build_state,
  worker TEXT,
  attempts INTEGER,
  queued_at TIMESTAMPTZ,
  ready_at TIMESTAMPTZ,
  started_at TIMESTAMPTZ,
  built_at TIMESTAMPTZ,
  finished_at TIMESTAMPTZ,
  error TEXT,
  log_tail TEXT,
  PRIMARY KEY (run_id, drv_path)
);

CREATE INDEX run_builds_drv_path ON run_builds (drv_path);

-- The builds of each run, as they were when it ended or as they are while
-- it is active.
CREATE VIEW run_build_states AS
SELECT
  rb.run_id,
  b.drv_path,
  b.system,
  b.height,
  s.*
FROM
  run_builds rb
  JOIN builds b USING (drv_path)
  CROSS JOIN LATERAL (
    SELECT
      rb.state,
      rb.worker,
      rb.attempts,
      rb.queued_at,
      rb.ready_at,
      rb.started_at,
      rb.built_at,
      rb.finished_at,
      rb.error,
      rb.log_tail
    WHERE
      rb.state IS NOT NULL
    UNION ALL
    SELECT
      b.state,
      b.worker,
      b.attempts,
      b.queued_at,
      b.ready_at,
      b.started_at,
      b.built_at,
      b.finished_at,
      b.error,
      b.log_tail
    WHERE
      rb.state IS NULL
  ) s;

CREATE TABLE workers (
  name TEXT PRIMARY KEY,
  last_seen TIMESTAMPTZ NOT NULL,
  status JSONB NOT NULL
);

-- The niks3 pins Amphi made or asked for, one per branch, jobset, attribute
-- and output.
CREATE TABLE pins (
  name TEXT PRIMARY KEY,
  repo TEXT NOT NULL,
  branch TEXT NOT NULL,
  jobset TEXT NOT NULL,
  attr TEXT NOT NULL,
  output TEXT NOT NULL,
  -- NULL until niks3 answered the pin's creation.
  store_path TEXT,
  run_id BIGINT NOT NULL REFERENCES runs (id),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX pins_branch ON pins (repo, branch, jobset);

-- The open GitHub issue tracking the failures of a branch's jobset.
CREATE TABLE failure_issues (
  repo TEXT NOT NULL,
  branch TEXT NOT NULL,
  jobset TEXT NOT NULL,
  -- NULL until GitHub answered the issue's creation.
  issue_number BIGINT,
  -- The failing attributes the issue last reported.
  failing TEXT[] NOT NULL,
  PRIMARY KEY (repo, branch, jobset)
);
