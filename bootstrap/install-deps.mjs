#!/usr/bin/env node
// Installs the pinned dsh dependency closure into <prefix> via npm.
// Usage: node install-deps.mjs <prefix> [registry]
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const prefix = process.argv[2];
if (!prefix) { console.error("usage: install-deps.mjs <prefix> [registry]"); process.exit(2); }
mkdirSync(prefix, { recursive: true });

const self = dirname(fileURLToPath(import.meta.url));
const npmCli = join(prefix, "node", "node_modules", "npm", "bin", "npm-cli.js");
const registry = process.argv[3] ?? process.env.DSH_NPM_REGISTRY;

if (!existsSync(join(prefix, "node_modules", "@deepseek-ai", "dsh"))) {
  const args = [npmCli, "install", "--prefix", prefix, "--omit=dev", "--no-audit", "--no-fund", "--loglevel=warn"];
  if (registry) args.push("--registry", registry);
  const res = spawnSync(join(prefix, "node", "node.exe"), args, { stdio: "inherit", env: process.env });
  if (res.status !== 0) { console.error("npm install failed with status", res.status); process.exit(res.status ?? 1); }
}
if (!existsSync(join(prefix, "node_modules", "@deepseek-ai", "dsh"))) {
  console.error("dependency install did not produce @deepseek-ai/dsh");
  process.exit(1);
}
console.log("dependencies ready");
