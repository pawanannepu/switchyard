// Builds switchyardd and places it where Tauri's `externalBin` expects it:
// apps/desktop/src-tauri/binaries/switchyardd-<target triple>.
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

const built = [];
for (const target of targets) {
  const cargoArgs = ["build", "-p", "sy-daemon", "--bin", "switchyardd", "--target", target];
  if (release) cargoArgs.push("--release");
  if (devtools) cargoArgs.push("--features", "devtools");
  run("cargo", cargoArgs);
  const src = join(root, "target", target, release ? "release" : "debug", "switchyardd");
  const dest = join(outDir, `switchyardd-${target}`);
  copyFileSync(src, dest);
  chmodSync(dest, 0o755);
  built.push(dest);
}

if (universal) {
  const dest = join(outDir, "switchyardd-universal-apple-darwin");
  run("lipo", ["-create", "-output", dest, ...built]);
  built.push(dest);
}

console.log(`switchyardd (${release ? "release" : "debug"}${devtools ? ", devtools" : ""}):`);
for (const path of built) console.log(`  ${path}`);
