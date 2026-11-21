// Checks a built switchyard.app against spec 0001's release proofs.
//
//   node scripts/release-check.mjs [path/to/switchyard.app] [--require-team-id]
//
// Without a Developer ID certificate the bundle is ad hoc signed, which has no
// Team ID; that check is a warning unless --require-team-id (feature 24 turns it on).

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync } from "node:fs";
import { connect } from "node:net";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const requireTeamId = args.includes("--require-team-id");
const app =
  args.find((a) => !a.startsWith("--")) ?? join(root, "target/universal-apple-darwin/release/bundle/macos/switchyard.app");

let failed = 0;
const pass = (msg) => console.log(`  ok    ${msg}`);
const fail = (msg) => {
  failed++;
  console.log(`  FAIL  ${msg}`);
};
const warn = (msg) => console.log(`  warn  ${msg}`);
// stdout and stderr together: codesign reports on stderr.
const sh = (cmd, cmdArgs) => {
  const result = spawnSync(cmd, cmdArgs, { encoding: "utf8" });
  return `${result.stdout ?? ""}${result.stderr ?? ""}`;
};

console.log(`Checking ${app}`);
if (!existsSync(app)) {
  console.error("No app bundle there. Run `pnpm app:build` first.");
  process.exit(1);
}

const plist = join(app, "Contents/Info.plist");
const plistValue = (key) => sh("plutil", ["-extract", key, "raw", "-o", "-", plist]).trim();
plistValue("CFBundleIdentifier") === "dev.switchyard.app"
  ? pass("bundle id is dev.switchyard.app")
  : fail(`bundle id is ${plistValue("CFBundleIdentifier")}`);
plistValue("LSMinimumSystemVersion") === "13.0"
  ? pass("minimum macOS is 13.0")
  : fail(`minimum macOS is ${plistValue("LSMinimumSystemVersion")}`);

const mainExe = join(app, "Contents/MacOS", plistValue("CFBundleExecutable"));
const engine = join(app, "Contents/MacOS/switchyardd");
for (const [label, bin] of [
  ["app", mainExe],
  ["switchyardd", engine],
]) {
  if (!existsSync(bin)) {
    fail(`${label} binary missing at ${bin}`);
    continue;
  }
  const archs = sh("lipo", ["-archs", bin]).trim().split(/\s+/);
  archs.includes("arm64") && archs.includes("x86_64")
    ? pass(`${label} is universal (${archs.join(", ")})`)
    : fail(`${label} archs: ${archs.join(", ")}`);
}

const signing = (bin) => {
  const info = sh("codesign", ["-dv", "--verbose=4", bin]);
  return {
    runtime: /flags=0x[0-9a-f]+\([^)]*runtime[^)]*\)/.test(info),
    team: /TeamIdentifier=(.+)/.exec(info)?.[1]?.trim() ?? "not set",
    signed: !info.includes("code object is not signed"),
  };
};
const appSig = signing(app);
const engineSig = signing(engine);
appSig.signed && engineSig.signed ? pass("app and engine are signed") : fail("app or engine is not signed");
engineSig.runtime ? pass("engine has the hardened runtime") : fail("engine lacks the hardened runtime");
appSig.runtime ? pass("app has the hardened runtime") : fail("app lacks the hardened runtime");
if (appSig.team !== "not set" && appSig.team === engineSig.team) pass(`app and engine share Team ID ${appSig.team}`);
else if (requireTeamId) fail(`Team IDs: app ${appSig.team}, engine ${engineSig.team}`);
else warn(`no Team ID (app ${appSig.team}, engine ${engineSig.team}): ad hoc signed; a Developer ID identity is feature 24's`);

const found = [];
const walk = (dir) => {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (entry.includes("devbridge")) found.push(path);
    if (statSync(path, { throwIfNoEntry: false })?.isDirectory()) walk(path);
  }
};
walk(app);
found.length === 0 ? pass("no sy-devbridge in the bundle") : fail(`dev bridge in bundle: ${found.join(", ")}`);

// The bundled engine must not answer any dev.* method.
const dataDir = mkdtempSync("/tmp/sy-rc-");
const child = spawn(engine, [], { env: { ...process.env, SWITCHYARD_DATA_DIR: dataDir }, stdio: "ignore" });
const socket = join(dataDir, "engine.sock");
const until = Date.now() + 5000;
while (!existsSync(socket) && Date.now() < until) await new Promise((r) => setTimeout(r, 50));

const probe = () =>
  new Promise((resolve, reject) => {
    const conn = connect(socket);
    let buffer = "";
    const replies = new Map();
    const send = (id, method, params) => conn.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    conn.on("connect", () => {
      send(1, "session.hello", { client: "test", client_version: "release-check", protocol: { major: 1, minor: 0 } });
      send(2, "dev.synthetic_run", { count: 1, bytes_per_sec: 0, echo: false });
      send(3, "dev.stop_runs", {});
      send(4, "engine.shutdown", {});
    });
    conn.on("data", (chunk) => {
      buffer += chunk;
      let nl;
      while ((nl = buffer.indexOf("\n")) >= 0) {
        const msg = JSON.parse(buffer.slice(0, nl));
        buffer = buffer.slice(nl + 1);
        if (msg.id !== undefined) replies.set(msg.id, msg);
        if (replies.size === 4) {
          conn.end();
          resolve(replies);
        }
      }
    });
    conn.on("error", reject);
    setTimeout(() => reject(new Error("engine did not answer within 5 s")), 5000);
  });

try {
  const replies = await probe();
  const notFound = [2, 3].every((id) => replies.get(id)?.error?.code === -32601);
  notFound ? pass("bundled engine answers no dev.* method") : fail(`dev.* answered: ${JSON.stringify([replies.get(2), replies.get(3)])}`);
} catch (err) {
  fail(`could not probe the bundled engine: ${err.message}`);
} finally {
  child.kill();
  rmSync(dataDir, { recursive: true, force: true });
}

console.log(failed === 0 ? "Release check passed." : `Release check failed (${failed}).`);
process.exit(failed === 0 ? 0 : 1);
