import type { BuildDetail } from "./api.ts";
import { buildPath, commitPath, shortSha } from "./api.ts";
import { Ago, Show, State, between, duration, usePoll, useText } from "./common.tsx";

export function BuildPage(props: { name: string }) {
  const build = usePoll<BuildDetail>(`/api/builds/${props.name}`, (data) =>
    ["queued", "running", "uploading"].includes(data.state),
  );
  return <Show loaded={build}>{(build) => <Build build={build} name={props.name} />}</Show>;
}

function Build(props: { build: BuildDetail; name: string }) {
  const { build } = props;
  return (
    <>
      <h2>
        <code>{build.drv_path}</code> <State state={build.state} />
      </h2>
      <table class="facts">
        <tbody>
          <tr>
            <th>System</th>
            <td>
              {build.system}
              {build.required_features.length > 0 && ` (${build.required_features.join(", ")})`}
            </td>
          </tr>
          <tr>
            <th>Worker</th>
            <td>
              {build.worker ?? "–"}, attempt {build.attempts + 1}
            </td>
          </tr>
          <tr>
            <th>Queued</th>
            <td>
              <Ago at={build.queued_at} />
            </td>
          </tr>
          <tr>
            <th>Time</th>
            <td>
              waiting for a slot {duration(between(build.ready_at, build.started_at))}, building{" "}
              {duration(between(build.started_at, build.built_at))}, uploading{" "}
              {duration(
                build.state === "succeeded" ? between(build.built_at, build.finished_at) : null,
              )}
            </td>
          </tr>
          <tr>
            <th>Height</th>
            <td>{build.height} builds wait for this one, in the longest chain</td>
          </tr>
          <tr>
            <th>Runs</th>
            <td>
              {build.runs.map((run) => (
                <div key={`${run.repo}/${run.sha}/${run.jobset}`}>
                  <a href={commitPath(run.repo, run.sha, run.jobset)}>
                    {run.repo}@<code>{shortSha(run.sha)}</code> {run.jobset}
                  </a>{" "}
                  <State state={run.state} />
                </div>
              ))}
            </td>
          </tr>
          {build.outputs !== null && (
            <tr>
              <th>Outputs</th>
              <td>
                {Object.entries(build.outputs).map(([name, path]) => (
                  <div key={name}>
                    {name}: <code>{path}</code>
                  </div>
                ))}
              </td>
            </tr>
          )}
          {build.deps.length > 0 && (
            <tr>
              <th>Dependencies</th>
              <td>
                {build.deps.map((dep) => (
                  <div key={dep}>
                    <a href={buildPath(dep)}>
                      <code>{dep}</code>
                    </a>
                  </div>
                ))}
              </td>
            </tr>
          )}
        </tbody>
      </table>
      {build.error !== null && <pre class="error">{build.error}</pre>}
      {build.state === "failed" && <Log name={props.name} />}
    </>
  );
}

function Log(props: { name: string }) {
  const log = useText(`/api/builds/${props.name}/log`);
  if (!log) return null;
  return (
    <>
      <h3>Log</h3>
      <pre>{log}</pre>
    </>
  );
}
