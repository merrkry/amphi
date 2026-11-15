import type { RunSummary, Worker } from "./api.ts";
import { buildPath, commitPath, refLabel, shortSha } from "./api.ts";
import { Ago, Show, State, usePoll } from "./common.tsx";

export function Overview() {
  const workers = usePoll<Worker[]>("/api/workers");
  const runs = usePoll<RunSummary[]>("/api/runs?limit=50");
  return (
    <>
      <h2>Workers</h2>
      <Show loaded={workers}>{(workers) => <Workers workers={workers} />}</Show>
      <h2>Runs</h2>
      <Show loaded={runs}>{(runs) => <Runs runs={runs} />}</Show>
    </>
  );
}

function Workers(props: { workers: Worker[] }) {
  return (
    <table>
      <thead>
        <tr>
          <th>Worker</th>
          <th>Seen</th>
          <th>Slots</th>
          <th>Idle cores</th>
          <th>Builds</th>
        </tr>
      </thead>
      <tbody>
        {props.workers.map((worker) => (
          <tr key={worker.name}>
            <td>{worker.name}</td>
            <td>
              <Ago at={worker.last_seen} />
            </td>
            <td>
              {worker.status.running.length} / {worker.status.max_jobs}
              {worker.status.uploading.length > 0 &&
                `, ${worker.status.uploading.length} uploading`}
            </td>
            <td>
              {worker.status.load.idle_cores.toFixed(1)} / {worker.status.load.cpus}
            </td>
            <td>
              {worker.builds.map((build) => (
                <div key={build.drv_path}>
                  <a href={buildPath(build.drv_path)}>
                    <code>{build.drv_path}</code>
                  </a>{" "}
                  <State state={build.state} /> <Ago at={build.started_at} />
                </div>
              ))}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Runs(props: { runs: RunSummary[] }) {
  return (
    <table>
      <thead>
        <tr>
          <th>Commit</th>
          <th>Repository</th>
          <th>Refs</th>
          <th>Jobset</th>
          <th>State</th>
          <th>Builds</th>
          <th>Created</th>
        </tr>
      </thead>
      <tbody>
        {props.runs.map((run) => {
          const builds = run.builds;
          const total = Object.values(builds).reduce((sum, count) => sum + count, 0);
          const done = (builds.succeeded ?? 0) + (builds.failed ?? 0) + (builds.dep_failed ?? 0);
          return (
            <tr key={run.id}>
              <td>
                <a href={commitPath(run.repo, run.sha, run.jobset)}>
                  <code>{shortSha(run.sha)}</code>
                </a>
              </td>
              <td>{run.repo}</td>
              <td>{run.refs.map(refLabel).join(", ")}</td>
              <td>{run.jobset}</td>
              <td>
                <State state={run.state} />
              </td>
              <td>{total > 0 ? `${done} / ${total}` : "–"}</td>
              <td>
                <Ago at={run.created_at} />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
