/**
 * Derive a session key from an incoming request so the load balancer can route
 * consecutive turns of the same conversation to the same backend.
 *
 * GitHub Copilot CLI (and most OpenAI-compatible clients) send no session ID.
 * They resend the full message history on every request, so we fingerprint the
 * conversation: the first user message is stable across all turns of a session
 * and distinct between sessions.
 *
 * Precedence:
 *   1. `X-Session-ID` header (explicit client-provided session)
 *   2. `session_id` / `thread_id` field in the request body
 *   3. hash(model + first user message content)
 */
export async function sessionKeyFromBody(
  req: Request,
  body: unknown,
): Promise<string | undefined> {
  const header = req.headers.get("x-session-id");
  if (header) return "h:" + header;

  if (body && typeof body === "object" && !Array.isArray(body)) {
    const obj = body as Record<string, unknown>;
    if (typeof obj.session_id === "string" && obj.session_id) {
      return "b:" + obj.session_id;
    }
    if (typeof obj.thread_id === "string" && obj.thread_id) {
      return "b:" + obj.thread_id;
    }
    if (Array.isArray(obj.messages)) {
      const firstUser = obj.messages.find(
        (m: unknown): m is Record<string, unknown> =>
          !!m && typeof m === "object" && !Array.isArray(m) &&
          (m as Record<string, unknown>).role === "user",
      );
      if (firstUser) {
        const content = typeof firstUser.content === "string"
          ? firstUser.content
          : JSON.stringify(firstUser.content ?? "");
        const model = typeof obj.model === "string" ? obj.model : "";
        const hash = await sha256hex(model + "\u0000" + content);
        return "f:" + hash.slice(0, 24);
      }
    }
  }
  return undefined;
}

/**
 * Normalize an internal session key (`h:…` / `b:…` / `f:…`) into a value
 * safe to send upstream as a conversation-ID header.
 *
 * Strips the internal single-char prefix, trims whitespace, passes values
 * through unchanged when they fit OpenRouter's 256-char limit, and falls
 * back to a deterministic SHA-256 hex digest when overlong. Returns
 * undefined when there is no usable value (no session key).
 */
export async function upstreamSessionValue(
  sessionKey: string | undefined,
): Promise<string | undefined> {
  if (!sessionKey) return undefined;
  const prefixed = /^[hbf]:(.*)$/.exec(sessionKey);
  const value = (prefixed ? prefixed[1] : sessionKey).trim();
  if (!value) return undefined;
  if (value.length <= 256) return value;
  return await sha256hex(value);
}

async function sha256hex(input: string): Promise<string> {
  const data = new TextEncoder().encode(input);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}
