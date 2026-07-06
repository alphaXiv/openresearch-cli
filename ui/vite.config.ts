import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  build: { outDir: "dist" },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:4791",
      "/opencode": "http://127.0.0.1:4791",
    },
  },
});
