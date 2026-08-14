/**
 * 独立桥服务端（供跨语言冒烟：Rust 客户端连接本服务端）。
 * 用法：node --experimental-strip-types test/pipe-server.ts <token>
 * 行为：收到 {"type":"ping"} → 回 state+notification；收到 {"type":"quit"} → 回 bye 并退出。
 */

import { writeFileSync } from "node:fs";
import { DesktopBridgeServer, type BridgeMessage } from "../src/bridge.ts";

const token = process.argv[2] ?? "cross-smoke";
const server = new DesktopBridgeServer(token, (command: BridgeMessage) => {
  console.log("server got:", JSON.stringify(command));
  if (command.type === "ping") {
    server.send({ type: "state", phase: "ready", host: "127.0.0.1", port: 3080, detail: "cross-language smoke" });
    server.send({ type: "notification", title: "已就绪", body: "http://127.0.0.1:3080", level: "info" });
  } else if (command.type === "quit") {
    server.send({ type: "bye" });
    void server.stop().then(() => process.exit(0));
  }
});

const endpoint = await server.start();
console.log("READY", endpoint);
// 就绪信号写入文件（避免外层 PowerShell 管道缓冲导致轮询读不到）
writeFileSync("pipe-ready.txt", endpoint, "utf8");
// 30s 兜底退出，防止冒烟脚本挂了服务端不退出
setTimeout(() => {
  console.log("server timeout exit");
  process.exit(2);
}, 30_000);
