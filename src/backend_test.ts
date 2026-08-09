import assert from "node:assert";
import { BackendManager } from "./backend.ts";
import type { Config } from "./types.ts";

// Hermetic persistence dir so tests never touch the real state dir.
const testStateDir = Deno.makeTempDirSync({ prefix: "aiproxy-test-" });

function makeManager(sticky: boolean): BackendManager {
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1 },
      { id: "c", baseUrl: "http://127.0.0.1:3/v1", weight: 1 },
    ],
    sticky,
    sessionTtlMs: 60_000,
    stateDir: testStateDir,
  } as Config;
  return new BackendManager(config);
}

Deno.test("sticky: same session key always lands on same backend", () => {
  const m = makeManager(true);
  const b1 = m.pick("f:key");
  const b2 = m.pick("f:key");
  const b3 = m.pick("f:key");
  assert.ok(b1 && b2 && b3);
  assert.strictEqual(b1.config.id, b2.config.id);
  assert.strictEqual(b2.config.id, b3.config.id);
});

Deno.test("healthy filter: excludes downed backends", () => {
  const m = makeManager(false);
  // bring down b and c
  const states = (m as unknown as { states: { healthy: boolean }[] }).states;
  states[1].healthy = false;
  states[2].healthy = false;
  const chosen = m.pick(undefined);
  assert.ok(chosen);
  assert.strictEqual(chosen.config.id, "a");
});

Deno.test("markDown flips backend to unhealthy", () => {
  const m = makeManager(false);
  m.markDown("a");
  const chosen = m.pick(undefined);
  assert.notStrictEqual(chosen?.config.id, "a");
});

Deno.test("priority: lowest-priority backend chosen when healthy", () => {
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1, priority: 0 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1, priority: 1 },
    ],
    sticky: false,
    stateDir: testStateDir,
  } as Config;
  const m = new BackendManager(config);
  for (let i = 0; i < 20; i++) {
    const chosen = m.pick(undefined);
    assert.strictEqual(chosen?.config.id, "a");
  }
});

Deno.test("priority: falls back to next priority when primary is unhealthy", () => {
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1, priority: 0 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1, priority: 1 },
    ],
    sticky: false,
    stateDir: testStateDir,
  } as Config;
  const m = new BackendManager(config);
  m.markDown("a");
  for (let i = 0; i < 20; i++) {
    const chosen = m.pick(undefined);
    assert.strictEqual(chosen?.config.id, "b");
  }
});

Deno.test("priority: backends without priority rank equal to each other", () => {
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1 },
    ],
    sticky: false,
    stateDir: testStateDir,
  } as Config;
  const m = new BackendManager(config);
  const seen = new Set<string>();
  for (let i = 0; i < 40; i++) {
    const chosen = m.pick(undefined);
    seen.add(chosen?.config.id ?? "");
  }
  assert.strictEqual(seen.size, 2);
});

Deno.test("pins persist across a restart (loads from disk)", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-pins-" });
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1 },
    ],
    sticky: true,
    sessionTtlMs: 60_000,
    stateDir: dir,
  } as Config;

  const m1 = new BackendManager(config);
  m1.pick("f:survive"); // fresh pin, scheduled for persist
  m1.stop(); // flush to disk
  const firstBackend = m1.pick("f:survive")!.config.id;

  // Simulate a restart: a brand-new manager reading the same dir.
  const m2 = new BackendManager(config);
  assert.strictEqual(m2.pick("f:survive")!.config.id, firstBackend);

  Deno.removeSync(dir, { recursive: true });
});

Deno.test("expired pins are dropped on restore", () => {
  const dir = Deno.makeTempDirSync({ prefix: "aiproxy-pins-" });
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1 },
    ],
    sticky: true,
    sessionTtlMs: 60_000,
    stateDir: dir,
  } as Config;

  const m1 = new BackendManager(config);
  m1.pick("f:stale");
  m1.stop();

  // Rewrite the persisted pin with an ancient lastSeen.
  const file = dir + (Deno.build.os === "windows" ? "\\" : "/") + "pins.json";
  const data = JSON.parse(Deno.readTextFileSync(file));
  data.pins["f:stale"].lastSeen = Date.now() - 3_600_000;
  Deno.writeTextFileSync(file, JSON.stringify(data));

  // A fresh manager must not honor the expired pin on restore.
  const m2 = new BackendManager(config);
  const pins = (m2 as unknown as { pins: Map<string, { lastSeen: number }> }).pins;
  assert.strictEqual(pins.has("f:stale"), false, "expired pin must be dropped on restore");

  Deno.removeSync(dir, { recursive: true });
});
