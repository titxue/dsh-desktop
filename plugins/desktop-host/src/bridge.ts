/**
 * dsh-desktop 桥 — 通用传输层（Node/插件侧）。
 *
 * 一份代码跨平台：Windows 走命名管道（\\.\pipe\...），macOS/Linux 走
 * unix socket（AF_UNIX）。node:net 原生统一两者，仅端点路径不同。
 *
 * 消息协议：新行分隔 JSON（\n 结尾），双向同构——
 *   插件 → 壳：progress / state / menu / notification / log
 *   壳 → 插件：menu-click / nav-result / window-event / shutdown-request
 *
 * 认证：token 拼进端点名本身（猜中名字即认证），无 header、无端口可扫。
 */

import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { chmodSync, existsSync, rmSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";

/** 桥消息：所有消息都带 type，其余字段随类型变化。 */
export type BridgeMessage = { type: string } & Record<string, unknown>;

/** 当前平台的桥端点：Windows 命名管道 / POSIX unix socket。 */
export function bridgeEndpoint(token: string): string {
  if (process.platform === "win32") {
    return `\\\\.\\pipe\\dsh-desktop-${token}`;
  }
  // AF_UNIX；tmpdir 路径短于 108 字节限制
  return join(tmpdir(), `dsh-desktop-${token}.sock`);
}

/**
 * 桥服务端（插件侧）。监听端点，把壳发来的命令逐行解析后交给 onCommand；
 * send() 把事件广播给所有已连接的壳。
 */
export class DesktopBridgeServer {
  #token: string;
  #onCommand: (message: BridgeMessage) => void;
  #onClientConnect: (socket: Socket) => void;
  #server: Server | null = null;
  #clients = new Set<Socket>();
  #stopped = false;

  constructor(
    token: string,
    onCommand: (message: BridgeMessage) => void,
    onClientConnect: (socket: Socket) => void = () => {},
  ) {
    this.#token = token;
    this.#onCommand = onCommand;
    this.#onClientConnect = onClientConnect;
  }

  get endpoint(): string {
    return bridgeEndpoint(this.#token);
  }

  /** 启动监听，返回实际端点。POSIX 下清理残留 socket 文件并 chmod 0600。 */
  start(): Promise<string> {
    const endpoint = this.endpoint;
    if (process.platform !== "win32" && existsSync(endpoint)) {
      rmSync(endpoint, { force: true }); // 上次异常退出的残留
    }
    return new Promise((resolve, reject) => {
      const server = createServer((socket) => this.#attach(socket));
      server.once("error", reject);
      server.listen(endpoint, () => {
        if (process.platform !== "win32") {
          try {
            chmodSync(endpoint, 0o600);
          } catch {
            /* 尽力而为 */
          }
        }
        this.#server = server;
        resolve(endpoint);
      });
    });
  }

  /** 广播一条事件给所有已连接的壳（新行分隔 JSON）。 */
  send(message: BridgeMessage): void {
    const line = JSON.stringify(message) + "\n";
    for (const socket of this.#clients) {
      socket.write(line);
    }
  }

  /** 停止监听、断开所有客户端；POSIX 下删除 socket 文件。 */
  async stop(): Promise<void> {
    if (this.#stopped) return;
    this.#stopped = true;
    for (const socket of this.#clients) {
      socket.destroy();
    }
    this.#clients.clear();
    const server = this.#server;
    this.#server = null;
    if (server) {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
    if (process.platform !== "win32") {
      try {
        rmSync(this.endpoint, { force: true });
      } catch {
        /* 尽力而为 */
      }
    }
  }

  #attach(socket: Socket): void {
    this.#clients.add(socket);
    this.#onClientConnect(socket); // 重放钩子：壳重连后立即恢复状态
    socket.on("close", () => this.#clients.delete(socket));
    socket.on("error", () => this.#clients.delete(socket));
    createInterface({ input: socket }).on("line", (line) => {
      try {
        this.#onCommand(JSON.parse(line) as BridgeMessage);
      } catch {
        // 忽略坏行：协议约定每行一个完整 JSON
      }
    });
  }
}
