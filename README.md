# DeepSeek Harness Desktop (bun + Tauri)

DeepSeek Harness — the agent harness for coding agents (github.com/deepseek-ai/deepseek-harness) —
packaged as a native Windows desktop application.

## Architecture

- **Shell**: a **Tauri v2** native window (WebView2) that bootstraps the harness on first
  run, shows a loading page, then navigates to `http://127.0.0.1:<port>` and kills the
  server on exit.
- **Dynamic runtime (default)**: the installer ships **no Node.js and no node_modules**
  (~2.5 MB installer). On first launch the app:
  1. downloads the pinned Node.js `v22.22.3` win-x64 zip (mirror list, SHA-256 verified),
  2. runs the bundled `npm install` with a pinned `package.json` + `package-lock.json`
     into `%LOCALAPPDATA%\ai.deepseek.harness.desktop\deps`,
  3. starts `dsh web` from that closure and loads the UI.
  Subsequent launches use the cached runtime and start in seconds. The server must run
  under Node — Bun lacks `node:module.stripTypeScriptTypes`, which the harness imports.
- **Toolchain**: everything except the Rust compilation is driven by **Bun** — `bun install`,
  `bunx tauri`, and the bootstrap-assembly script.

## Layout

```
dsh-desktop/
  package.json                bun project (devDep: @tauri-apps/cli)
  assets/                     app icon sources (svg/png)
  ui/index.html               loading page shown while the server boots
  bootstrap/                  tiny first-run resources shipped in the installer
    package.json              pinned dependency roots (dsh + web profile closure)
    package-lock.json         locked closure (reproducible npm install)
    node-manifest.json        Node version, mirror URLs, SHA-256
    install-deps.mjs          standalone fallback installer script
  src-tauri/                  Tauri v2 shell (Rust)
  scripts/setup-runtime.ts    assembles bootstrap/ via bun (see below)
```

## Build

```sh
bun install                       # install @tauri-apps/cli
bun scripts/setup-runtime.ts      # assemble bootstrap/ (env overrides below)
bunx tauri build                  # compile Rust + bundle NSIS installer
```

Artifacts land in `src-tauri/target/release/bundle/nsis/`:
`DeepSeek Harness_0.1.0_x64-setup.exe` (~2.5 MB).

## setup-runtime.ts

- Default: reuses the pre-installed closure at `DSH_RUNTIME_NODE_MODULES` to write the
  pinned `package.json` + `package-lock.json` (works offline), and downloads the Node zip
  once to compute `node-manifest.json`'s SHA-256.
- Fallback: if `DSH_RUNTIME_NODE_MODULES` is unset, `npm install` fetches the closure from
  the registry and the Node zip from nodejs.org / npmmirror.

## Runtime behavior & knobs

- Server binds a **random free port**, so it never collides with another dsh instance.
- Sessions/settings live in the standard `$DSH_HOME` (`~/.dsh`), shared with the `dsh` CLI.
- First run downloads ~35 MB (Node) + ~190 MB (deps). Sources default to the
  **npmmirror** mirrors for mainland-China access (Node zip: npmmirror first,
  nodejs.org fallback; npm deps: `registry.npmmirror.com` via `bootstrap/.npmrc`).
  Overrides:
  - `DSH_NPM_REGISTRY` — npm registry override (passed as `--registry`)
- Diagnostic logs: `%LOCALAPPDATA%\ai.deepseek.harness.desktop\logs\` (desktop.log,
  npm.out/err.log, server.out/err.log).

## Bundling node_modules instead (offline-first variant)

Edit `src-tauri/tauri.conf.json`: `bundle.resources` → `["../runtime"]`, then assemble the
full closure (`node_modules` + `node.exe`) with `bun scripts/setup-runtime.ts --bundle`.
The shell finds `runtime` next to the exe or under the NSIS `_up_` staging dir. Installer
grows to ~45 MB but works fully offline.