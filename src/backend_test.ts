import assert from "node:assert";
import { BackendManager } from "./backend.ts";
import type { Config } from "./types.ts";

function makeManager(sticky: boolean): BackendManager {
  const config = {
    backends: [
      { id: "a", baseUrl: "http://127.0.0.1:1/v1", weight: 1 },
      { id: "b", baseUrl: "http://127.0.0.1:2/v1", weight: 1 },
      { id: "c", baseUrl: "http://127.0.0.1:3/v1", weight: 1 },
    ],
    sticky,
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
  } as Config;
  const m = new BackendManager(config);
  const seen = new Set<string>();
  for (let i = 0; i < 40; i++) {
    const chosen = m.pick(undefined);
    seen.add(chosen?.config.id ?? "");
  }
  assert.strictEqual(seen.size, 2);
});
