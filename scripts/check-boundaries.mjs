// Checks the crate dependency rules from docs/specs/0001-stack-architecture/index.md.
// Feature 2 wires this (or a cargo-deny equivalent) into CI.
//
//  - allow: the only direct normal dependencies a crate may have (workspace and external)
//  - forbid: crates that must never appear anywhere in its dependency tree
//  - no `cfg(target_os` / `cfg(unix|windows)` outside sy-infra, sy-client and src-tauri

import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const meta = JSON.parse(
  execFileSync("cargo", ["metadata", "--format-version", "1", "--locked"], {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 256 << 20,
  }),
);

const isAdapter = (name) => name.startsWith("sy-adapter-") && name !== "sy-adapter-api";
const TAURI = /^tauri($|-)/;
const STORAGE = ["rusqlite", "portable-pty", "keyring"];

const rules = {
  "sy-protocol": { allow: ["serde", "schemars", "serde_json"] },
  "sy-client": { forbid: ["sy-core", "sy-infra", "sy-daemon", TAURI, isAdapter] },
  "sy-adapter-api": { allow: ["serde", "tokio", "thiserror"] },
  "sy-core": {
    // spec 0002 adds uuid (typed ids) and async-trait (the `Store` port as `Arc<dyn Store>`)
    allow: ["sy-adapter-api", "serde", "tokio", "thiserror", "tracing", "uuid", "async-trait"],
    forbid: ["sy-infra", "sy-daemon", TAURI, isAdapter, ...STORAGE],
  },
  "sy-infra": { forbid: ["sy-daemon", TAURI, isAdapter] },
  "sy-daemon": { forbid: [TAURI] },
  "sy-devbridge": { forbid: ["sy-core", "sy-infra", "sy-daemon", TAURI, isAdapter] },
  switchyard: { forbid: ["sy-core", "sy-infra", "sy-daemon", isAdapter] },
};
const adapterRule = { allowWorkspace: ["sy-adapter-api"], forbid: ["sy-core", "sy-infra", "sy-daemon", TAURI] };

const pkgById = new Map(meta.packages.map((p) => [p.id, p]));
const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
const workspace = new Set(meta.workspace_members);
const normalDeps = (id) =>
  (nodes.get(id)?.deps ?? []).filter((d) => d.dep_kinds.some((k) => k.kind === null)).map((d) => d.pkg);

function closure(id) {
  const seen = new Set();
  const stack = [...normalDeps(id)];
  while (stack.length) {
    const next = stack.pop();
    if (seen.has(next)) continue;
    seen.add(next);
    stack.push(...normalDeps(next));
  }
  return [...seen].map((d) => pkgById.get(d).name);
}

const matches = (pattern, name) =>
  typeof pattern === "string" ? pattern === name : pattern instanceof RegExp ? pattern.test(name) : pattern(name);

const failures = [];
for (const id of workspace) {
  const name = pkgById.get(id).name;
  const rule = rules[name] ?? (isAdapter(name) ? adapterRule : null);
  if (!rule) continue;
  const direct = normalDeps(id).map((d) => pkgById.get(d));
  if (rule.allow) {
    for (const dep of direct)
      if (!rule.allow.includes(dep.name)) failures.push(`${name} may not depend on ${dep.name}`);
  }
  if (rule.allowWorkspace) {
    for (const dep of direct.filter((d) => workspace.has(d.id))) {
      if (!rule.allowWorkspace.includes(dep.name)) failures.push(`${name} may not depend on ${dep.name}`);
    }
  }
  for (const dep of closure(id)) {
    if ((rule.forbid ?? []).some((p) => matches(p, dep)))
      failures.push(`${name} must never depend on ${dep} (found in its tree)`);
  }
}

// OS specific code only in sy-infra, sy-client and src-tauri.
const osAllowed = ["crates/sy-infra", "crates/sy-client", "apps/desktop/src-tauri"];
const osPattern = /cfg\((target_os|unix|windows)|cfg\(not\((target_os|unix|windows)/;
function walk(dir) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) walk(path);
    else if (path.endsWith(".rs") && !path.includes("/tests/")) {
      const rel = relative(root, path);
      if (osAllowed.some((a) => rel.startsWith(a))) continue;
      readFileSync(path, "utf8")
        .split("\n")
        .forEach((line, i) => {
          if (osPattern.test(line))
            failures.push(`OS specific code outside sy-infra/sy-client/src-tauri: ${rel}:${i + 1}`);
        });
    }
  }
}
walk(join(root, "crates"));

if (failures.length) {
  console.error("Dependency rule violations:");
  for (const f of failures) console.error(`  ${f}`);
  process.exit(1);
}
console.log(`Dependency rules hold for ${workspace.size} workspace crates.`);
