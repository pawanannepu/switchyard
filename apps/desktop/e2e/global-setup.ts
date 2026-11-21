// Starts a devtools engine and `sy-devbridge` in a throwaway data dir, then
// hands the bridge's port and token to the tests through the environment.
// Playwright drives the Vite build in WebKit; the page connects with
// `?bridge=<port>&token=<token>` (see src/engine/client.ts).

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";

const root = resolve(import.meta.dirname, "../../..");
const bin = (name: string) => join(root, "target/debug", name);
const STARTUP_TIMEOUT_MS = 30_000;

/** What the tests read from `process.env`. */
export const BRIDGE_ENV = { port: "SY_E2E_BRIDGE_PORT", token: "SY_E2E_BRIDGE_TOKEN" } as const;

function stop(child: ChildProcess | undefined) {
  if (child?.exitCode === null) child.kill("SIGTERM");
}

async function waitFor(check: () => boolean, what: string) {
  const deadline = Date.now() + STARTUP_TIMEOUT_MS;
  while (!check()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(50);
  }
}

function readBridgeLine(bridge: ChildProcess): Promise<{ port: number; token: string }> {
  const stdout = bridge.stdout;
  if (!stdout) return Promise.reject(new Error("sy-devbridge has no stdout"));
  return new Promise((resolveLine, reject) => {
    const lines = createInterface({ input: stdout });
    const timer = setTimeout(() => {
      reject(new Error("sy-devbridge printed no port line"));
    }, STARTUP_TIMEOUT_MS);
    lines.once("line", (line) => {
      clearTimeout(timer);
      lines.close();
      const parsed: unknown = JSON.parse(line);
      if (
        typeof parsed === "object" &&
        parsed !== null &&
        "port" in parsed &&
        "token" in parsed &&
        typeof parsed.port === "number" &&
        typeof parsed.token === "string"
      ) {
        resolveLine({ port: parsed.port, token: parsed.token });
      } else {
        reject(new Error(`unexpected sy-devbridge line: ${line}`));
      }
    });
    bridge.once("exit", (code) => {
      reject(new Error(`sy-devbridge exited early with ${String(code)}`));
    });
  });
}

export default async function globalSetup() {
  execFileSync("cargo", ["build", "-q", "-p", "sy-daemon", "-p", "sy-devbridge", "--features", "sy-daemon/devtools"], {
    cwd: root,
    stdio: "inherit",
  });

  // Short on purpose: macOS caps socket paths at 104 bytes.
  const dataDir = mkdtempSync("/tmp/sy-");
  const env = { ...process.env, SWITCHYARD_DATA_DIR: dataDir };
  let engine: ChildProcess | undefined;
  let bridge: ChildProcess | undefined;
  const teardown = () => {
    stop(bridge);
    stop(engine);
    rmSync(dataDir, { recursive: true, force: true });
  };

  try {
    engine = spawn(bin("switchyardd"), [], { env, stdio: ["ignore", "ignore", "inherit"] });
    await waitFor(() => existsSync(join(dataDir, "engine.sock")), "the engine socket");

    bridge = spawn(bin("sy-devbridge"), [], { env, stdio: ["ignore", "pipe", "inherit"] });
    const { port, token } = await readBridgeLine(bridge);
    process.env[BRIDGE_ENV.port] = String(port);
    process.env[BRIDGE_ENV.token] = token;
  } catch (error) {
    teardown();
    throw error;
  }
  return teardown;
}
