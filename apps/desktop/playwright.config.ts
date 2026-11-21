import { defineConfig, devices } from "@playwright/test";

// Port 1430, not Vite's 1420, so e2e runs don't collide with `pnpm dev`.
const PORT = 1430;
const CI = Boolean(process.env.CI);

// The app renders in WKWebView, so WebKit is the browser that matters.
export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/global-setup.ts",
  // One engine is shared by every test; keep them from racing each other.
  workers: 1,
  fullyParallel: false,
  forbidOnly: CI,
  retries: CI ? 1 : 0,
  reporter: CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: `http://127.0.0.1:${String(PORT)}`,
    trace: "retain-on-failure",
  },
  projects: [{ name: "webkit", use: { ...devices["Desktop Safari"] } }],
  webServer: {
    command: `pnpm exec vite --host 127.0.0.1 --port ${String(PORT)} --strictPort`,
    url: `http://127.0.0.1:${String(PORT)}`,
    reuseExistingServer: !CI,
    timeout: 60_000,
  },
});
