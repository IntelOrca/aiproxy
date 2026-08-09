// Sandbox backend for aiproxy: a minimal OpenAI-compatible fake server used for
// testing without real backends.
//
// - Validates the requested model against an allowed list and returns a 404
//   error for unknown models (matching real OpenAI API behavior).
// - Exposes N example models via GET /models.
// - Responds to streaming (SSE) and non-streaming chat completions.
// - Logs every request to the console and a per-port log file.

export interface SandboxOptions {
  port: number;
  models?: string[];
  logFile?: string;
}

export const DEFAULT_MODELS = [
  "sandbox-gpt-4o",
  "sandbox-claude-sonnet",
  "sandbox-llama-3.1-70b",
];

export function startSandboxServer(opts: SandboxOptions): void {
  const models = opts.models ?? DEFAULT_MODELS;
  const logFile = opts.logFile ?? `aiproxy-sandbox-${opts.port}.log`;

  async function log(s: string) {
    try {
      await Deno.writeTextFile(logFile, s + "\n", { append: true });
    } catch {
      // never let logging break the server
    }
  }

  function error(status: number, message: string): Response {
    return Response.json(
      { error: { message, type: "invalid_request_error", code: status } },
      { status },
    );
  }

  function sseResponse(model: string): Response {
    const encoder = new TextEncoder();
    const created = Math.floor(Date.now() / 1000);
    const body = new ReadableStream({
      start(controller) {
        const chunk = {
          id: "chatcmpl-sandbox",
          object: "chat.completion.chunk",
          created,
          model,
          choices: [{
            index: 0,
            delta: { role: "assistant", content: "sandbox reply" },
            finish_reason: null,
          }],
        };
        controller.enqueue(
          encoder.encode(`data: ${JSON.stringify(chunk)}\n\n`),
        );
        const done = {
          id: "chatcmpl-sandbox",
          object: "chat.completion.chunk",
          created,
          model,
          choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
        };
        controller.enqueue(encoder.encode(`data: ${JSON.stringify(done)}\n\n`));
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      },
    });
    return new Response(body, {
      headers: {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        "Connection": "keep-alive",
      },
    });
  }

  Deno.serve({ port: opts.port, hostname: "127.0.0.1" }, async (req) => {
    const url = new URL(req.url);
    let bodyText = "";
    try {
      bodyText = await req.text();
    } catch {
      // ignore
    }
    const sizeKb = (bodyText.length / 1024).toFixed(1);
    console.log(
      `[sandbox:${opts.port}] ${req.method} ${url.pathname}${url.search} (${sizeKb} KB)`,
    );
    await log(
      `[${
        new Date().toISOString()
      }] ${req.method} ${url.pathname}${url.search} (${sizeKb} KB)`,
    );

    if (req.method === "OPTIONS") return new Response(null, { status: 204 });

    if (req.method === "GET" && url.pathname.endsWith("/models")) {
      return Response.json({
        object: "list",
        data: models.map((id) => ({
          id,
          object: "model",
          owned_by: "sandbox",
        })),
      });
    }

    if (!url.pathname.endsWith("/chat/completions")) {
      return error(404, "not found");
    }

    let parsed: Record<string, unknown> | null = null;
    try {
      const json: unknown = bodyText ? JSON.parse(bodyText) : null;
      if (json && typeof json === "object" && !Array.isArray(json)) {
        parsed = json as Record<string, unknown>;
      }
    } catch {
      return error(400, "invalid JSON body");
    }

    const model = parsed?.model;
    if (typeof model !== "string" || !model) return error(400, "missing model");
    if (!models.includes(model)) {
      return error(
        404,
        `The model '${model}' does not exist or you do not have access to it.`,
      );
    }

    if (parsed?.stream === false) {
      return Response.json({
        id: "chatcmpl-sandbox",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model,
        choices: [{
          index: 0,
          message: { role: "assistant", content: "sandbox reply" },
          finish_reason: "stop",
        }],
      });
    }
    return sseResponse(model);
  });
}

if (import.meta.main) {
  const port = Number(Deno.env.get("PORT") ?? 6060);
  const models = (Deno.env.get("MODELS") ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  startSandboxServer({ port, models: models.length ? models : DEFAULT_MODELS });
}
