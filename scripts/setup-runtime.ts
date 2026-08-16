/**
 * Assemble the first-run bootstrap resources (default) or the full bundled
 * runtime (--bundle) for the DeepSeek Harness desktop app.
 *
 * Default (dynamic-download mode):
 *   bootstrap/package.json          pinned dependency roots
 *   bootstrap/package-lock.json     locked closure
 *   bootstrap/node-manifest.json    Node version + mirror URLs + SHA-256
 *   bootstrap/install-deps.mjs      standalone installer script
 *
 * --bundle (offline variant):
 *   runtime/node.exe + runtime/node_modules/  full server closure
 *
 * Sources (env overrides):
 *   DSH_RUNTIME_NODE_MODULES  — node_modules tree to derive/install from
 *   DSH_RUNTIME_NODE_EXE      — node.exe to copy (bundle mode)
 */
import { cpSync, existsSync, mkdirSync, readdirSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const NODE_VERSION = "v22.22.3";
const NODE_ZIP = `https://npmmirror.com/mirrors/node/${NODE_VERSION}/node-${NODE_VERSION}-win-x64.zip`;
const NODE_ZIP_FALLBACK = `https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-win-x64.zip`;

const bundleMode = process.argv.includes("--bundle");
const nmSrc = process.env.DSH_RUNTIME_NODE_MODULES;
const nodeSrc = process.env.DSH_RUNTIME_NODE_EXE;

async function download(url, dest) {
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`download failed: ${url} -> ${resp.status}`);
  writeFileSync(dest, Buffer.from(await resp.arrayBuffer()));
}

