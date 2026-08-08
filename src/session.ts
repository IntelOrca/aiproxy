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
  body: any,
): Promise<string | undefined> {
  const header = req.headers.get("x-session-id");
  if (header) return "h:" + header;

  if (body && typeof body === "object") {
    if (typeof body.session_id === "string" && body.session_id) {
      return "b:" + body.session_id;
    }
    if (typeof body.thread_id === "string" && body.thread_id) {
      return "b:" + body.thread_id;
    }
    if (Array.isArray(body.messages)) {
      const firstUser = body.messages.find((m: any) => m && m.role === "user");
      if (firstUser) {
        const content = typeof firstUser.content === "string"
          ? firstUser.content
          : JSON.stringify(firstUser.content ?? "");
        const model = typeof body.model === "string" ? body.model : "";
        const hash = await sha256hex(model + "\u0000" + content);
        return "f:" + hash.slice(0, 24);
      }
    }
  }
  return undefined;
}

async function sha256hex(input: string): Promise<string> {
  const data = new TextEncoder().encode(input);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
