import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Backend the dev server proxies to. Defaults to the standard `orx up` port;
// override with ORX_BACKEND when running against a backend on another port.
const backend = process.env.ORX_BACKEND ?? "http://127.0.0.1:4791";

export default defineConfig({
  plugins: [react()],
  build: { outDir: "dist" },
  server: {
    proxy: {
      "/api": backend,
      "/opencode": backend,
    },
  },
});
