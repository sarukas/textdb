/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const target = process.env.TEXTDB_API ?? "http://localhost:4317";

export default defineConfig({
  plugins: [react()],
  // CodeMirror + markdown-it in one bundle is expected for a local tool.
  build: { chunkSizeWarningLimit: 1500 },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        // SSE connections are long-lived: never time them out in the proxy.
        timeout: 0,
        proxyTimeout: 0,
        configure(proxy) {
          proxy.on("proxyReq", (proxyReq, req) => {
            if (req.url?.startsWith("/api/events")) {
              // A compressed event stream is buffered by the encoder; ask for identity.
              proxyReq.setHeader("accept-encoding", "identity");
            }
          });
          proxy.on("proxyRes", (proxyRes, _req, res) => {
            const type = String(proxyRes.headers["content-type"] ?? "");
            if (type.startsWith("text/event-stream")) {
              // Only adjust headers here: the proxy copies them onto `res` after this event,
              // so flushing now would send the response without its content-type.
              proxyRes.headers["cache-control"] = "no-cache, no-transform";
              proxyRes.headers["x-accel-buffering"] = "no";
              delete proxyRes.headers["content-length"];
              res.socket?.setNoDelay(true);
            }
          });
        },
      },
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
