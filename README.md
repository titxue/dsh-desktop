# DeepSeek Harness Desktop（bun + Tauri）

[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)——面向编码智能体的 Agent 运行框架——的 Windows 原生桌面版封装。

## 架构

- **外壳（Shell）**：基于 **Tauri v2** 的原生窗口（WebView2）。首次运行时自动引导安装 Harness，显示加载页，就绪后跳转到 `http://127.0.0.1:<port>`，退出时自动关闭服务进程。
- **动态运行时（默认）**：安装包**不携带 Node.js 和 node_modules**（安装包仅约 2.5 MB）。首次启动时应用会：
  1. 下载固定版本 Node.js `v22.22.3` win-x64 压缩包（多镜像源，SHA-256 校验）；
  2. 使用固定的 `package.json` + `package-lock.json` 执行内置的 `npm install`，安装到 `%LOCALAPPDATA%\ai.deepseek.harness.desktop\deps`；
  3. 从该依赖闭包启动 `dsh web` 并加载界面。
  之后启动直接使用缓存，几秒内完成。服务端必须运行在 Node 下——Bun 缺少 Harness 依赖的 `node:module.stripTypeScriptTypes`。
- **工具链**：除 Rust 编译外，一切均由 **Bun** 驱动——`bun install`、`bunx tauri` 以及 bootstrap 组装脚本。

## 目录结构

```
dsh-desktop/
  package.json                bun 项目（devDep: @tauri-apps/cli）
  assets/                     应用图标源文件（svg/png）
  ui/index.html               服务启动期间的加载页
  bootstrap/                  随安装包发布的轻量首次运行资源
    package.json              固定版本的依赖根（dsh + web profile 闭包）
    package-lock.json         锁定闭包（可复现的 npm install）
    node-manifest.json        Node 版本、镜像地址、SHA-256
    install-deps.mjs          独立的备用安装脚本
  src-tauri/                  Tauri v2 外壳（Rust）
  scripts/setup-runtime.ts    通过 bun 组装 bootstrap/（见下文）
```

## 构建

```sh
bun install                       # 安装 @tauri-apps/cli
bun scripts/setup-runtime.ts      # 组装 bootstrap/（环境变量覆盖见下文）
bunx tauri build                  # 编译 Rust 并打包 NSIS 安装程序
```

产物输出到 `src-tauri/target/release/bundle/nsis/`：
`DeepSeek Harness_0.1.0_x64-setup.exe`（约 2.5 MB）。

## setup-runtime.ts

- **默认模式**：复用 `DSH_RUNTIME_NODE_MODULES` 指向的已安装闭包，生成固定的 `package.json` + `package-lock.json`（可离线工作），并下载一次 Node 压缩包计算 `node-manifest.json` 的 SHA-256。
- **回退模式**：未设置 `DSH_RUNTIME_NODE_MODULES` 时，通过 npm 从 registry 拉取依赖闭包，从 nodejs.org / npmmirror 下载 Node 压缩包。

## 运行时行为与配置项

- 服务端绑定**随机空闲端口**，绝不会与其他 dsh 实例冲突。
- 会话/设置存放在标准的 `$DSH_HOME`（`~/.dsh`），与 `dsh` CLI 共享。
- 首次运行需下载约 35 MB（Node）+ 190 MB（依赖）。下载源默认使用 **npmmirror** 镜像（国内访问友好；Node 压缩包优先 npmmirror，nodejs.org 兜底；npm 依赖通过 `bootstrap/.npmrc` 走 `registry.npmmirror.com`）。
  - `DSH_NPM_REGISTRY` —— 覆盖 npm registry（以 `--registry` 传入）
- **首次运行进度**：加载页显示实时进度条——Node 下载阶段按实际字节百分比精确推进；npm 阶段按预估 90 秒时间线推进（慢网络自动延长），详情行显示当前下载的包名；窗口标题同步显示状态。
- 诊断日志：`%LOCALAPPDATA%\ai.deepseek.harness.desktop\logs\`（desktop.log、npm.out/err.log、server.out/err.log）。

## 离线变体（打包 node_modules）

编辑 `src-tauri/tauri.conf.json`，将 `bundle.resources` 改为 `["../runtime"]`，然后执行 `bun scripts/setup-runtime.ts --bundle` 组装完整闭包（`node_modules` + `node.exe`）。外壳会从 exe 同级目录或 NSIS 的 `_up_` 暂存目录查找 `runtime`。安装包体积增至约 45 MB，但可完全离线使用。
