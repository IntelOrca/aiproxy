import assert from "node:assert";
import { BackendManager } from "./backend.ts";
import { forwardWithRetry } from "./upstream.ts";
import type { Config } from "./types.ts";

interface TestServer {
  port: number;
  close: () => Promise<void>;
}

function startTestServer(handler: (req: Request) => Response): TestServer {
  const server = Deno.serve({ port: 0, hostname: "127.0.0.1" }, handler);
  const port = (server.addr as Deno.NetAddr).port;
  return { port, close: () => server.shutdown() };
}

function makeConfig(
  primaryPort: number,
  backupPort: number,
  overrides: Partial<Config> = {},
): Config {
  return {
    port: 0,
    backends: [
      {
        id: "primary",
        baseUrl: `http://127.0.0.1:${primaryPort}/v1`,
        weight: 1,
        priority: 0,
      },
      {
        id: "backup",
        baseUrl: `http://127.0.0.1:${backupPort}/v1`,
        weight: 1,
        priority: 1,
      },
    ],
    sticky: true,
    healthCheckIntervalMs: 60_000,
    healthCheckTimeoutMs: 1_000,
    sessionTtlMs: 60_000,
    upstreamTimeoutMs: 5_000,
    maxRetries: 1,
    stateDir: Deno.makeTempDirSync({ prefix: "aiproxy-upstream-" }),
    ...overrides,
  };
}

function chatRequest(port: number): Request {
  return new Request(`http://127.0.0.1:${port}/v1/chat/completions`, {
    method: "POST",
    body: JSON.stringify({
      model: "gpt-4o",
      messages: [{ role: "user", content: "hi" }],
    }),
    headers: { "content-type": "application/json" },
  });
}

Deno.test("rate-limit 429 fails over to backup without marking primary down", async () => {
  let primaryHits = 0;
  let backupHits = 0;
  const primary = startTestServer(() => {
    primaryHits++;
    return Response.json({ error: { message: "rate limit" } }, { status: 429 });
  });
  const backup = startTestServer(() => {
    backupHits++;
    return Response.json({ ok: true }, { status: 200 });
  });

  const config = makeConfig(primary.port, backup.port);
  const manager = new BackendManager(config);
  const body = { model: "gpt-4o", messages: [{ role: "user", content: "hi" }] };
  const result = await forwardWithRetry(
    chatRequest(primary.port),
    body,
    "f:abc",
    manager,
    config,
  );

  assert.strictEqual(result.backendId, "backup");
  assert.strictEqual(result.response.status, 200);
  assert.strictEqual(primaryHits, 1);
  assert.strictEqual(backupHits, 1);
  // A rate limit is not "down" — the primary stays healthy and pinned.
  assert.ok(manager.healthReport().find((b) => b.id === "primary")?.healthy);

  manager.stop();
  await primary.close();
  await backup.close();
});

Deno.test("rate-limit failover re-pins the session to the backend that served", async () => {
  let primaryHits = 0;
  let backupHits = 0;
  const primary = startTestServer(() => {
    primaryHits++;
    return Response.json({ error: { message: "rate limit" } }, { status: 429 });
  });
  const backup = startTestServer(() => {
    backupHits++;
    return Response.json({ ok: true }, { status: 200 });
  });

  const config = makeConfig(primary.port, backup.port);
  const manager = new BackendManager(config);
  const body = {
    model: "gpt-4o",
    messages: [{ role: "user", content: "turn" }],
  };

  // Turn 1: primary 429 -> fail over to backup.
  const r1 = await forwardWithRetry(
    chatRequest(primary.port),
    body,
    "f:keep",
    manager,
    config,
  );
  assert.strictEqual(r1.backendId, "backup");
  assert.strictEqual(primaryHits, 1);
  assert.strictEqual(backupHits, 1);

  // Turn 2: same session is now pinned to the backup (cache affinity) — the
  // primary must NOT be re-tried; the warm cache on the backup is reused.
  const r2 = await forwardWithRetry(
    chatRequest(primary.port),
    body,
    "f:keep",
    manager,
    config,
  );
  assert.strictEqual(r2.backendId, "backup");
  assert.strictEqual(
    primaryHits,
    1,
    "primary must not be retried once the session was pinned away",
  );
  assert.strictEqual(backupHits, 2);

  manager.stop();
  await primary.close();
  await backup.close();
});

Deno.test("rate-limit message in error body triggers failover", async () => {
  const primary = startTestServer(() =>
    Response.json({ error: { message: "you have reached your limit" } }, {
      status: 400,
    })
  );
  const backup = startTestServer(() =>
    Response.json({ ok: true }, { status: 200 })
  );

  const config = makeConfig(primary.port, backup.port);
  const manager = new BackendManager(config);
  const result = await forwardWithRetry(
    chatRequest(primary.port),
    { model: "gpt-4o", messages: [] },
    "f:x",
    manager,
    config,
  );

  assert.strictEqual(result.backendId, "backup");
  assert.strictEqual(result.response.status, 200);

  manager.stop();
  await primary.close();
  await backup.close();
});

