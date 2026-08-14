/**
 * 生命周期检测：等待 web 服务就绪并返回实际监听地址。
 *
 * cordis 的服务代理会把访问延迟到服务可用之后（源码注释原文：
 * "delays the method call until the declared services are available"），
 * 所以 `await ctx.webServer` 即"等 webServer 服务就绪"；随后读 .port
 * （实际监听端口，config.port 为 0 时是 OS 分配值）。
 *
 * TODO(M1 验证): 接入真实组合后确认 webServer 服务名与字段形状。
 */

import type { Context } from "@deepseek-ai/cordis";

export interface ReadyInfo {
  host: string;
  port: number;
}

export async function detectReady(ctx: Context, timeoutMs = 120_000): Promise<ReadyInfo> {
  const webServer = (ctx as unknown as { webServer?: unknown }).webServer;
  if (webServer === undefined) {
    throw new Error("webServer 服务未提供（组合中缺少 dsh-host-webserver？）");
  }
  const server = (await Promise.race([
    Promise.resolve(webServer),
    new Promise<never>((_, reject) => {
      setTimeout(() => reject(new Error(`webServer 未就绪（${timeoutMs}ms 超时）`)), timeoutMs);
    }),
  ])) as { port?: unknown; host?: unknown } | undefined;

  const port = server?.port;
  if (typeof port !== "number" || port <= 0) {
    throw new Error("webServer 已就绪但端口无效");
  }
  const host = typeof server?.host === "string" && server.host !== "" ? server.host : "127.0.0.1";
  return { host, port };
}
