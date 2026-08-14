/**
 * @titxue/dsh-desktop-host — DeepSeek Harness 桌面发行版宿主插件。
 *
 * 遵循 DSH 插件规范：导出 name + apply(ctx)，所有注册经 ctx.effect 可撤销。
 * 职责：
 *   1. 检测到 DSH_DESKTOP_TOKEN 时启动本地 IPC 桥（Windows 管道 / unix socket）；
 *   2. 把 web 服务就绪状态推给桌面壳（驱动窗口导航、托盘状态、通知）；
 *   3. 处理壳发来的命令（菜单点击、关机请求等）。
 *
 * 降级：未设置 DSH_DESKTOP_TOKEN（纯 dsh web / 浏览器模式）时静默跳过，
 * 不影响任何现有功能。
 */

import type { Context } from "@deepseek-ai/cordis";
import { DesktopBridgeServer, type BridgeMessage } from "./bridge.ts";
import { detectReady } from "./lifecycle.ts";

export const name = "dsh-desktop-host";

export function apply(ctx: Context) {
  const token = process.env.DSH_DESKTOP_TOKEN;
  if (!token) {
    ctx.logger.info("[dsh-desktop-host] DSH_DESKTOP_TOKEN 未设置，跳过桌面桥（纯 web 模式）");
    return;
  }

  ctx.effect(() => {
    const bridge = new DesktopBridgeServer(token, (command) => {
      void handleCommand(ctx, bridge, command);
    });

    void bridge.start().then((endpoint) => {
      ctx.logger.info(`[dsh-desktop-host] bridge up: ${endpoint}`);
      bridge.send({ type: "log", line: `bridge up: ${endpoint}` });

      // 服务就绪 → 通知壳导航窗口并刷新托盘状态
      void detectReady(ctx).then(
        ({ host, port }) => {
          bridge.send({ type: "state", phase: "ready", host, port, detail: "" });
          bridge.send({
            type: "notification",
            title: "DeepSeek Harness 已就绪",
            body: `http://${host}:${port}`,
            level: "info",
          });
        },
        (error: unknown) => {
          ctx.logger.error(`[dsh-desktop-host] ready detection failed: ${error}`);
          bridge.send({ type: "state", phase: "error", detail: String(error) });
        },
      );
    });

    // 可撤销：插件卸载（或宿主退出）时自动关闭桥
    return () => {
      void bridge.stop();
    };
  });
}

async function handleCommand(ctx: Context, bridge: DesktopBridgeServer, command: BridgeMessage) {
  switch (command.type) {
    case "shutdown-request":
      ctx.logger.info("[dsh-desktop-host] shutdown requested");
      bridge.send({ type: "log", line: "shutdown acknowledged" });
      // TODO(M3): 触发宿主优雅退出（等价于卸载本插件，框架自动清理所有注册）
      break;
    case "menu-click":
      ctx.logger.info(`[dsh-desktop-host] menu click: ${String(command.id)}`);
      // TODO(M2): 托盘菜单动作分发（显示窗口由壳本地处理，其余走这里）
      break;
    default:
      ctx.logger.warn(`[dsh-desktop-host] unknown command: ${String(command.type)}`);
  }
}
