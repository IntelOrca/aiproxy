import assert from "node:assert";
import { loadStats, saveStats } from "./stats-store.ts";
import { loadConfig } from "./config.ts";
import type { RoutingEntry } from "./types.ts";

Deno.test("stats round-trips through disk", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-stats-" });
  const counts = { "sandbox-1": 5, "sandbox-2": 3 };
  const recent: RoutingEntry[] = [
    {
      at: "2026-08-09T12:00:00.000Z",
      sessionId: "f:abc123",
      model: "sandbox-gpt-4o",
      backendId: "sandbox-1",
      endpoint: "http://127.0.0.1:6061/v1",
      status: 200,
      ms: 42,
    },
  ];

  saveStats(dir, counts, recent);
  const loaded = loadStats(dir);

  assert.deepStrictEqual(loaded.backendCounts, counts);
  assert.strictEqual(loaded.recent.length, 1);
  assert.deepStrictEqual(loaded.recent[0], recent[0]);

  Deno.removeSync(dir, { recursive: true });
});

Deno.test("loadStats returns empty for missing or malformed files", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-stats-" });
  const empty = loadStats(dir);
  assert.deepStrictEqual(empty, { backendCounts: {}, recent: [] });

  // Malformed JSON -> empty.
  Deno.mkdirSync(dir, { recursive: true });
  Deno.writeTextFileSync(
    dir + (Deno.build.os === "windows" ? "\\" : "/") + "stats.json",
    "not json",
  );
  const malformed = loadStats(dir);
  assert.deepStrictEqual(malformed, { backendCounts: {}, recent: [] });

  Deno.removeSync(dir, { recursive: true });
});

Deno.test("loadStats drops invalid entries", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-stats-" });
  const bad = {
    version: 1,
    backendCounts: { "x": "oops" },
    recent: [{ nope: true }],
  };
  const file = dir + (Deno.build.os === "windows" ? "\\" : "/") + "stats.json";
  Deno.writeTextFileSync(file, JSON.stringify(bad));

  const loaded = loadStats(dir);
  assert.deepStrictEqual(loaded.backendCounts, {});
  assert.strictEqual(loaded.recent.length, 0);

  Deno.removeSync(dir, { recursive: true });
});

Deno.test("config recentRoutes defaults to 200 and honors explicit value", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-cfg-" });
  const sep = Deno.build.os === "windows" ? "\\" : "/";

  const plain = dir + sep + "plain.json";
  Deno.writeTextFileSync(plain, JSON.stringify({ backends: [] }));
  assert.strictEqual(loadConfig(plain).recentRoutes, 200);

  const custom = dir + sep + "custom.json";
  Deno.writeTextFileSync(
    custom,
    JSON.stringify({ recentRoutes: 10, backends: [] }),
  );
  assert.strictEqual(loadConfig(custom).recentRoutes, 10);

  Deno.removeSync(dir, { recursive: true });
});