async function sha256(path) {
  const data = readFileSync(path);
  const buf = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

if (bundleMode) {
  // ---- offline variant: full runtime payload ----
  const runtime = join(root, "runtime");
  mkdirSync(runtime, { recursive: true });
  if (nodeSrc) {
    cpSync(nodeSrc, join(runtime, "node.exe"));
  } else {
    const zip = join(root, ".tmp-node.zip");
    try { await download(NODE_ZIP, zip); } catch { await download(NODE_ZIP_FALLBACK, zip); }
    execFileSync("powershell", ["-NoProfile", "-Command",
      `Expand-Archive -Force '${zip}' '${runtime}' -DestinationPath '${runtime}'`]);
    rmSync(zip);
  }
  if (nmSrc) {
    rmSync(join(runtime, "node_modules"), { recursive: true, force: true });
    cpSync(nmSrc, join(runtime, "node_modules"), { recursive: true });
  } else {
    throw new Error("bundle mode needs DSH_RUNTIME_NODE_MODULES pointing at a node_modules tree");
  }
  console.log("runtime ready at", runtime);
  return;
}

/** 构建发行版插件并复制进 bootstrap/plugin（供壳复制进 profile）。 */
function buildDesktopPlugin(bootstrap) {
  const pluginDir = join(root, "plugins", "desktop-host");
  const pkgPath = join(pluginDir, "package.json");
  if (!existsSync(pkgPath)) {
    console.log("plugins/desktop-host 不存在，跳过发行版插件");
    return;
  }
  // 构建产物（lib/）：已有则复用，否则尝试 tsc
  const lib = join(pluginDir, "lib");
  const hasTsc = existsSync(join(pluginDir, "node_modules", ".bin", "tsc" + (process.platform === "win32" ? ".cmd" : "")));
  if (!existsSync(join(lib, "index.js"))) {
    if (!hasTsc) throw new Error("需要先构建插件：cd plugins/desktop-host && npm i && npx tsc");
    execFileSync(hasTsc ? join(pluginDir, "node_modules", ".bin", "tsc" + (process.platform === "win32" ? ".cmd" : "")) : "tsc",
      ["-p", join(pluginDir, "tsconfig.json")], { stdio: "inherit" });
  }
  const out = join(bootstrap, "plugin", "@titxue", "dsh-desktop-host");
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  cpSync(pkgPath, join(out, "package.json"));
  cpSync(lib, join(out, "lib"), { recursive: true });
  console.log("wrote bootstrap/plugin/@titxue/dsh-desktop-host");
  // desktop.yml 叠加层
  cpSync(join(pluginDir, "desktop.yml"), join(bootstrap, "desktop.yml"));
  console.log("wrote bootstrap/desktop.yml");
}

// ---- dynamic mode: bootstrap resources ----
const bootstrap = join(root, "bootstrap");
mkdirSync(bootstrap, { recursive: true });

// 0. registry default: npmmirror (fast in CN); DSH_NPM_REGISTRY overrides at runtime.
writeFileSync(join(bootstrap, ".npmrc"), "registry=https://registry.npmmirror.com\n");
console.log("wrote bootstrap/.npmrc (registry=https://registry.npmmirror.com)");

// 1. pinned manifests: derive from an existing installed closure when given.
if (nmSrc) {
  const launcher = JSON.parse(readFileSync(join(nmSrc, "@deepseek-ai", "dsh", "package.json"), "utf8"));
  const base = JSON.parse(readFileSync(join(nmSrc, "@deepseek-ai", "dsh-base", "package.json"), "utf8"));
  const webApp = JSON.parse(readFileSync(join(nmSrc, "@deepseek-ai", "dsh-web-app", "package.json"), "utf8"));
  const deps = (m) => ({ ...(m.dependencies ?? {}), ...(m.peerDependencies ?? {}) });
  const patchNames = new Set();
  for (const pkg of ["dsh-base", "dsh-web-app"]) {
    const patch = readFileSync(join(nmSrc, "@deepseek-ai", pkg, "cordis.patch.yml"), "utf8");
    for (const m of patch.matchAll(/name:\s*['"]?([^'"\s]+)['"]?/g)) {
      if (!m[1].includes("/")) patchNames.add(m[1]);
    }
  }
  const pin = { ...deps(launcher), ...deps(base), ...deps(webApp) };
  for (const name of patchNames) {
    const dir = join(nmSrc, ...name.split("/"));
    if (existsSync(join(dir, "package.json"))) {
      pin[name] = "^" + JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).version;
    }
  }
  for (const name of [
    "@deepseek-ai/dsh-host-webserver",
    "@deepseek-ai/dsh-host-frontend-static",
    "@deepseek-ai/dsh-host-apiproxy",
    "@deepseek-ai/dsh-web-frontend",
  ]) {
    const dir = join(nmSrc, ...name.split("/"));
    if (existsSync(join(dir, "package.json"))) pin[name] = "^" + JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).version;
  }
  const pinned = Object.fromEntries(Object.entries(pin).sort(([a], [b]) => a.localeCompare(b)));
  writeFileSync(join(bootstrap, "package.json"), JSON.stringify({ name: "dsh-desktop-runtime", private: true, dependencies: pinned }, null, 2) + "
");
  console.log("wrote bootstrap/package.json with", Object.keys(pinned).length, "roots");
} else {
  throw new Error("dynamic mode needs DSH_RUNTIME_NODE_MODULES pointing at an installed closure (to pin versions)");
}

// 2. lockfile: generate from the pinned roots (needs network for resolution metadata).
const lockDir = join(root, ".tmp-lock");
rmSync(lockDir, { recursive: true, force: true });
mkdirSync(lockDir);
cpSync(join(bootstrap, "package.json"), join(lockDir, "package.json"));
execFileSync("npm", ["install", "--package-lock-only", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"], { cwd: lockDir, stdio: "inherit" });
cpSync(join(lockDir, "package-lock.json"), join(bootstrap, "package-lock.json"));
rmSync(lockDir, { recursive: true, force: true });
console.log("wrote bootstrap/package-lock.json");

// 3. node manifest: download node zip once to compute the SHA-256.
const zip = join(root, ".tmp-node.zip");
try { await download(NODE_ZIP, zip); } catch (e) { try { await download(NODE_ZIP_FALLBACK, zip); } catch { throw new Error(`node download failed: ${e}`); } }
const nodeSha = await sha256(zip);
rmSync(zip);
writeFileSync(join(bootstrap, "node-manifest.json"), JSON.stringify({
  version: NODE_VERSION,
  urls: [NODE_ZIP, NODE_ZIP_FALLBACK],
  sha256: nodeSha,
}, null, 2) + "
");
console.log("wrote bootstrap/node-manifest.json (sha256", nodeSha, ")");
console.log("bootstrap ready at", bootstrap);