import { useEffect, useState } from "preact/hooks";

import type { Build, Commit, Ref, RunDetail, Timing } from "./api.ts";
import { buildName, buildPath, commitPath, post, refLabel, shortSha } from "./api.ts";
import { Ago, Show, State, duration, usePoll, useText } from "./common.tsx";

const ACTIVE = ["queued", "evaluating", "building"];

export function CommitPage(props: { owner: string; repo: string; sha: string }) {
  const api = `/api/repos/${props.owner}/${props.repo}/commits/${props.sha}`;
  const commit = usePoll<Commit>(api, (data) =>
    data.runs.some((run) => ACTIVE.includes(run.state)),
  );
  const loaded = commit?.ok === true;
  // The browser scrolls to a jobset's anchor before its section exists.
  useEffect(() => {
    if (loaded && location.hash !== "") {
      document.getElementById(decodeURIComponent(location.hash.slice(1)))?.scrollIntoView();
    }
  }, [loaded]);
  return <Show loaded={commit}>{(commit) => <CommitRuns commit={commit} api={api} />}</Show>;
}

function CommitRuns(props: { commit: Commit; api: string }) {
  const { commit } = props;
  const repoUrl = `https://github.com/${commit.repo}`;
  return (
    <>
      <h2>
        {commit.repo}@<code>{shortSha(commit.sha)}</code>
      </h2>
      <table class="facts">
        <tbody>
          <tr>
            <th>Commit</th>
            <td>
              <a href={`${repoUrl}/commit/${commit.sha}`} data-native>
                <code>{commit.sha}</code>
              </a>
            </td>
          </tr>
          <tr>
            <th>Head of</th>
            <td>
              {commit.heads.length > 0 ? (
                <Refs repo={commit.repo} refs={commit.heads} />
              ) : (
                <span class="muted">no watched ref</span>
              )}
            </td>
          </tr>
        </tbody>
      </table>
      {commit.runs.map((run) => (
        <Run
          key={run.jobset}
          repo={commit.repo}
          run={run}
          api={`${props.api}/runs/${encodeURIComponent(run.jobset)}`}
        />
      ))}
    </>
  );
}

function Refs(props: { repo: string; refs: Ref[] }) {
  const repoUrl = `https://github.com/${props.repo}`;
  return (
    <>
      {props.refs.map((ref, index) => (
        <span key={`${ref.kind}/${ref.name}`}>
          {index > 0 && ", "}
          <a
            href={
              ref.kind === "pull" ? `${repoUrl}/pull/${ref.name}` : `${repoUrl}/tree/${ref.name}`
            }
            data-native
          >
            {refLabel(ref)}
          </a>
        </span>
      ))}
    </>
  );
}

function Run(props: { repo: string; run: RunDetail; api: string }) {
  const { run } = props;
  const { timing } = run;
  const failing = run.attrs.filter((attr) =>
    ["failed", "dep_failed", "eval_error"].includes(attr.status),
  );
  return (
    <section id={run.jobset}>
      <h3>
        {run.jobset} <State state={run.state} />
      </h3>
      <table class="facts">
        <tbody>
          <tr>
            <th>Attribute</th>
            <td>
              <code>{run.attr}</code>
            </td>
          </tr>
          <tr>
            <th>Built for</th>
            <td>
              <Refs repo={props.repo} refs={run.refs} />
            </td>
          </tr>
          {run.superseded_by !== null && (
            <tr>
              <th>Superseded by</th>
              <td>
                <a href={commitPath(props.repo, run.superseded_by, run.jobset)}>
                  <code>{shortSha(run.superseded_by)}</code>
                </a>
              </td>
            </tr>
          )}
          <tr>
            <th>Created</th>
            <td>
              <Ago at={run.created_at} />
            </td>
          </tr>
          <tr>
            <th>Queued</th>
            <td>{duration(timing.queued_seconds)}</td>
          </tr>
          <tr>
            <th>Evaluating</th>
            <td>{duration(timing.evaluation_seconds)}</td>
          </tr>
          <tr>
            <th>Building</th>
            <td>{duration(timing.building_seconds)}</td>
          </tr>
          <tr>
            <th>Progress</th>
            <td>
              {Object.entries(run.progress)
                .filter(([, count]) => count > 0)
                .map(([state, count]) => `${count} ${state.replace("_", " ")}`)
                .join(", ") || "nothing to build"}
            </td>
          </tr>
        </tbody>
      </table>
      <BuildTimes builds={timing.builds} />
      <Rerun run={run} api={props.api} />
      {run.error !== null && <pre class="error">{run.error}</pre>}
      {failing.length > 0 && (
        // Loads the list again when another build fails.
        <FailedBuilds key={run.progress.failed} api={props.api} />
      )}
      <h4>Attributes</h4>
      <table>
        <tbody>
          {run.attrs.map((attr) => (
            <tr key={attr.attr}>
              <td>
                <State state={attr.status} />
              </td>
              <td>
                <code>{attr.attr}</code>
                {attr.error !== null && <pre class="error">{attr.error}</pre>}
              </td>
              <td>
                {attr.drv_path !== null &&
                  // The run builds nothing for a cached attribute.
                  (attr.status === "cached" ? (
                    <code>{attr.drv_path}</code>
                  ) : (
                    <a href={buildPath(attr.drv_path)}>
                      <code>{attr.drv_path}</code>
                    </a>
                  ))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

function BuildTimes(props: { builds: Timing["builds"] }) {
  const { builds } = props;
  const rows = [
    ["Waiting for a slot", builds.wait_seconds_total, builds.wait_seconds_max],
    ["Building", builds.build_seconds_total, builds.build_seconds_max],
    ["Uploading", builds.upload_seconds_total, builds.upload_seconds_max],
  ] as const;
  return (
    <table class="facts times">
      <thead>
        <tr>
          <th>{builds.started} builds started</th>
          <th>Total</th>
          <th>Longest</th>
        </tr>
      </thead>
      <tbody>
        {rows.map(([label, total, longest]) => (
          <tr key={label}>
            <th>{label}</th>
            <td>{duration(total)}</td>
            <td>{duration(longest)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Rerun(props: { run: RunDetail; api: string }) {
  const [message, setMessage] = useState<string>();
  if (ACTIVE.includes(props.run.state)) return null;
  const rerun = async () => {
    try {
      await post(`${props.api}/rerun`);
      location.reload();
    } catch (error) {
      setMessage(String(error));
    }
  };
  return (
    <p>
      <button onClick={() => void rerun()}>Run again</button> {message}
    </p>
  );
}

/** The build's log as the run saw it, loaded once opened. */
function RunLog(props: { path: string }) {
  const [open, setOpen] = useState(false);
  const log = useText(open ? props.path : null);
  return (
    <details onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>Log</summary>
      {log && <pre>{log}</pre>}
    </details>
  );
}

function FailedBuilds(props: { api: string }) {
  const builds = usePoll<Build[]>(`${props.api}/builds?state=failed`, () => false);
  return (
    <>
      <h4>Failed builds</h4>
      <Show loaded={builds}>
        {(builds) => (
          <ul>
            {builds.map((build) => (
              <li key={build.drv_path}>
                <a href={buildPath(build.drv_path)}>
                  <code>{build.drv_path}</code>
                </a>
                {build.worker !== null && ` on ${build.worker}`}
                {build.error !== null && `: ${build.error}`}
                <RunLog path={`${props.api}/builds/${buildName(build.drv_path)}/log`} />
              </li>
            ))}
          </ul>
        )}
      </Show>
    </>
  );
}
