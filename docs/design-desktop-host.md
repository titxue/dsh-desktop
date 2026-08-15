# dsh-desktop 插件化 + 系统托盘 — 设计方案

> 目标：把 dsh-desktop 从「Rust 壳包着整个框架」改造成「**插件化的发行版**」——业务逻辑下沉为
> 真正的 DSH 插件（apply(ctx) + 可撤销），Rust 壳瘦身为原生表面（窗口/托盘/进程），并新增
> 完整的系统托盘体验。

---

## 1. 设计原则

| # | 原则 | 含义 |
|---|------|------|
| P1 | **逻辑进插件，壳只做原生渲染** | 状态、进度、菜单模型、生命周期决策由插件产出；窗口、托盘图标、通知、进程由壳渲染 |
| P2 | **单一事实来源** | 服务状态/进度/菜单只有插件一份定义，壳只是它的显示器，杜绝 Rust/Node 双份状态 |
| P3 | **桥可降级** | 插件在无壳环境（纯 dsh web）下静默跳过桥逻辑，发行版插件与普通插件完全兼容 |
| P4 | **最小新依赖** | 桥用本地 IPC 套接字（Node net + Rust std，零新依赖）；托盘用 Tauri v2 内置 API；通知/单实例/自启用官方插件 |
| P5 | **引导流程不变** | Node 下载 + npm install 存在鸡生蛋问题，必须留在壳内，其进度仍由壳直接推给加载页 |

## 2. 架构总览

```
┌─────────────────────────────────────────────────────┐
│ Tauri 壳 (Rust, 瘦身)                                │
│  · WebView2 窗口           · 托盘图标/菜单渲染         │
│  · Node 引导(下载/安装,不变) · 子进程管理(spawn/kill)    │
│  · 桥客户端(std::fs)       · 通知/单实例/自启           │
└───────────────┬─────────────────────────────────────┘
                │ 本地 IPC 套接字（随平台自动切换，见 §3）
                │  Windows: \\.\pipe\dsh-desktop-<token>
                │  macOS/Linux: <tmpdir>/dsh-desktop-<token>.sock (AF_UNIX)
                │  ▲ 事件流: progress/state/menu/notification
                │  ▼ 命令:   menu-click/nav-result/shutdown
┌───────────────┴─────────────────────────────────────┐
│ dsh 进程 (Node)  dsh --profile web --patch desktop.yml│
│  ├─ dsh-base / dsh-web-app / …(现有几十个插件)        │
│  └─ @titxue/dsh-desktop-host ★ 新插件 (apply(ctx))     │
│      · 桥服务(IPC)      · 生命周期/进度事件源         │
│      · 托盘菜单模型       · 优雅关闭                   │
└─────────────────────────────────────────────────────┘
```

**关键验证结论**（来自上游源码）：
- dsh --profile web 即 dsh web，profile 是一等公民；
- --patch <file> 可在内置 profile 上叠加自定义插件配置（可重复），**这是官方支持的扩展入口**；
- 仓库根目录的 composed-web.yml 就是这种 profile 的格式（插件 id/name/config 列表）。
  → 发行版只需随资源附带 desktop.yml（= web 全量 + desktop-host 一项），壳以
  --profile web --patch <绝对路径>/desktop.yml 启动。

**patch 合并语义（M1 已实测，cordis-plugin-include 源码 + dump-config 实证）**：
- 默认是**覆盖**：patch 条目按 id 匹配已有条目，未匹配报 `entry not found`；
- **追加新插件必须用 `insert:` 关键字**：`- insert: [{id, name, config}]` 把条目追加到
  profile 顶层（无 id 时）；带 id 的 insert 会把条目插进 group 条目的 config 数组；
- 验证命令：`dsh --profile web --patch <file> --dump-config`（打印合并结果，不启动服务）；
- **模块解析起点是 DSH_HOME/profiles/<profile>/ 目录**（非进程 cwd）——插件包需出现在
  profile 目录的 node_modules 解析链上（M4 组装要点）。

## 3. 桥协议 v1

**传输（主）：本地 IPC 套接字，随平台切换两种实现——不占用端口、无 HTTP 层、两边零新依赖**。

