import assert from "node:assert";
import { sessionKeyFromBody } from "./session.ts";

Deno.test("same conversation -> same session key across turns", async () => {
  const req = () => new Request("http://router/v1/chat/completions", { method: "POST" });
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
  const req = () => new Request("http://router/v1/chat/completions", { method: "POST" });
  const k1 = await sessionKeyFromBody(req(), { messages: [{ role: "user", content: "aaa" }] });
  const k2 = await sessionKeyFromBody(req(), { messages: [{ role: "user", content: "bbb" }] });
  assert.notStrictEqual(k1, k2);
});

Deno.test("x-session-id header wins over fingerprint", async () => {
  const req = new Request("http://router/v1/chat/completions", {
    method: "POST",
    headers: { "x-session-id": "my-session" },
  });
  const key = await sessionKeyFromBody(req, { messages: [{ role: "user", content: "hello" }] });
  assert.strictEqual(key, "h:my-session");
});

Deno.test("session_id in body is used", async () => {
  const req = new Request("http://router/v1/chat/completions", { method: "POST" });
  const key = await sessionKeyFromBody(req, { session_id: "abc", messages: [] });
  assert.strictEqual(key, "b:abc");
});

Deno.test("empty body -> no session key", async () => {
  const req = new Request("http://router/v1/chat/completions", { method: "POST" });
  const key = await sessionKeyFromBody(req, { messages: [] });
  assert.strictEqual(key, undefined);
});
