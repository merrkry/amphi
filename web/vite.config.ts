import { defineConfig } from "vite-plus";

export default defineConfig({
  oxc: { jsx: { runtime: "automatic", importSource: "preact" } },
  // `vp dev` forwards the API to an Amphi server, by default a local one.
  server: {
    proxy: { "/api": process.env["AMPHI_URL"] ?? "http://127.0.0.1:8080" },
  },
});
