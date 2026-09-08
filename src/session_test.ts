import assert from "node:assert";
import { sessionKeyFromBody, upstreamSessionValue } from "./session.ts";

Deno.test("same conversation -> same session key across turns", async () => {
  const req = () =>
    new Request("http://router/v1/chat/completions", { method: "POST" });
  const turn1 = await sessionKeyFromBody(req(), {
    model: "m",
    messages: [
      { role: "system", content: "you are a bot" },
      { role: "user", content: "hello" },
    ],
  });
  const turn2 = await sessionKeyFromBody(req(), {
    model: "m",
    messages: [
      { role: "system", content: "you are a bot" },
      { role: "user", content: "hello" },
      { role: "assistant", content: "hi" },
      { role: "user", content: "say more" },
    ],
  });
  assert.ok(turn1 && turn2);
  assert.strictEqual(turn1, turn2);
});

Deno.test("different first user message -> different session key", async () => {
  const req = () =>
    new Request("http://router/v1/chat/completions", { method: "POST" });
  const k1 = await sessionKeyFromBody(req(), {
    messages: [{ role: "user", content: "aaa" }],
  });
  const k2 = await sessionKeyFromBody(req(), {
    messages: [{ role: "user", content: "bbb" }],
  });
  assert.notStrictEqual(k1, k2);
});

Deno.test("x-session-id header wins over fingerprint", async () => {
  const req = new Request("http://router/v1/chat/completions", {
    method: "POST",
    headers: { "x-session-id": "my-session" },
  });
  const key = await sessionKeyFromBody(req, {
    messages: [{ role: "user", content: "hello" }],
  });
  assert.strictEqual(key, "h:my-session");
});

Deno.test("session_id in body is used", async () => {
  const req = new Request("http://router/v1/chat/completions", {
    method: "POST",
  });
  const key = await sessionKeyFromBody(req, {
    session_id: "abc",
    messages: [],
  });
  assert.strictEqual(key, "b:abc");
});

Deno.test("empty body -> no session key", async () => {
  const req = new Request("http://router/v1/chat/completions", {
    method: "POST",
  });
  const key = await sessionKeyFromBody(req, { messages: [] });
  assert.strictEqual(key, undefined);
});

Deno.test("upstreamSessionValue strips internal prefixes", async () => {
  assert.strictEqual(await upstreamSessionValue("h:my-session"), "my-session");
  assert.strictEqual(await upstreamSessionValue("b:abc"), "abc");
  assert.strictEqual(await upstreamSessionValue("f:abc123"), "abc123");
  // Raw values that merely contain a colon are left alone.
  assert.strictEqual(await upstreamSessionValue("ab:cd"), "ab:cd");
  assert.strictEqual(await upstreamSessionValue("x:yz"), "x:yz");
});

Deno.test("upstreamSessionValue returns undefined without a key", async () => {
  assert.strictEqual(await upstreamSessionValue(undefined), undefined);
  assert.strictEqual(await upstreamSessionValue("h:  "), undefined);
});

Deno.test("upstreamSessionValue hashes overlong values deterministically", async () => {
  const long = "x".repeat(300);
  const v1 = await upstreamSessionValue("h:" + long);
  const v2 = await upstreamSessionValue("h:" + long);
  assert.ok(v1 && v2);
  assert.strictEqual(v1, v2);
  assert.strictEqual(v1.length, 64);
  assert.match(v1, /^[0-9a-f]{64}$/);
  // Boundary: exactly 256 chars passes through unhashed.
  assert.strictEqual(
    (await upstreamSessionValue("b:" + "y".repeat(256)))?.length,
    256,
  );
});
