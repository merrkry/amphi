// Types and requests for the server's JSON API, see src/server/web.rs.

export type RunState = "queued" | "evaluating" | "building" | "succeeded" | "failed" | "superseded";

export type BuildState = "queued" | "running" | "uploading" | "succeeded" | "failed" | "dep_failed";

export type AttrStatus =
  | "queued"
  | "running"
  | "uploading"
  | "eval_error"
  | "cached"
  | "succeeded"
  | "failed"
  | "dep_failed";

export interface Ref {
  kind: "branch" | "pull";
  name: string;
}

interface RunFields {
  jobset: string;
  state: RunState;
  error: string | null;
  // The commit of the run that replaced this one.
  superseded_by: string | null;
  created_at: string;
  started_at: string | null;
  evaluated_at: string | null;
  finished_at: string | null;
  // Refs whose head the commit has been.
  refs: Ref[];
}

export interface RunSummary extends RunFields {
  // A cursor for paging, with `before`.
  id: number;
  repo: string;
  sha: string;
  builds: Partial<Record<BuildState, number>>;
}

export interface Commit {
  repo: string;
  sha: string;
  // Watched refs whose head this commit is.
  heads: Ref[];
  runs: RunDetail[];
}

export interface RunDetail extends RunFields {
  attr: string;
  flake_ref: string;
  progress: Record<BuildState, number>;
  timing: Timing;
  attrs: Attr[];
}

export interface Timing {
  queued_seconds: number | null;
  evaluation_seconds: number | null;
  building_seconds: number | null;
  builds: {
    started: number;
    wait_seconds_total: number | null;
    wait_seconds_max: number | null;
    build_seconds_total: number | null;
    build_seconds_max: number | null;
    upload_seconds_total: number | null;
    upload_seconds_max: number | null;
  };
}

export interface Attr {
  attr: string;
  status: AttrStatus;
  drv_path: string | null;
  outputs: Record<string, string | null> | null;
  error: string | null;
}

export interface Build {
  drv_path: string;
  system: string;
  state: BuildState;
  height: number;
  worker: string | null;
  attempts: number;
  queued_at: string;
  ready_at: string | null;
  started_at: string | null;
  built_at: string | null;
  finished_at: string | null;
  error: string | null;
}

export interface BuildDetail extends Build {
  required_features: string[];
  outputs: Record<string, string> | null;
  deps: string[];
  runs: { repo: string; sha: string; jobset: string; state: RunState }[];
}

export interface Worker {
  name: string;
  last_seen: string;
  status: {
    max_jobs: number;
    running: string[];
    uploading: string[];
    load: { cpus: number; idle_cores: number; mem_available_bytes: number };
  };
  builds: { drv_path: string; state: BuildState; started_at: string; built_at: string | null }[];
}

export class Unauthorized extends Error {}

export async function get<T>(path: string): Promise<T> {
  const response = await fetch(path, { headers: { accept: "application/json" } });
  if (response.status === 401) throw new Unauthorized(await response.text());
  if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);
  return (await response.json()) as T;
}

export async function getText(path: string): Promise<string> {
  const response = await fetch(path);
  if (response.status === 401) throw new Unauthorized(await response.text());
  if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);
  return await response.text();
}

export async function post(path: string): Promise<void> {
  const response = await fetch(path, { method: "POST" });
  if (response.status === 401) throw new Unauthorized(await response.text());
  if (!response.ok) throw new Error(await response.text());
}

export function loginUrl(): string {
  return `/api/auth/login?next=${encodeURIComponent(location.pathname)}`;
}

// The API names builds by the file name of their derivation.
export function buildName(drvPath: string): string {
  return drvPath.slice(drvPath.lastIndexOf("/") + 1);
}

// The page of a build.
export function buildPath(drvPath: string): string {
  return `/builds/${buildName(drvPath)}`;
}

// The page of a commit, at the same path as on GitHub.
export function commitPath(repo: string, sha: string, jobset?: string): string {
  return `/${repo}/commit/${sha}${jobset === undefined ? "" : `#${encodeURIComponent(jobset)}`}`;
}

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

// A ref as GitHub shows it, e.g. `master` or `#12`.
export function refLabel(ref: Ref): string {
  return ref.kind === "pull" ? `#${ref.name}` : ref.name;
}
