// All-in-one sandbox: starts 3 fake backends plus a router wired to them, so
// you can watch load balancing, sticky sessions, and the dashboard without any
// real backends.
//
//   PORT  router port (default 6060); fake backends listen on PORT+1..+3

import { startAiproxy } from "./app.ts";
import { startSandboxServer, DEFAULT_MODELS } from "./sandbox-server.ts";
import type { Config } from "./types.ts";

const port = Number(Deno.env.get("PORT") ?? 6060);
const backendPorts = [port + 1, port + 2, port + 3];

for (const p of backendPorts) {
  startSandboxServer({ port: p, models: DEFAULT_MODELS });
}

const config: Config = {
  port,
  backends: backendPorts.map((p, i) => ({
    id: `sandbox-${i + 1}`,
    baseUrl: `http://127.0.0.1:${p}/v1`,
    weight: 1,
  })),
  sticky: true,
  healthCheckIntervalMs: 5000,
  healthCheckTimeoutMs: 2000,
  sessionTtlMs: 30 * 60_000,
  upstreamTimeoutMs: 120_000,
  maxRetries: 2,
  models: DEFAULT_MODELS,
};

console.log(
  `[aiproxy-sandbox] fake backends on ${backendPorts.map((p) => `127.0.0.1:${p}`).join(", ")}`,
);
startAiproxy(config);