| 平台 | 插件（Node）服务端 | 壳（Rust）客户端 |
|------|-------------------|------------------|
| Windows | `net.createServer()` 监听 `\\.\pipe\dsh-desktop-<token>` | `std::fs::File::open(管道路径)`（std 映射 CreateFileW） |
| macOS/Linux | 同一个 `net.createServer()`，路径换为 `<os.tmpdir()>/dsh-desktop-<token>.sock`（AF_UNIX） | `std::os::unix::net::UnixStream::connect(路径)` |

- **Node 侧一份代码**：net 模块原生统一了命名管道与 unix socket，只是路径参数不同；
- **Rust 侧一个 trait**（`BridgeTransport`）+ 两个 ~40 行实现，`#[cfg(target_os)]` 选择，协议层零改动；
- **连接时序**：服务端未就绪 → Windows 报 ERROR_PIPE_BUSY / POSIX 报 ECONNREFUSED，统一指数退避重试（250ms→2s）；
- **认证**：32 字节随机 token 拼进管道名/socket 文件名——猜中名字即认证，无 header、无端口可扫；
  token 经环境变量 `DSH_DESKTOP_TOKEN` 传入子进程；
- **POSIX 细节**：监听前 unlink 残留 socket 文件（上次异常退出的遗留）；监听后 chmod 0600；
- **消息格式**：新行分隔 JSON（`\n` 结尾），双向同构，与 HTTP 版完全一致；即推即达，断线重连 = 重开句柄。

**传输（备）：HTTP 通道，仅开发调试用**——插件额外挂 `/-/desktop/*`（SSE 事件流 +
POST 命令），方便开发者在浏览器里单独调试插件；生产构建禁用。

| 方向 | 消息 | 说明 |
|------|------|------|
| 插件 → 壳 | progress / state / menu / notification / log | 事件流，逐行 JSON |
| 壳 → 插件 | menu-click / nav-result / window-event / shutdown-request | 命令，逐行 JSON |

**事件流消息（插件→壳）**：

```jsonc
{ "type": "progress", "phase": "install-deps", "pct": 42, "label": "正在安装依赖…", "detail": "128/190 个包" }
{ "type": "state",    "phase": "ready", "port": 3080, "detail": "" }
// phase: booting | ready | error
{ "type": "menu", "items": [ /* 见 §5 菜单模型 */ ] }
{ "type": "notification", "title": "DeepSeek Harness 已就绪", "body": "点击托盘图标打开", "level": "info" }
{ "type": "log", "line": "[desktop-host] bridge up on port 3080" }
```

**命令消息（壳→插件）**：

```jsonc
{ "type": "menu-click", "id": "restart" }
{ "type": "nav-result", "ok": true }
{ "type": "window-event", "event": "shown" | "hidden" | "closed" }
{ "type": "shutdown-request", "graceful": true }
```

## 4. dsh-desktop-host 插件

```
plugins/desktop-host/
  package.json          name: @titxue/dsh-desktop-host  (发布到 npm)
  desktop.yml           组合叠加层：insert desktop-host 条目（随安装包发布）
  src/index.ts          export const name/inject; export function apply(ctx)
  src/bridge.ts         本地 IPC 桥（Windows 管道 / POSIX unix socket，HTTP 调试通道可选）
  src/lifecycle.ts      就绪检测（await ctx.webServer → 实际端口）
  src/menu.ts           托盘菜单模型（M2）
  src/progress.ts       引导进度转发（M2）
```

**M2 实现偏差说明**：托盘菜单在**壳侧构建**（菜单项多为壳本地动作：显示/打开目录/退出），
  插件侧仅通过 state 事件驱动状态行与图标；设计中的"插件下发完整菜单模型"（menu 事件）暂缓——
  当需要插件自定义菜单项时再启用（协议已预留 menu 类型）。

**cordis 4 的两个关键约束（M1 实测踩坑）**：
- 访问服务必须声明依赖：`export const inject = ["webServer"]`（否则
  `cannot get property "webServer" without inject`）；依赖在组合层面注入，
  无 webServer 的组合（tui）应移除该行；
