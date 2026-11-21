/// <reference types="vitest/config" />
import process from "node:process";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(({ mode }) => ({
  plugins: [react()],

  define: {
    // The load check screen ships only in dev builds and `pnpm app:build:devtools`.
    __SWITCHYARD_DEVTOOLS__: JSON.stringify(mode === "development" || process.env.SWITCHYARD_DEVTOOLS === "1"),
  },

  // Tauri: keep Rust errors visible, use a fixed port, don't watch src-tauri.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    ...(host ? { hmr: { protocol: "ws", host, port: 1421 } } : {}),
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    // Safari 16 is the oldest WebKit on macOS 13.
    target: "safari16",
  },

  // Vitest: UI units in jsdom. Playwright owns e2e/ (pnpm test:e2e).
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
}));
