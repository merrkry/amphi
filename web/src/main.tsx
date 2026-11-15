import { render } from "preact";
import { useEffect, useState } from "preact/hooks";

import { BuildPage } from "./build.tsx";
import { CommitPage } from "./commit.tsx";
import { Overview } from "./overview.tsx";

function usePath(): string {
  const [path, setPath] = useState(location.pathname);
  useEffect(() => {
    const update = () => setPath(location.pathname);
    // Links within the app change the page without reloading it.
    const click = (event: MouseEvent) => {
      const link = (event.target as Element | null)?.closest("a");
      if (
        link === null ||
        link === undefined ||
        link.origin !== location.origin ||
        link.hasAttribute("data-native") ||
        event.button !== 0 ||
        event.metaKey ||
        event.ctrlKey ||
        event.shiftKey
      ) {
        return;
      }
      event.preventDefault();
      history.pushState(null, "", link.pathname + link.search + link.hash);
      window.scrollTo(0, 0);
      update();
    };
    addEventListener("popstate", update);
    document.addEventListener("click", click);
    return () => {
      removeEventListener("popstate", update);
      document.removeEventListener("click", click);
    };
  }, []);
  return path;
}

function Page(props: { path: string }) {
  const commit = /^\/([^/]+)\/([^/]+)\/commit\/([0-9a-f]+)$/i.exec(props.path);
  if (commit?.[1] !== undefined && commit[2] !== undefined && commit[3] !== undefined) {
    return <CommitPage owner={commit[1]} repo={commit[2]} sha={commit[3]} />;
  }
  const build = /^\/builds\/([^/]+)$/.exec(props.path);
  if (build?.[1] !== undefined) return <BuildPage name={build[1]} />;
  return <Overview />;
}

function App() {
  const path = usePath();
  return (
    <>
      <nav>
        <a href="/">Amphi</a>
      </nav>
      <main>
        <Page path={path} />
      </main>
    </>
  );
}

const root = document.getElementById("app");
if (root !== null) render(<App />, root);