- **状态重放**：客户端（壳）可能在事件发出后才连接/重连，桥须在
  `onClientConnect` 时补发最近一次 state（已实现）。

apply(ctx) 核心（**完全符合 dshfind 教程定义**）：

```ts
export const name = 'dsh-desktop-host'

export function apply(ctx: Context) {
  if (!process.env.DSH_DESKTOP_TOKEN) {
    ctx.logger.info('[desktop-host] 未检测到桌面桥环境，跳过（纯 web 模式）')
    return  // P3: 无壳降级
  }
  ctx.effect(() => {
    const bridge = new DesktopBridge(ctx, process.env.DSH_DESKTOP_TOKEN!)
    bridge.start()                       // 挂载 /-/desktop/*
    const offs = [
      ctx.on('webStartup/ready', ...),   // → state ready + 通知
      ctx.on('webStartup/error', ...),   // → state error
      ctx.on('bootstrap/progress', ...), // → progress 事件
    ]
    return () => {                       // 可撤销：卸载时自动关桥、退订阅
      offs.forEach(f => f())
      bridge.stop()
    }
  })
}
```

要点：
- **状态机**：booting → ready | error，由 ctx 事件驱动（webStartup 等），不做轮询；
- **进度归一**：壳侧 npm/Node 阶段的进度由壳直推加载页（P5），服务启动后的进度由插件经桥推送，
  加载页脚本不变（都走 window.updateProgress）；
- **优雅关闭**：收到 shutdown-request → 触发 ctx 清理（等价卸载，所有注册自动撤销）→ 回执后退出；
- **测试性**：插件逻辑与壳完全解耦，可 dsh --profile web --patch desktop.yml 在浏览器里单独开发调试。

## 5. 托盘设计

**图标状态机**（壳渲染，插件驱动 tray:set-icon）：

```
引导中(下载/安装) ──► 已就绪 ──► 错误(红点)
      ▲                  │
      └──── 重启 ────────┘
```

**菜单模型**（插件下发 menu 事件，壳渲染 Tauri Menu；点击回传 menu-click）：

| 菜单项 | target | 动作 |
|--------|--------|------|
| 显示主窗口 | shell | 壳 show + focus |
| 状态：● 已就绪 · 端口 3080 | plugin | disabled 动态行（由 state 事件生成） |
| ─── | — | 分隔符 |
| 打开主界面 | shell | show + navigate 到服务地址 |
| 重新启动服务 | plugin | 壳 kill 子进程 → 重新 spawn（bootstrap 缓存命中，秒级恢复） |
| 打开数据目录 / 打开日志目录 | shell | 壳开 explorer（路径由插件在菜单模型里给出） |
| ─── | — | 分隔符 |
| 退出 | shell | 优雅关闭序列（§6） |

**交互行为**：
- **左键单击**：显示/聚焦窗口；**双击**：同上（兜底）；
- **关闭按钮**：默认最小化到托盘（隐藏窗口，首次隐藏弹通知提示），设置可关闭（直接退出）；
- **通知**：首次就绪、引导失败、服务器崩溃（tauri-plugin-notification）；
- **单实例**：二次启动聚焦已有窗口（tauri-plugin-single-instance）——托盘应用标配；
- **开机自启**：tauri-plugin-autostart（HKCU Run），设置项控制；
- **桥断降级**：事件流断开时菜单只剩 shell 项（显示/退出），图标置灰，状态行显示连接中…。

## 6. 窗口与生命周期

| 场景 | 行为 |
|------|------|
| 首次启动 | 壳：Node 引导（进度直推加载页）→ spawn dsh --profile web --patch desktop.yml → 等待插件 ready 事件 → 导航窗口（**替代**现有 wait_for_port 轮询，轮询保留为兜底） |
| 关闭窗口 | ExitRequested + 最小化到托盘开启 → prevent_close + hide；否则正常退出 |
| 托盘退出 | ① POST shutdown-request（插件优雅清理）→ ② 等 ≤3s → ③ taskkill /T /F 兜底 → ④ app.exit |
| 托盘重启 | kill 子进程树 → 重新 spawn（跳过引导，缓存命中） |
| 服务器崩溃 | 插件 state=error → 托盘红点 + 通知 + 菜单重新启动高亮 |

