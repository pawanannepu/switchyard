import { expect, test, type Page } from "@playwright/test";
import { strings } from "../src/strings.ts";
import { BRIDGE_ENV } from "./global-setup.ts";

function bridgeQuery(): string {
  const port = process.env[BRIDGE_ENV.port];
  const token = process.env[BRIDGE_ENV.token];
  if (!port || !token) throw new Error("global setup did not start the dev bridge");
  return `?bridge=${port}&token=${encodeURIComponent(token)}`;
}

async function openApp(page: Page) {
  await page.goto(`/${bridgeQuery()}`);
  await expect(page.getByRole("heading", { level: 1, name: strings.app.name })).toBeVisible();
}

test("shows the engine's hello reply once connected through the bridge", async ({ page }) => {
  await openApp(page);
  await expect(page.getByRole("status")).toHaveText(strings.connection.connected);
  await expect(page.getByTestId("engine-version")).toHaveText(/^\d+\.\d+\.\d+/);
  // `PROTOCOL` in crates/sy-protocol/src/lib.rs (spec 0003 raised the minor).
  await expect(page.getByTestId("engine-protocol")).toHaveText("1.1");
  await expect(page.getByTestId("engine-pid")).toHaveText(/^\d+$/);
});

test("reaches the load check from the keyboard", async ({ page }) => {
  await openApp(page);
  const nav = page.getByRole("navigation", { name: strings.nav.main });
  const link = nav.getByRole("link", { name: strings.nav.loadCheck });
  // On macOS, WebKit's plain Tab skips links (unless full keyboard navigation
  // is on); Option+Tab is the built in way to reach them, so press that.
  await nav.getByRole("link", { name: strings.nav.home }).focus();
  await page.keyboard.press("Alt+Tab");
  await expect(link).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("heading", { level: 2, name: strings.load.heading })).toBeVisible();
});
