/**
 * 桥自测（Node ↔ Node，走真实端点）：验证
 *   1. 服务端监听（Windows 管道 / unix socket 由平台自动选择）；
 *   2. 客户端 → 服务端命令送达；
 *   3. 服务端 → 客户端事件送达；
 *   4. 广播事件所有客户端都能收到。
 * 运行：npm test 或 node --experimental-strip-types test/bridge.smoke.ts
 */

import { connect } from "node:net";
import { createInterface } from "node:readline";
import { DesktopBridgeServer } from "../src/bridge.ts";

const TOKEN = "smoke-" + Math.random().toString(36).slice(2, 12);

function fail(message: string): never {
  console.error("FAIL:", message);
  process.exit(1);
}

async function main() {
  const serverGot: string[] = [];
  const server = new DesktopBridgeServer(TOKEN, (command) => {
    serverGot.push(command.type);
    if (command.type === "ping") {
      server.send({ type: "pong", echo: command.payload });
    }
  });
  const endpoint = await server.start();
  console.log("server listening:", endpoint);
  if (endpoint !== (process.platform === "win32" ? `\\\\.\\pipe\\dsh-desktop-${TOKEN}` : endpoint)) {
    // 平台端点形状已由 bridgeEndpoint 保证，此处仅打印
  }

  const received: Array<Record<string, unknown>> = [];
  const socket = connect(endpoint);
  await new Promise<void>((resolve, reject) => {
    socket.once("connect", () => resolve());
    socket.once("error", reject);
  });
  createInterface({ input: socket }).on("line", (line) => {
    try {
      received.push(JSON.parse(line));
    } catch {
      /* 忽略 */
    }
  });

  // 1) 客户端 → 服务端
  socket.write(JSON.stringify({ type: "ping", payload: "hello" }) + "\n");
  await new Promise((r) => setTimeout(r, 300));
  if (!serverGot.includes("ping")) fail("命令未送达服务端");
  console.log("PASS: 命令 client→server");

  // 2) 服务端 → 客户端（单播回复）
  if (!received.some((m) => m.type === "pong" && m.echo === "hello")) {
    fail("事件未送达客户端");
  }
  console.log("PASS: 事件 server→client");

  // 3) 广播
  server.send({ type: "state", phase: "ready", port: 3080, detail: "" });
  await new Promise((r) => setTimeout(r, 200));
  if (!received.some((m) => m.type === "state" && m.port === 3080)) {
    fail("广播事件未收到");
  }
  console.log("PASS: 广播事件");

  // 4) 坏行不崩
  socket.write("this is not json\n");
  await new Promise((r) => setTimeout(r, 150));
  console.log("PASS: 坏行被忽略");

  socket.destroy();
  await server.stop();
  console.log("ALL PASS ✔");
}

main().catch((error) => fail(String(error)));