## 7. 设置项（DSH settings，插件侧持有，与 CLI 共享）

```yaml
desktop:
  minimizeToTray: true     # 关闭按钮行为
  notifyOnReady: true      # 就绪通知
  launchAtLogin: false     # 壳经桥读取并应用
```

壳只缓存最小必要值（minimizeToTray 需在桥就绪前生效 → 默认 true，桥通后同步）。

## 8. 安全

- token 32B 随机、仅经 env 传递，不入命令行参数（避免进程列表泄露）；
- 管道名 = dsh-desktop-<token>：无固定名字可枚举，仅本地进程可连；
- 桥只接受白名单消息类型；托盘菜单中打开数据目录等路径由插件下发 → 壳校验为 %LOCALAPPDATA% 前缀后打开；
- HTTP 调试通道仅在开发构建启用，生产禁用。

## 9. Rust 壳改动清单

| 动作 | 内容 |
|------|------|
| 删 | wait_for_port 轮询（保留兜底）；壳内启动参数拼装改为 profile+patch 形式 |
| 改 | spawn 命令：node bin.js --profile web --patch <desktop.yml>；env 注入 token |
| 增 | 桥客户端线程（SSE 消费 + 命令发送）；托盘创建/图标切换/菜单重建；通知插件；单实例插件；自启插件；重启流程 |
| 依赖 | 已有 ureq；新增 tauri-plugin-notification、tauri-plugin-single-instance、tauri-plugin-autostart（均为官方） |

## 10. 实施计划

| 里程碑 | 内容 | 验收标准 |
|--------|------|----------|
| **M1 桥 + 就绪事件** ✅ 已完成 | 插件骨架（apply/ctx.effect/桥服务）；desktop.yml insert 语义验证；壳侧桥客户端（BridgeClient）+ lib.rs 接线（ready 驱动导航，wait_for_port 兜底）；状态重放 | 实测通过：`dsh --profile web --patch desktop.yml` 加载插件 → 桥 up → `state: ready {host, port}` 经命名管道送达 Rust 客户端（含重连重放）；纯 web 模式（无 token）不受影响 |
| **M2 托盘** ✅ 已完成 | 图标 4 态（tray-idle/ready/error/off，scripts/gen-tray-icons.mjs 生成）；壳侧菜单 + 状态行动态更新；关闭按钮 → 最小化到托盘；托盘退出 → 优雅退出；重启服务；系统通知（tauri-plugin-notification） | 编译通过；菜单项 show/open/restart/data-dir/log-dir/quit + 左键单击显示窗口；bridge 事件驱动图标与状态行 |
| **M3 生命周期增强** ✅ 已完成 | 重启服务（M2）；**优雅关闭**：托盘退出 → shutdown-request（插件回执后 process.exit，壳 2s 兜底强杀）；**单实例**（tauri-plugin-single-instance，二次启动聚焦+通知）；**开机自启**（tauri-plugin-autostart，托盘复选开关）；**设置项**：最小化到托盘开关（本地 settings.json，托盘复选） | 编译通过；退出序列：通知插件 → 2s 等待 → kill_tree 兜底；DSH settings（插件侧）集成留待 M4 |
| **M4 发布** | 插件发布 npm；bootstrap/package.json 加入固定版本；setup-runtime.ts 组装支持；**CI 三平台构建矩阵**（Windows NSIS / macOS DMG / Linux deb+AppImage）；node-manifest 多平台 SHA-256；README 更新 | 三平台安装包各自首启成功、托盘全功能；**仓库含真正插件 → dsh-plugin topic 名副其实** |

## 11. 备选方案对比

