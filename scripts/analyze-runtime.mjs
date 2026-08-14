import { readFileSync, writeFileSync, existsSync, mkdirSync, rmSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const RUNTIME = "D:/work/dsh-desktop/runtime";
const NM = join(RUNTIME, "node_modules");
const SCOPE = join(NM, "@deepseek-ai");

const readManifest = (dir) => JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
const depsOf = (dir) => {
  const m = readManifest(dir);
  return [...Object.keys(m.dependencies ?? {}), ...Object.keys(m.peerDependencies ?? {})];
};
const readdirSafe = (d) => { try { return readdirSync(d); } catch { return []; } };
const statSafe = (p) => { try { return statSync(p); } catch { return null; } };

const launcher = readManifest(join(SCOPE, "dsh"));
const patchNames = new Set();
for (const pkg of ["dsh-base", "dsh-web-app"]) {
  const patch = readFileSync(join(SCOPE, pkg, "cordis.patch.yml"), "utf8");
  for (const m of patch.matchAll(/name:\s*['"]?([^'"\s]+)['"]?/g)) patchNames.add(m[1]);
}

const roots = new Set([
  ...depsOf(join(SCOPE, "dsh")),
  ...depsOf(join(SCOPE, "dsh-base")),
  ...depsOf(join(SCOPE, "dsh-web-app")),
  ...patchNames,
  "@deepseek-ai/dsh-host-webserver",
  "@deepseek-ai/dsh-host-frontend-static",
  "@deepseek-ai/dsh-host-apiproxy",
  "@deepseek-ai/dsh-web-frontend",
  "@deepseek-ai/dsh-web-app",
  "@deepseek-ai/dsh-base",
  "@deepseek-ai/dsh",
]);

const pin = {};
for (const name of roots) {
  const dir = join(NM, ...name.split("/"));
  if (existsSync(join(dir, "package.json"))) pin[name] = "^" + readManifest(dir).version;
  else console.log("MISSING IN TREE:", name);
}
console.log("root count:", Object.keys(pin).length);

const out = join("D:/work/dsh-desktop/.prune-roots");
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
writeFileSync(join(out, "package.json"), JSON.stringify({ name: "dsh-min-roots", private: true, dependencies: pin }, null, 2));
console.log("roots written to", out);

// size breakdown
const sizes = [];
const walk = (dir) => {
  for (const e of readdirSafe(dir)) {
    const p = join(dir, e);
    if (e === "node_modules") continue;
    const st = statSafe(p);
    if (!st) continue;
    if (st.isDirectory()) walk(p);
    else sizes.push([p, st.size]);
  }
};
walk(NM);
const byTop = new Map();
for (const [p, s] of sizes) {
  const rel = p.slice(NM.length + 1).replaceAll("\\", "/");
  const parts = rel.split("/");
  const key = parts[0].startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
  byTop.set(key, (byTop.get(key) ?? 0) + s);
}
const top = [...byTop.entries()].sort((a, b) => b[1] - a[1]).slice(0, 30);
console.log("total bytes:", (sizes.reduce((a, [, s]) => a + s, 0) / 1048576).toFixed(1), "MB");
for (const [k, s] of top) console.log((s / 1048576).toFixed(1).padStart(8), "MB", k);