Deno.test("new sessions still try the primary first, then pin to the serving fallback", async () => {
  let primaryHits = 0;
  let backupHits = 0;
  const primary = startTestServer(() => {
    primaryHits++;
    return Response.json({ error: { message: "rate limit" } }, { status: 429 });
  });
  const backup = startTestServer(() => {
    backupHits++;
    return Response.json({ ok: true }, { status: 200 });
  });

  const config = makeConfig(primary.port, backup.port);
  const manager = new BackendManager(config);
  const mkBody = (m: string) => ({
    model: "gpt-4o",
    messages: [{ role: "user", content: m }],
  });

  // Session A rate-limits on primary and gets pinned to backup.
  await forwardWithRetry(
    chatRequest(primary.port),
    mkBody("session a"),
    "f:a",
    manager,
    config,
  );
  await forwardWithRetry(
    chatRequest(primary.port),
    mkBody("session a"),
    "f:a",
    manager,
    config,
  );
  assert.strictEqual(
    primaryHits,
    1,
    "session a stays on backup after failover",
  );

  // Session B (new) still tries the primary first (priority 0), then fails
  // over and pins to the backup too.
  await forwardWithRetry(
    chatRequest(primary.port),
    mkBody("session b"),
    "f:b",
    manager,
    config,
  );
  assert.strictEqual(
    primaryHits,
    2,
    "a fresh session still tries the primary first",
  );
  assert.strictEqual(backupHits, 3); // A turn1 failover, A turn2 direct, B failover

  manager.stop();
  await primary.close();
  await backup.close();
});

Deno.test("healthy primary backend is used first", async () => {
  let primaryHits = 0;
  const primary = startTestServer(() => {
    primaryHits++;
    return Response.json({ ok: true }, { status: 200 });
  });
  const backup = startTestServer(() =>
    Response.json({ ok: true }, { status: 200 })
  );

  const config = makeConfig(primary.port, backup.port);
  const manager = new BackendManager(config);
  const result = await forwardWithRetry(
    chatRequest(primary.port),
    { model: "gpt-4o", messages: [] },
    "f:y",
    manager,
    config,
  );

  assert.strictEqual(result.backendId, "primary");
  assert.strictEqual(primaryHits, 1);

  manager.stop();
  await primary.close();
  await backup.close();
});

Deno.test("backend apiKey is sent as Bearer to the backend", async () => {
  let auth: string | null = null;
  const backend = startTestServer((req) => {
    auth = req.headers.get("authorization");
    return Response.json({ ok: true }, { status: 200 });
  });

  const config = {
    port: 0,
    backends: [
      {
        id: "b",
        baseUrl: `http://127.0.0.1:${backend.port}/v1`,
        weight: 1,
        apiKey: "sekret",
      },
    ],
    sticky: false,
    healthCheckIntervalMs: 60_000,
    healthCheckTimeoutMs: 1_000,
    sessionTtlMs: 60_000,
    upstreamTimeoutMs: 5_000,
    maxRetries: 0,
    stateDir: Deno.makeTempDirSync({ prefix: "aiproxy-api-" }),
  } as Config;
  const manager = new BackendManager(config);

  const result = await forwardWithRetry(
    chatRequest(backend.port),
    { model: "gpt-4o", messages: [] },
    undefined,
    manager,
    config,
  );

  assert.strictEqual(result.response.status, 200);
  assert.strictEqual(auth, "Bearer sekret");

  manager.stop();
  await backend.close();
});

Deno.test("client auth key is stripped upstream when router apiKeys are set", async () => {
  let auth: string | null = "unset";
  const backend = startTestServer((req) => {
    auth = req.headers.get("authorization");
    return Response.json({ ok: true }, { status: 200 });
  });

  const config = {
    port: 0,
    backends: [{
      id: "b",
      baseUrl: `http://127.0.0.1:${backend.port}/v1`,
      weight: 1,
    }],
    sticky: false,
    healthCheckIntervalMs: 60_000,
    healthCheckTimeoutMs: 1_000,
    sessionTtlMs: 60_000,
    upstreamTimeoutMs: 5_000,
    maxRetries: 0,
    apiKeys: ["router-secret"],
    stateDir: Deno.makeTempDirSync({ prefix: "aiproxy-api-" }),
  } as Config;
  const manager = new BackendManager(config);

  const req = chatRequest(backend.port);
  req.headers.set("authorization", "Bearer router-secret");
  const result = await forwardWithRetry(
    req,
    { model: "gpt-4o", messages: [] },
    undefined,
    manager,
    config,
  );

  assert.strictEqual(result.response.status, 200);
  assert.strictEqual(auth, null);

  manager.stop();
  await backend.close();
});
