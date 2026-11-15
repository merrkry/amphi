import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";

import { Unauthorized, get, getText, loginUrl } from "./api.ts";

export type Loaded<T> = { ok: true; data: T } | { ok: false; error: Error };

/** Fetches `path`, and fetches it again every few seconds while `live` says so. */
export function usePoll<T>(
  path: string,
  live: (data: T) => boolean = () => true,
): Loaded<T> | undefined {
  const [loaded, setLoaded] = useState<Loaded<T>>();
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stopped = false;
    const load = async () => {
      try {
        const data = await get<T>(path);
        if (stopped) return;
        setLoaded({ ok: true, data });
        if (live(data)) timer = setTimeout(() => void load(), 5000);
      } catch (error) {
        if (!stopped)
          setLoaded({
            ok: false,
            error: error instanceof Error ? error : new Error(String(error)),
          });
      }
    };
    setLoaded(undefined);
    void load();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [path]);
  return loaded;
}

/** Fetches `path` as text, unless it is null. A failure shows as the text. */
export function useText(path: string | null): string | undefined {
  const [text, setText] = useState<string>();
  useEffect(() => {
    if (path === null) return;
    let stopped = false;
    const show = (text: string) => {
      if (!stopped) setText(text);
    };
    getText(path).then(show, (error: unknown) => show(String(error)));
    return () => {
      stopped = true;
    };
  }, [path]);
  return text;
}

/** Renders what `usePoll` loaded, or why it could not. */
export function Show<T>(props: {
  loaded: Loaded<T> | undefined;
  children: (data: T) => ComponentChildren;
}) {
  const { loaded } = props;
  if (loaded === undefined) return <p class="muted">Loading…</p>;
  if (loaded.ok) return <>{props.children(loaded.data)}</>;
  if (loaded.error instanceof Unauthorized) {
    return (
      <p>
        <a href={loginUrl()} data-native>
          Log in with GitHub
        </a>
      </p>
    );
  }
  return <pre class="error">{loaded.error.message}</pre>;
}

export function State(props: { state: string }) {
  return <span class={`state ${props.state}`}>{props.state.replace("_", " ")}</span>;
}

export function Ago(props: { at: string | null }) {
  if (props.at === null) return <span class="muted">–</span>;
  const seconds = Math.max(0, (Date.now() - Date.parse(props.at)) / 1000);
  const local = new Date(props.at).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "long",
  });
  return (
    <time dateTime={props.at} title={local}>
      {duration(seconds)} ago
    </time>
  );
}

export function duration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "–";
  if (seconds < 1) return `${Math.round(seconds * 1000)}ms`;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.floor(seconds % 60)}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

export function between(from: string | null, to: string | null): number | null {
  if (from === null || to === null) return null;
  return (Date.parse(to) - Date.parse(from)) / 1000;
}
