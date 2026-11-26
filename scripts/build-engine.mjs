// Builds switchyardd and its hook helper sy-signal (spec 0003) and places them
// where Tauri's `externalBin` expects them, so the bundle has both in
// Contents/MacOS, signed with the same identity:
// apps/desktop/src-tauri/binaries/<name>-<target triple>.
//
//   node scripts/build-engine.mjs                       debug, host target
//   node scripts/build-engine.mjs --release --universal  both macOS archs, plus the lipo'd universal binary
//   SWITCHYARD_DEVTOOLS=1 (or --devtools)                adds the dev.* methods (never for a public release)

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, chmodSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const release = args.includes("--release");
const universal = args.includes("--universal");
const devtools = args.includes("--devtools") || process.env.SWITCHYARD_DEVTOOLS === "1";
const targetArg = args.find((a) => a.startsWith("--target="))?.slice("--target=".length);

const run = (cmd, cmdArgs) => execFileSync(cmd, cmdArgs, { cwd: root, stdio: "inherit" });
const hostTriple = () =>
  execFileSync("rustc", ["-vV"], { encoding: "utf8" })
    .split("\n")
    .find((l) => l.startsWith("host:"))
    .slice("host: ".length)
    .trim();

const targets = universal ? ["aarch64-apple-darwin", "x86_64-apple-darwin"] : [targetArg ?? hostTriple()];
const outDir = join(root, "apps/desktop/src-tauri/binaries");
mkdirSync(outDir, { recursive: true });

// sy-fake-agent is dev and test only: never built here, never bundled.
const binaries = [
  { pkg: "sy-daemon", bin: "switchyardd", features: devtools ? ["--features", "devtools"] : [] },
  { pkg: "sy-signal", bin: "sy-signal", features: [] },
];

const built = [];
for (const { pkg, bin, features } of binaries) {
  const perTarget = [];
  for (const target of targets) {
    const cargoArgs = ["build", "-p", pkg, "--bin", bin, "--target", target, ...features];
    if (release) cargoArgs.push("--release");
    run("cargo", cargoArgs);
    const src = join(root, "target", target, release ? "release" : "debug", bin);
    const dest = join(outDir, `${bin}-${target}`);
    copyFileSync(src, dest);
    chmodSync(dest, 0o755);
    perTarget.push(dest);
  }
  built.push(...perTarget);
  if (universal) {
    const dest = join(outDir, `${bin}-universal-apple-darwin`);
    run("lipo", ["-create", "-output", dest, ...perTarget]);
    built.push(dest);
  }
}

console.log(`switchyardd and sy-signal (${release ? "release" : "debug"}${devtools ? ", devtools" : ""}):`);
for (const path of built) console.log(`  ${path}`);