| 方案 | 优点 | 缺点 | 结论 |
|------|------|------|------|
| A. 托盘全做在 Rust，不插件化 | 实现最快（半天） | 状态双份：Rust 猜服务状态，与插件世界脱节；不解决发行版不是插件的问题 | 否决 |
| B. 本方案（插件 + 桥 + 托盘） | 单一事实来源、可测试、可降级、dsh-plugin 名副其实、未来可做 CLI/服务端发行版复用 | 多一层桥协议，M1-M2 约一周 | **选定** |
| C. 桥走 stdio（stdin/stdout） | 无 HTTP 层 | dsh web 自身的 stdout（URL 打印、日志）会污染消息流，劫持脆弱；fd>2 在 Windows 子进程继承不可靠 | 否决 |
| D. 本地 IPC 套接字（本方案传输层）：Windows 管道 + POSIX unix socket | 零新依赖（Node net 原生统一 + Rust std 双实现）、无端口占用、即推即达、名字即认证、**天然跨平台** | 需写两个 ~40 行传输实现 | **选定** |

## 12. 风险与预案

| 风险 | 预案 |
|------|------|
| patch 合并语义与预期不符（追加 vs 覆盖） | M1 首日验证；若追加不支持，改由 setup-runtime.ts 在组装期把插件 id 直接注入 shipped profile |
| IPC 连接时序（服务端未就绪） | ERROR_PIPE_BUSY / ECONNREFUSED 统一指数退避重试；壳在 spawn 子进程后延迟 1s 再首连 |
| 三平台行为差异（Node 归档、打包目标、路径、系统工具） | 见 §13 跨平台清单；CI 三平台构建矩阵兜底（§10 M4） |
| Tauri 托盘动态菜单在 WebView2 上的刷新问题 | M2 先做最小集（图标 + 静态菜单 + 动态状态行），验证后扩 |
| npm 发布需要账号/权限 | 用 titxue 账号；或先仓库内发布 + 组装期复制进闭包（setup-runtime.ts 已支持此模式） |

---

## 13. 跨平台清单（Windows / macOS / Linux）

| 关注点 | 现状 | 方案 |
|--------|------|------|
| **IPC 传输** | 仅管道 | §3 双实现：Windows 管道 + POSIX unix socket，Node 侧零分支 |
| **Node 下载归档** | `node-v22.22.3-win-x64.zip` | 三平台归档：win-x64.zip / darwin-arm64(+x64).tar.gz / linux-x64.tar.xz；`node-manifest.json` 改为 `platforms: { win32-x64, darwin-arm64, darwin-x64, linux-x64: { urls[], sha256 } }`，setup-runtime.ts 逐个下载计算 SHA-256 |
| **归档解压** | Rust zip crate | Windows 沿用 zip；macOS/Linux 调系统 `tar`（-xzf / -xJf，macOS/Linux 自带）——零新 crate |
| **壳的 Windows 专属代码** | `CommandExt::creation_flags`（CREATE_NO_WINDOW）、`taskkill /T /F`、`win_clean`（\?\ 前缀） | `#[cfg(target_os)]` 拆分：macOS/Linux 用 `kill` + 进程组；`win_clean` 仅 Windows |
| **路径** | `%LOCALAPPDATA%` 直写 | 全部走 `app.path()`（Tauri 已按平台返回数据/日志目录），托盘菜单路径校验前缀随平台调整 |
| **打包目标** | NSIS | tauri 按平台构建：NSIS (win) / DMG+app (macOS) / deb+AppImage (Linux)；CI 用 `--bundles` 按 OS 指定 |
| **通知/单实例/自启** | 未引入 | tauri-plugin-notification / single-instance / autostart 均三平台支持 |
| **托盘** | Tauri v2 内置 | 三平台同一套 API；macOS 注意 menu bar 图标与 `set_as_app_icon` 差异，Linux 注意 AppIndicator 环境 |
| **CI** | 无 | GitHub Actions 矩阵：windows-latest / macos-latest / ubuntu-latest；每平台跑 build + 冒烟（启动→桥连接→ready） |
| **签名/公证** | 无 | Windows 代码签名、macOS 公证为发布后事项（CI 预留 secret 位）；Linux 无要求 |
| **服务端兼容** | 未验证 | M1 在 macOS/Linux 上冒烟 `dsh --profile web`（Node 全平台，预期可用，仍需实测） |